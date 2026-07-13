//! Frame layout: fixed 16-byte header followed by `payload_len` bytes.

use crate::BwpError;

pub const FRAME_HEADER_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// Worker -> router, first frame: magic + version (u32).
    Hello = 1,
    /// Router -> worker: accepted version (u32).
    HelloAck = 2,
    /// Router -> worker: flat request (see `request.rs`).
    Request = 3,
    /// Worker -> router: status (aux) + serialized header block.
    ResponseHeaders = 4,
    /// Worker -> router: response body chunk.
    ResponseBody = 5,
    /// Worker -> router: request finished (supports early finish while the
    /// script keeps running — the `fastcgi_finish_request` analogue).
    End = 6,
    /// Worker -> router: request failed; router replies 502/503 to client.
    Error = 7,
    /// Worker -> router: line for the error log.
    Log = 8,
    /// Worker -> router: ready for the next request (pool accounting).
    /// Obsolete under the kernel-arbitrated work queue; tolerated on read.
    Ready = 9,
    /// Worker -> router: "request <id> is mine" — sent right after picking
    /// a request off the shared work socket (diagnostics, stuck tracking).
    Claim = 10,
    /// Router -> workers via the shared work socket: exactly one idle
    /// worker consumes it, finishes claimed requests and exits (dynamic
    /// pool shrink; graceful by contract).
    Retire = 11,
    /// Router -> worker on the private stream, after Claim of a request
    /// flagged FLAG_BODY_STREAM: one body chunk. A zero-length payload
    /// terminates the body; the worker judges completeness by comparing
    /// received bytes with content_length (short = client aborted).
    RequestBody = 12,
    /// Both directions, after a request flagged FLAG_UPGRADE was answered
    /// with 101: one complete WebSocket message, opcode in `aux`
    /// (WS_OP_TEXT / WS_OP_BINARY / WS_OP_CLOSE). The router owns RFC 6455:
    /// masking, fragmentation and ping/pong never reach the worker.
    WsMessage = 13,
}

impl TryFrom<u8> for FrameKind {
    type Error = BwpError;

    fn try_from(v: u8) -> Result<Self, BwpError> {
        Ok(match v {
            1 => Self::Hello,
            2 => Self::HelloAck,
            3 => Self::Request,
            4 => Self::ResponseHeaders,
            5 => Self::ResponseBody,
            6 => Self::End,
            7 => Self::Error,
            8 => Self::Log,
            9 => Self::Ready,
            10 => Self::Claim,
            11 => Self::Retire,
            12 => Self::RequestBody,
            13 => Self::WsMessage,
            other => return Err(BwpError::UnknownKind(other)),
        })
    }
}

/// ```text
/// 0    1      2        4            8             12         16
/// kind flags  reserved request_id   payload_len   aux
/// u8   u8     u16      u32          u32           u32
/// ```
/// `aux` carries the HTTP status for ResponseHeaders frames and is zero
/// otherwise. `flags` bit 0 (FLAG_BODY_FILE) on Request frames: the preread
/// body field holds a temp-file path instead of body bytes.
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub kind: FrameKind,
    pub flags: u8,
    pub request_id: u32,
    pub payload_len: u32,
    pub aux: u32,
}

impl FrameHeader {
    pub fn new(kind: FrameKind, request_id: u32, payload_len: u32) -> Self {
        Self { kind, flags: 0, request_id, payload_len, aux: 0 }
    }

    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0] = self.kind as u8;
        buf[1] = self.flags;
        buf[4..8].copy_from_slice(&self.request_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[12..16].copy_from_slice(&self.aux.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8; FRAME_HEADER_LEN]) -> Result<Self, BwpError> {
        Ok(Self {
            kind: FrameKind::try_from(buf[0])?,
            flags: buf[1],
            request_id: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            payload_len: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            aux: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KINDS: [FrameKind; 13] = [
        FrameKind::Hello,
        FrameKind::HelloAck,
        FrameKind::Request,
        FrameKind::ResponseHeaders,
        FrameKind::ResponseBody,
        FrameKind::End,
        FrameKind::Error,
        FrameKind::Log,
        FrameKind::Ready,
        FrameKind::Claim,
        FrameKind::Retire,
        FrameKind::RequestBody,
        FrameKind::WsMessage,
    ];

    #[test]
    fn frame_kind_roundtrips_through_u8() {
        for kind in ALL_KINDS {
            let byte = kind as u8;
            assert_eq!(FrameKind::try_from(byte).unwrap(), kind);
        }
    }

    #[test]
    fn frame_kind_rejects_unknown_discriminants() {
        for byte in [0u8, 14, 100, 255] {
            match FrameKind::try_from(byte) {
                Err(BwpError::UnknownKind(v)) => assert_eq!(v, byte),
                other => panic!("expected UnknownKind({byte}), got {other:?}"),
            }
        }
    }

    #[test]
    fn header_encode_has_fixed_layout() {
        let h = FrameHeader::new(FrameKind::Request, 1, 0);
        let buf = h.encode();
        assert_eq!(buf.len(), FRAME_HEADER_LEN);
        assert_eq!(buf[0], FrameKind::Request as u8);
        // bytes 2..4 are reserved and must stay zero.
        assert_eq!(&buf[2..4], &[0, 0]);
    }

    #[test]
    fn header_roundtrips_with_all_fields() {
        let h = FrameHeader {
            kind: FrameKind::ResponseHeaders,
            flags: 0b0000_0001,
            request_id: 0xDEAD_BEEF,
            payload_len: 0x0102_0304,
            aux: 404,
        };
        let decoded = FrameHeader::decode(&h.encode()).unwrap();
        assert_eq!(decoded.kind, h.kind);
        assert_eq!(decoded.flags, h.flags);
        assert_eq!(decoded.request_id, h.request_id);
        assert_eq!(decoded.payload_len, h.payload_len);
        assert_eq!(decoded.aux, h.aux);
    }

    #[test]
    fn header_uses_little_endian() {
        let h = FrameHeader::new(FrameKind::Request, 0x0403_0201, 0x0807_0605);
        let buf = h.encode();
        assert_eq!(&buf[4..8], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&buf[8..12], &[0x05, 0x06, 0x07, 0x08]);
    }

    #[test]
    fn new_zeroes_flags_and_aux() {
        let h = FrameHeader::new(FrameKind::End, 7, 3);
        assert_eq!(h.flags, 0);
        assert_eq!(h.aux, 0);
        assert_eq!(h.request_id, 7);
        assert_eq!(h.payload_len, 3);
    }

    #[test]
    fn decode_rejects_unknown_kind_byte() {
        let mut buf = FrameHeader::new(FrameKind::Hello, 1, 0).encode();
        buf[0] = 200;
        assert!(matches!(FrameHeader::decode(&buf), Err(BwpError::UnknownKind(200))));
    }
}
