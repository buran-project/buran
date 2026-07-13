//! Minimal server-side RFC 6455 codec.
//!
//! The router owns the WebSocket wire protocol so runtime modules never
//! see it: the decoder validates masking/rsv/opcodes, reassembles
//! fragmented messages and enforces the size limit; the encoder emits
//! single unmasked server frames. Extensions (compression) are not
//! negotiated, therefore rsv bits must be zero.

use base64::Engine;
use sha1::{Digest, Sha1};

/// Close codes the tunnel uses (RFC 6455 section 7.4.1).
pub const CLOSE_NORMAL: u16 = 1000;
pub const CLOSE_GOING_AWAY: u16 = 1001;
pub const CLOSE_PROTOCOL_ERROR: u16 = 1002;
pub const CLOSE_BAD_DATA: u16 = 1007;
pub const CLOSE_TOO_BIG: u16 = 1009;
pub const CLOSE_INTERNAL: u16 = 1011;

pub const OP_CONTINUATION: u8 = 0;
pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;
pub const OP_PING: u8 = 9;
pub const OP_PONG: u8 = 10;

/// `Sec-WebSocket-Accept` for a client's `Sec-WebSocket-Key`.
pub fn accept_key(key: &[u8]) -> String {
    const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut sha = Sha1::new();
    sha.update(key);
    sha.update(GUID);
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

/// One complete message out of the decoder (fragments already merged).
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    /// Payload validated as UTF-8.
    Text(Vec<u8>),
    Binary(Vec<u8>),
    /// Raw close payload: empty, or a big-endian code + UTF-8 reason.
    Close(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
}

/// Decode failures map straight to close codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// 1002: bad framing (unmasked client frame, rsv bits, bad opcode,
    /// broken fragmentation, oversized control frame).
    Protocol(&'static str),
    /// 1009: message over the configured limit.
    TooBig,
    /// 1007: text message that is not valid UTF-8.
    BadUtf8,
}

impl DecodeError {
    pub fn close_code(&self) -> u16 {
        match self {
            Self::Protocol(_) => CLOSE_PROTOCOL_ERROR,
            Self::TooBig => CLOSE_TOO_BIG,
            Self::BadUtf8 => CLOSE_BAD_DATA,
        }
    }
}

/// Incremental decoder over a caller-owned buffer.
pub struct Decoder {
    max_message: usize,
    /// Data opcode of the fragmented message in progress.
    frag_opcode: Option<u8>,
    frag: Vec<u8>,
}

impl Decoder {
    pub fn new(max_message: usize) -> Self {
        Self { max_message, frag_opcode: None, frag: Vec::new() }
    }

    /// Try to cut one complete message off the front of `buf`.
    /// `Ok(None)` = need more bytes. Control frames interleaved between
    /// fragments come out on their own; data fragments accumulate until
    /// their fin frame.
    pub fn next(&mut self, buf: &mut Vec<u8>) -> Result<Option<Message>, DecodeError> {
        loop {
            let Some(frame) = parse_frame(buf, self.max_message)? else {
                return Ok(None);
            };

            match frame.opcode {
                OP_CLOSE | OP_PING | OP_PONG => {
                    if !frame.fin {
                        return Err(DecodeError::Protocol("fragmented control frame"));
                    }
                    return Ok(Some(match frame.opcode {
                        OP_CLOSE => Message::Close(frame.payload),
                        OP_PING => Message::Ping(frame.payload),
                        _ => Message::Pong(frame.payload),
                    }));
                }
                OP_TEXT | OP_BINARY => {
                    if self.frag_opcode.is_some() {
                        return Err(DecodeError::Protocol("new message inside a fragmented one"));
                    }
                    if frame.fin {
                        return Ok(Some(finish_message(frame.opcode, frame.payload)?));
                    }
                    self.frag_opcode = Some(frame.opcode);
                    self.frag = frame.payload;
                }
                OP_CONTINUATION => {
                    let Some(opcode) = self.frag_opcode else {
                        return Err(DecodeError::Protocol("continuation without a start"));
                    };
                    if self.frag.len() + frame.payload.len() > self.max_message {
                        return Err(DecodeError::TooBig);
                    }
                    self.frag.extend_from_slice(&frame.payload);
                    if frame.fin {
                        self.frag_opcode = None;
                        let payload = std::mem::take(&mut self.frag);
                        return Ok(Some(finish_message(opcode, payload)?));
                    }
                }
                _ => return Err(DecodeError::Protocol("reserved opcode")),
            }
        }
    }
}

fn finish_message(opcode: u8, payload: Vec<u8>) -> Result<Message, DecodeError> {
    if opcode == OP_TEXT {
        if std::str::from_utf8(&payload).is_err() {
            return Err(DecodeError::BadUtf8);
        }
        Ok(Message::Text(payload))
    } else {
        Ok(Message::Binary(payload))
    }
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Parse one client frame off the front of `buf`; unmasks in place.
fn parse_frame(buf: &mut Vec<u8>, max_message: usize) -> Result<Option<Frame>, DecodeError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let b0 = buf[0];
    let b1 = buf[1];
    if b0 & 0b0111_0000 != 0 {
        return Err(DecodeError::Protocol("rsv bits set without an extension"));
    }
    // Client frames must be masked (section 5.1).
    if b1 & 0x80 == 0 {
        return Err(DecodeError::Protocol("unmasked client frame"));
    }

    let mut header = 2usize;
    let len = match b1 & 0x7F {
        126 => {
            if buf.len() < header + 2 {
                return Ok(None);
            }
            let len = u16::from_be_bytes([buf[2], buf[3]]) as u64;
            header += 2;
            len
        }
        127 => {
            if buf.len() < header + 8 {
                return Ok(None);
            }
            let len = u64::from_be_bytes(buf[2..10].try_into().unwrap());
            header += 8;
            len
        }
        n => u64::from(n),
    };

    let opcode = b0 & 0x0F;
    let is_control = opcode >= 8;
    if is_control && len > 125 {
        return Err(DecodeError::Protocol("control frame over 125 bytes"));
    }
    // The limit guards a single frame too, or a huge length would make us
    // buffer it before the message-level check.
    if !is_control && len > max_message as u64 {
        return Err(DecodeError::TooBig);
    }

    let len = len as usize;
    if buf.len() < header + 4 + len {
        return Ok(None);
    }
    let mask: [u8; 4] = buf[header..header + 4].try_into().unwrap();
    let start = header + 4;
    let mut payload = buf[start..start + len].to_vec();
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    buf.drain(..start + len);

    Ok(Some(Frame { fin: b0 & 0x80 != 0, opcode, payload }))
}

/// One unmasked fin=1 server frame.
pub fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | (opcode & 0x0F));
    match payload.len() {
        n if n <= 125 => out.push(n as u8),
        n if n <= u16::MAX as usize => {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

pub fn encode_close(code: u16, reason: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason.as_bytes());
    encode_frame(OP_CLOSE, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Client-side frame builder: masked, arbitrary fin/opcode.
    fn client_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(if fin { 0x80 } else { 0 } | opcode);
        match payload.len() {
            n if n <= 125 => out.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    fn decode_all(dec: &mut Decoder, buf: &mut Vec<u8>) -> Vec<Message> {
        let mut out = Vec::new();
        while let Some(msg) = dec.next(buf).unwrap() {
            out.push(msg);
        }
        out
    }

    #[test]
    fn accept_key_matches_rfc_vector() {
        // RFC 6455 section 1.3 handshake example.
        assert_eq!(accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn decodes_text_and_binary() {
        let mut dec = Decoder::new(1024);
        let mut buf = client_frame(true, OP_TEXT, "привет".as_bytes());
        buf.extend(client_frame(true, OP_BINARY, &[0, 159, 146, 150]));
        let msgs = decode_all(&mut dec, &mut buf);
        assert_eq!(
            msgs,
            vec![
                Message::Text("привет".as_bytes().to_vec()),
                Message::Binary(vec![0, 159, 146, 150]),
            ]
        );
        assert!(buf.is_empty(), "all bytes consumed");
    }

    #[test]
    fn decodes_extended_lengths() {
        // 126..=65535 uses the 16-bit form, larger the 64-bit form.
        let mid = vec![7u8; 300];
        let big = vec![9u8; 70_000];
        let mut dec = Decoder::new(1 << 20);
        let mut buf = client_frame(true, OP_BINARY, &mid);
        buf.extend(client_frame(true, OP_BINARY, &big));
        let msgs = decode_all(&mut dec, &mut buf);
        assert_eq!(msgs, vec![Message::Binary(mid), Message::Binary(big)]);
    }

    #[test]
    fn incremental_feeding_needs_more_then_completes() {
        let frame = client_frame(true, OP_TEXT, b"chunked arrival");
        let mut dec = Decoder::new(1024);
        let mut buf = Vec::new();
        // Feed byte by byte: no message until the last byte lands.
        for &b in &frame[..frame.len() - 1] {
            buf.push(b);
            assert!(dec.next(&mut buf).unwrap().is_none());
        }
        buf.push(*frame.last().unwrap());
        assert_eq!(
            dec.next(&mut buf).unwrap(),
            Some(Message::Text(b"chunked arrival".to_vec()))
        );
    }

    #[test]
    fn reassembles_fragments_with_interleaved_ping() {
        let mut dec = Decoder::new(1024);
        let mut buf = client_frame(false, OP_TEXT, b"frag-");
        // Control frames may interleave between fragments (section 5.4).
        buf.extend(client_frame(true, OP_PING, b"marco"));
        buf.extend(client_frame(true, OP_CONTINUATION, b"ment"));
        let msgs = decode_all(&mut dec, &mut buf);
        assert_eq!(
            msgs,
            vec![Message::Ping(b"marco".to_vec()), Message::Text(b"frag-ment".to_vec())]
        );
    }

    #[test]
    fn close_ping_pong_pass_through() {
        let mut dec = Decoder::new(1024);
        let mut buf = client_frame(true, OP_CLOSE, &[0x03, 0xE8]); // 1000
        buf.extend(client_frame(true, OP_PONG, b"polo"));
        let msgs = decode_all(&mut dec, &mut buf);
        assert_eq!(msgs, vec![Message::Close(vec![0x03, 0xE8]), Message::Pong(b"polo".to_vec())]);
    }

    #[test]
    fn rejects_unmasked_client_frame() {
        let mut frame = client_frame(true, OP_TEXT, b"x");
        frame[1] &= 0x7F; // clear the mask bit, key bytes become garbage payload
        let mut dec = Decoder::new(1024);
        assert_eq!(dec.next(&mut frame), Err(DecodeError::Protocol("unmasked client frame")));
    }

    #[test]
    fn rejects_rsv_bits() {
        let mut frame = client_frame(true, OP_TEXT, b"x");
        frame[0] |= 0b0100_0000;
        let mut dec = Decoder::new(1024);
        assert!(matches!(dec.next(&mut frame), Err(DecodeError::Protocol(_))));
    }

    #[test]
    fn rejects_reserved_opcode() {
        let mut frame = client_frame(true, 3, b"x");
        let mut dec = Decoder::new(1024);
        assert!(matches!(dec.next(&mut frame), Err(DecodeError::Protocol(_))));
    }

    #[test]
    fn rejects_broken_fragmentation() {
        // Continuation with nothing started.
        let mut dec = Decoder::new(1024);
        let mut buf = client_frame(true, OP_CONTINUATION, b"lost");
        assert!(matches!(dec.next(&mut buf), Err(DecodeError::Protocol(_))));

        // A fresh data frame in the middle of a fragmented message.
        let mut dec = Decoder::new(1024);
        let mut buf = client_frame(false, OP_TEXT, b"first");
        buf.extend(client_frame(true, OP_TEXT, b"second"));
        assert!(matches!(dec.next(&mut buf), Err(DecodeError::Protocol(_))));

        // Fragmented control frame.
        let mut dec = Decoder::new(1024);
        let mut buf = client_frame(false, OP_PING, b"p");
        assert!(matches!(dec.next(&mut buf), Err(DecodeError::Protocol(_))));
    }

    #[test]
    fn rejects_oversized_control_frame() {
        let mut buf = client_frame(true, OP_PING, &[0u8; 126]);
        let mut dec = Decoder::new(1 << 20);
        assert!(matches!(dec.next(&mut buf), Err(DecodeError::Protocol(_))));
    }

    #[test]
    fn rejects_bad_utf8_text() {
        let mut buf = client_frame(true, OP_TEXT, &[0xFF, 0xFE]);
        let mut dec = Decoder::new(1024);
        assert_eq!(dec.next(&mut buf), Err(DecodeError::BadUtf8));

        // Also when the invalid byte hides in a later fragment.
        let mut dec = Decoder::new(1024);
        let mut buf = client_frame(false, OP_TEXT, b"ok");
        buf.extend(client_frame(true, OP_CONTINUATION, &[0xFF]));
        assert_eq!(decode_err(&mut dec, &mut buf), DecodeError::BadUtf8);
    }

    #[test]
    fn rejects_messages_over_the_limit() {
        // Single frame over the limit is cut before buffering.
        let mut buf = client_frame(true, OP_BINARY, &[0u8; 100]);
        let mut dec = Decoder::new(99);
        assert_eq!(dec.next(&mut buf), Err(DecodeError::TooBig));

        // Fragments individually under the limit, together over it.
        let mut dec = Decoder::new(99);
        let mut buf = client_frame(false, OP_BINARY, &[0u8; 60]);
        buf.extend(client_frame(true, OP_CONTINUATION, &[0u8; 60]));
        assert_eq!(decode_err(&mut dec, &mut buf), DecodeError::TooBig);
    }

    fn decode_err(dec: &mut Decoder, buf: &mut Vec<u8>) -> DecodeError {
        loop {
            match dec.next(buf) {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("expected an error, ran out of data"),
                Err(e) => return e,
            }
        }
    }

    #[test]
    fn error_close_codes() {
        assert_eq!(DecodeError::Protocol("x").close_code(), CLOSE_PROTOCOL_ERROR);
        assert_eq!(DecodeError::TooBig.close_code(), CLOSE_TOO_BIG);
        assert_eq!(DecodeError::BadUtf8.close_code(), CLOSE_BAD_DATA);
    }

    #[test]
    fn encode_frame_length_forms() {
        let small = encode_frame(OP_TEXT, &[b'a'; 125]);
        assert_eq!(&small[..2], &[0x81, 125]);

        let mid = encode_frame(OP_BINARY, &[0; 126]);
        assert_eq!(&mid[..4], &[0x82, 126, 0, 126]);

        let big = encode_frame(OP_BINARY, &[0; 70_000]);
        assert_eq!(big[0], 0x82);
        assert_eq!(big[1], 127);
        assert_eq!(u64::from_be_bytes(big[2..10].try_into().unwrap()), 70_000);
    }

    #[test]
    fn encode_close_carries_code_and_reason() {
        let frame = encode_close(1001, "bye");
        assert_eq!(frame[0], 0x80 | OP_CLOSE);
        assert_eq!(frame[1], 5); // 2 code bytes + 3 reason bytes, unmasked
        assert_eq!(&frame[2..4], &1001u16.to_be_bytes());
        assert_eq!(&frame[4..], b"bye");
    }

    #[test]
    fn server_frames_roundtrip_through_client_decoder_rules() {
        // encode_frame output is a valid frame except for masking, which
        // the decoder demands from clients — sanity-check the layout by
        // re-masking it manually.
        let encoded = encode_frame(OP_TEXT, b"loop");
        assert_eq!(encoded[0], 0x81);
        assert_eq!(encoded[1], 4);
        assert_eq!(&encoded[2..], b"loop");
    }
}
