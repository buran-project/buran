//! Hello / HelloAck payloads: the capability negotiation of BWP.
//!
//! The worker opens with Hello declaring who it is and what it can do; the
//! router answers with HelloAck carrying the *granted* concurrency (the
//! declared value capped by `applications.<name>.concurrency`). The worker
//! must never hold more claimed-and-unfinished requests than granted.

use crate::{BwpError, BWP_MAGIC};

/// Declared in Hello: "no fixed limit, cap me via config if you care".
pub const CONCURRENCY_UNBOUNDED: u32 = 0;

/// Hello frame payload (worker -> router).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    /// Worker process id: lets the router kill a wedged worker precisely.
    pub pid: u32,
    /// Requests the worker can process at once: 1 for blocking runtimes,
    /// N for event loops, CONCURRENCY_UNBOUNDED for "whatever arrives".
    pub concurrency: u32,
    /// CAP_* bit set.
    pub capabilities: u32,
}

pub const HELLO_PAYLOAD_LEN: usize = 4 + 4 * 4;

impl Hello {
    pub fn encode(&self) -> [u8; HELLO_PAYLOAD_LEN] {
        let mut buf = [0u8; HELLO_PAYLOAD_LEN];
        buf[0..4].copy_from_slice(BWP_MAGIC);
        buf[4..8].copy_from_slice(&self.version.to_le_bytes());
        buf[8..12].copy_from_slice(&self.pid.to_le_bytes());
        buf[12..16].copy_from_slice(&self.concurrency.to_le_bytes());
        buf[16..20].copy_from_slice(&self.capabilities.to_le_bytes());
        buf
    }

    pub fn decode(payload: &[u8]) -> Result<Self, BwpError> {
        if payload.len() < HELLO_PAYLOAD_LEN {
            return Err(BwpError::Truncated {
                expected: HELLO_PAYLOAD_LEN,
                actual: payload.len(),
            });
        }
        if &payload[0..4] != BWP_MAGIC {
            return Err(BwpError::BadMagic);
        }
        Ok(Self {
            version: u32::from_le_bytes(payload[4..8].try_into().unwrap()),
            pid: u32::from_le_bytes(payload[8..12].try_into().unwrap()),
            concurrency: u32::from_le_bytes(payload[12..16].try_into().unwrap()),
            capabilities: u32::from_le_bytes(payload[16..20].try_into().unwrap()),
        })
    }
}

/// HelloAck frame payload (router -> worker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloAck {
    pub version: u32,
    /// Effective concurrency: min(declared, config cap). u32::MAX when both
    /// sides left it unbounded.
    pub concurrency: u32,
}

pub const HELLO_ACK_PAYLOAD_LEN: usize = 4 * 2;

impl HelloAck {
    pub fn encode(&self) -> [u8; HELLO_ACK_PAYLOAD_LEN] {
        let mut buf = [0u8; HELLO_ACK_PAYLOAD_LEN];
        buf[0..4].copy_from_slice(&self.version.to_le_bytes());
        buf[4..8].copy_from_slice(&self.concurrency.to_le_bytes());
        buf
    }

    pub fn decode(payload: &[u8]) -> Result<Self, BwpError> {
        if payload.len() < HELLO_ACK_PAYLOAD_LEN {
            return Err(BwpError::Truncated {
                expected: HELLO_ACK_PAYLOAD_LEN,
                actual: payload.len(),
            });
        }
        Ok(Self {
            version: u32::from_le_bytes(payload[0..4].try_into().unwrap()),
            concurrency: u32::from_le_bytes(payload[4..8].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips() {
        let h = Hello { version: 1, pid: 4242, concurrency: 128, capabilities: 0b1 };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn hello_rejects_bad_magic() {
        let mut buf = Hello { version: 1, pid: 1, concurrency: 1, capabilities: 0 }.encode();
        buf[0] = b'X';
        assert!(matches!(Hello::decode(&buf), Err(BwpError::BadMagic)));
    }

    #[test]
    fn hello_rejects_short_payload() {
        assert!(matches!(
            Hello::decode(&[0u8; HELLO_PAYLOAD_LEN - 1]),
            Err(BwpError::Truncated { .. })
        ));
    }

    #[test]
    fn hello_ack_roundtrips() {
        let a = HelloAck { version: 1, concurrency: u32::MAX };
        assert_eq!(HelloAck::decode(&a.encode()).unwrap(), a);
    }

    #[test]
    fn hello_ack_rejects_short_payload() {
        assert!(matches!(HelloAck::decode(&[0u8; 4]), Err(BwpError::Truncated { .. })));
    }
}
