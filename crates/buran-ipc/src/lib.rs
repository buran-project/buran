//! Buran Worker Protocol (BWP) v1.
//!
//! Transport-agnostic framing and the flat request encoding: one contiguous
//! blob with an offset table, so the worker never re-parses HTTP and reads
//! fields without copying. The transport (UDS in v1) lives in the
//! router and in the `buran-worker` SDK; this crate only defines bytes.
//!
//! All integers are little-endian.
//!
//! # Concurrency contract
//!
//! The protocol is designed for both blocking runtimes (PHP: one request at
//! a time) and event-loop runtimes (Node, Go, PHP TrueAsync: many requests
//! per process). The rules every worker must follow:
//!
//! - The worker declares its concurrency in Hello; the router answers with
//!   the granted value in HelloAck (declared capped by config). The worker
//!   must never hold more claimed-and-unfinished requests than granted.
//! - The work socket is a shared SOCK_DGRAM queue arbitrated by the kernel.
//!   A worker at its concurrency limit must NOT consume work datagrams:
//!   staying out of `recv` is what keeps the kernel balancing load across
//!   workers.
//! - Every frame carries a request id; frames of concurrent requests may
//!   interleave freely on the response stream in both directions.
//! - Retire is graceful: after consuming a Retire datagram the worker picks
//!   up no new work, finishes every claimed request, then exits (closing
//!   the response stream tells the router).
//! - A worker that responded to a streamed-body request early must still
//!   drain that request's RequestBody frames up to the terminator.
//!
//! # WebSocket contract
//!
//! A request flagged FLAG_UPGRADE is a WebSocket upgrade offer delivered to
//! workers that declared CAP_WEBSOCKET. The application decides:
//!
//! - reject: answer with a regular HTTP response — it goes to the client
//!   as-is, no tunnel opens;
//! - accept: answer ResponseHeaders with status 101 (optionally carrying
//!   `sec-websocket-protocol`). The router computes Sec-WebSocket-Accept
//!   and owns RFC 6455 from there: masking, fragment reassembly, UTF-8
//!   checks and ping/pong never reach the worker.
//!
//! After 101 both sides exchange WsMessage frames (opcode in `aux`): whole
//! messages only. A close from the client arrives as WsMessage/WS_OP_CLOSE
//! and the worker must answer End; a worker that wants to close sends End
//! (the router performs the closing handshake with code 1000). The
//! request's concurrency slot stays occupied for the tunnel's lifetime.

mod frame;
mod hello;
mod request;

pub use frame::{FrameHeader, FrameKind, FRAME_HEADER_LEN};
pub use hello::{
    Hello, HelloAck, CONCURRENCY_UNBOUNDED, HELLO_ACK_PAYLOAD_LEN, HELLO_PAYLOAD_LEN,
};
pub use request::{FieldView, RequestBuilder, RequestView};

use thiserror::Error;

/// Protocol version negotiated in the Hello/HelloAck exchange.
pub const BWP_VERSION: u32 = 1;

/// Magic prefix of the Hello frame payload.
pub const BWP_MAGIC: &[u8; 4] = b"BWP\0";

/// Request frame flag: the preread body field carries a temp-file path
/// (bodies larger than the datagram budget spill to disk).
pub const FLAG_BODY_FILE: u8 = 0b0000_0001;

/// Request frame flag: the body arrives as RequestBody frames on the
/// worker's stream after Claim; the preread body field is empty and
/// content_length holds the expected total.
pub const FLAG_BODY_STREAM: u8 = 0b0000_0010;

/// Request frame flag: this request is a WebSocket upgrade offer (see the
/// WebSocket contract above). Only sent to CAP_WEBSOCKET workers.
pub const FLAG_UPGRADE: u8 = 0b0000_0100;

/// Hello capability: the worker accepts FLAG_BODY_STREAM requests.
pub const CAP_BODY_STREAM: u32 = 0b0000_0001;

/// Hello capability: the worker accepts FLAG_UPGRADE requests and speaks
/// WsMessage frames.
pub const CAP_WEBSOCKET: u32 = 0b0000_0010;

/// WsMessage opcodes (the `aux` field), straight from RFC 6455.
pub const WS_OP_TEXT: u32 = 1;
pub const WS_OP_BINARY: u32 = 2;
/// Payload: big-endian u16 status code + UTF-8 reason (both optional).
pub const WS_OP_CLOSE: u32 = 8;

/// Pong status (the `aux` field of a Pong frame): the worker is free.
pub const PONG_IDLE: u32 = 0;
/// Pong status: the worker is still busy on the probed task.
pub const PONG_BUSY: u32 = 1;

#[derive(Debug, Error)]
pub enum BwpError {
    #[error("frame payload too short: {actual} < {expected}")]
    Truncated { expected: usize, actual: usize },
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("bad magic in Hello frame")]
    BadMagic,
    #[error("unsupported BWP version {0}")]
    UnsupportedVersion(u32),
    #[error("offset table entry out of bounds: {off}+{len} > {blob}")]
    OutOfBounds { off: usize, len: usize, blob: usize },
}
