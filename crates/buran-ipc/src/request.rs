//! Flat request encoding: a fixed offset table followed by one blob.
//!
//! The router builds this once while parsing HTTP; the worker reads fields
//! by offset without copying or re-parsing (`RequestView` borrows the
//! payload). Layout of the table (offsets are relative to blob start):
//!
//! ```text
//! 0   u32 blob_off          (= table end, from payload start)
//! 4   (off u32, len u32) method
//! 12  (off u32, len u32) path        — decoded, normalized
//! 20  (off u32, len u32) target      — original request target
//! 28  (off u32, len u32) query
//! 36  (off u32, len u32) version
//! 44  (off u32, len u32) remote_addr
//! 52  (off u32, len u32) server_name
//! 60  u64 content_length
//! 68  u32 app_target       (reserved for multi-target apps, 0 for now)
//! 72  u32 server_port      (local port of the accepted connection)
//! 76  u32 fields_count
//! 80  fields_count * (name_off u32, name_len u32, val_off u32, val_len u32)
//! ..  (off u32, len u32) preread body
//! ```

use crate::BwpError;

const TABLE_FIXED: usize = 80;
const FIELD_ENTRY: usize = 16;
const BODY_ENTRY: usize = 8;

#[derive(Debug, Clone, Copy)]
pub struct FieldView<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
}

/// Borrowed view over an encoded request payload.
pub struct RequestView<'a> {
    payload: &'a [u8],
    blob_off: usize,
    fields_count: usize,
}

impl<'a> RequestView<'a> {
    pub fn parse(payload: &'a [u8]) -> Result<Self, BwpError> {
        if payload.len() < TABLE_FIXED {
            return Err(BwpError::Truncated { expected: TABLE_FIXED, actual: payload.len() });
        }
        let blob_off = read_u32(payload, 0) as usize;
        let fields_count = read_u32(payload, 76) as usize;
        let table_len = TABLE_FIXED + fields_count * FIELD_ENTRY + BODY_ENTRY;
        if payload.len() < table_len || blob_off < table_len || blob_off > payload.len() {
            return Err(BwpError::Truncated { expected: table_len, actual: payload.len() });
        }
        Ok(Self { payload, blob_off, fields_count })
    }

    pub fn method(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(4)
    }

    pub fn path(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(12)
    }

    pub fn target(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(20)
    }

    pub fn query(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(28)
    }

    pub fn version(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(36)
    }

    pub fn remote_addr(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(44)
    }

    pub fn server_name(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(52)
    }

    pub fn content_length(&self) -> u64 {
        u64::from_le_bytes(self.payload[60..68].try_into().unwrap())
    }

    pub fn server_port(&self) -> u16 {
        read_u32(self.payload, 72) as u16
    }

    pub fn fields_count(&self) -> usize {
        self.fields_count
    }

    pub fn field(&self, i: usize) -> Result<FieldView<'a>, BwpError> {
        debug_assert!(i < self.fields_count);
        let base = TABLE_FIXED + i * FIELD_ENTRY;
        Ok(FieldView { name: self.slice_at(base)?, value: self.slice_at(base + 8)? })
    }

    pub fn fields(&self) -> impl Iterator<Item = Result<FieldView<'a>, BwpError>> + '_ {
        (0..self.fields_count).map(|i| self.field(i))
    }

    pub fn preread_body(&self) -> Result<&'a [u8], BwpError> {
        self.slice_at(TABLE_FIXED + self.fields_count * FIELD_ENTRY)
    }

    fn slice_at(&self, table_pos: usize) -> Result<&'a [u8], BwpError> {
        let off = read_u32(self.payload, table_pos) as usize;
        let len = read_u32(self.payload, table_pos + 4) as usize;
        let start = self.blob_off + off;
        let end = start.checked_add(len).ok_or(BwpError::OutOfBounds {
            off,
            len,
            blob: self.payload.len(),
        })?;
        if end > self.payload.len() {
            return Err(BwpError::OutOfBounds { off, len, blob: self.payload.len() });
        }
        Ok(&self.payload[start..end])
    }
}

/// Builds an encoded request payload. Two-pass free: strings are appended to
/// the blob as they are set; the table is fixed-size per field count.
pub struct RequestBuilder {
    table: Vec<u8>,
    blob: Vec<u8>,
    fields: Vec<(u32, u32, u32, u32)>,
    method: (u32, u32),
    path: (u32, u32),
    target: (u32, u32),
    query: (u32, u32),
    version: (u32, u32),
    remote_addr: (u32, u32),
    server_name: (u32, u32),
    content_length: u64,
    server_port: u16,
    body: (u32, u32),
}

impl Default for RequestBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestBuilder {
    pub fn new() -> Self {
        Self {
            table: Vec::new(),
            blob: Vec::with_capacity(1024),
            fields: Vec::with_capacity(24),
            method: (0, 0),
            path: (0, 0),
            target: (0, 0),
            query: (0, 0),
            version: (0, 0),
            remote_addr: (0, 0),
            server_name: (0, 0),
            content_length: 0,
            server_port: 0,
            body: (0, 0),
        }
    }

    fn append(&mut self, data: &[u8]) -> (u32, u32) {
        let off = self.blob.len() as u32;
        self.blob.extend_from_slice(data);
        (off, data.len() as u32)
    }

    pub fn method(&mut self, v: &[u8]) -> &mut Self {
        self.method = self.append(v);
        self
    }

    pub fn path(&mut self, v: &[u8]) -> &mut Self {
        self.path = self.append(v);
        self
    }

    pub fn target(&mut self, v: &[u8]) -> &mut Self {
        self.target = self.append(v);
        self
    }

    pub fn query(&mut self, v: &[u8]) -> &mut Self {
        self.query = self.append(v);
        self
    }

    pub fn version(&mut self, v: &[u8]) -> &mut Self {
        self.version = self.append(v);
        self
    }

    pub fn remote_addr(&mut self, v: &[u8]) -> &mut Self {
        self.remote_addr = self.append(v);
        self
    }

    pub fn server_name(&mut self, v: &[u8]) -> &mut Self {
        self.server_name = self.append(v);
        self
    }

    pub fn content_length(&mut self, v: u64) -> &mut Self {
        self.content_length = v;
        self
    }

    pub fn server_port(&mut self, v: u16) -> &mut Self {
        self.server_port = v;
        self
    }

    pub fn field(&mut self, name: &[u8], value: &[u8]) -> &mut Self {
        let n = self.append(name);
        let v = self.append(value);
        self.fields.push((n.0, n.1, v.0, v.1));
        self
    }

    pub fn preread_body(&mut self, v: &[u8]) -> &mut Self {
        self.body = self.append(v);
        self
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let table_len = TABLE_FIXED + self.fields.len() * FIELD_ENTRY + BODY_ENTRY;
        self.table.clear();
        self.table.reserve(table_len + self.blob.len());

        let t = &mut self.table;
        push_u32(t, table_len as u32);
        for pair in [
            self.method,
            self.path,
            self.target,
            self.query,
            self.version,
            self.remote_addr,
            self.server_name,
        ] {
            push_u32(t, pair.0);
            push_u32(t, pair.1);
        }
        t.extend_from_slice(&self.content_length.to_le_bytes());
        push_u32(t, 0); // app_target, reserved
        push_u32(t, u32::from(self.server_port));
        push_u32(t, self.fields.len() as u32);
        for (no, nl, vo, vl) in &self.fields {
            push_u32(t, *no);
            push_u32(t, *nl);
            push_u32(t, *vo);
            push_u32(t, *vl);
        }
        push_u32(t, self.body.0);
        push_u32(t, self.body.1);

        debug_assert_eq!(t.len(), table_len);
        t.extend_from_slice(&self.blob);
        std::mem::take(&mut self.table)
    }
}

fn read_u32(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap())
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        RequestBuilder::new()
            .method(b"POST")
            .path(b"/index.php")
            .target(b"/index.php?x=1")
            .query(b"x=1")
            .version(b"HTTP/1.1")
            .remote_addr(b"10.0.0.7")
            .server_name(b"example.test")
            .server_port(8080)
            .content_length(11)
            .field(b"host", b"example.test")
            .field(b"accept", b"*/*")
            .preread_body(b"hello world")
            .finish()
    }

    #[test]
    fn builder_view_roundtrip() {
        let payload = sample();
        let view = RequestView::parse(&payload).unwrap();

        assert_eq!(view.method().unwrap(), b"POST");
        assert_eq!(view.path().unwrap(), b"/index.php");
        assert_eq!(view.target().unwrap(), b"/index.php?x=1");
        assert_eq!(view.query().unwrap(), b"x=1");
        assert_eq!(view.version().unwrap(), b"HTTP/1.1");
        assert_eq!(view.remote_addr().unwrap(), b"10.0.0.7");
        assert_eq!(view.server_name().unwrap(), b"example.test");
        assert_eq!(view.server_port(), 8080);
        assert_eq!(view.content_length(), 11);
        assert_eq!(view.preread_body().unwrap(), b"hello world");
    }

    #[test]
    fn fields_preserve_order_and_values() {
        let payload = sample();
        let view = RequestView::parse(&payload).unwrap();
        assert_eq!(view.fields_count(), 2);

        let fields: Vec<_> =
            view.fields().map(|f| f.unwrap()).map(|f| (f.name.to_vec(), f.value.to_vec())).collect();
        assert_eq!(fields[0], (b"host".to_vec(), b"example.test".to_vec()));
        assert_eq!(fields[1], (b"accept".to_vec(), b"*/*".to_vec()));
    }

    #[test]
    fn empty_request_roundtrips() {
        let payload = RequestBuilder::new().finish();
        let view = RequestView::parse(&payload).unwrap();
        assert_eq!(view.fields_count(), 0);
        assert_eq!(view.method().unwrap(), b"");
        assert_eq!(view.preread_body().unwrap(), b"");
        assert_eq!(view.content_length(), 0);
        assert_eq!(view.server_port(), 0);
    }

    #[test]
    fn parse_rejects_short_payload() {
        // Anything below the fixed table header is truncated.
        assert!(matches!(
            RequestView::parse(&[0u8; TABLE_FIXED - 1]),
            Err(BwpError::Truncated { expected: TABLE_FIXED, .. })
        ));
    }

    #[test]
    fn parse_rejects_impossible_blob_offset() {
        let mut payload = sample();
        // Corrupt blob_off to point before the table end.
        payload[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(RequestView::parse(&payload), Err(BwpError::Truncated { .. })));
    }

    #[test]
    fn slice_out_of_bounds_is_reported() {
        let mut payload = sample();
        // Blow up the method length field (table pos 8) past the blob.
        let bad_len = (payload.len() as u32).to_le_bytes();
        payload[8..12].copy_from_slice(&bad_len);
        assert!(matches!(
            RequestView::parse(&payload).unwrap().method(),
            Err(BwpError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn default_builder_matches_new() {
        let a = RequestBuilder::default().method(b"GET").finish();
        let b = RequestBuilder::new().method(b"GET").finish();
        assert_eq!(a, b);
    }
}
