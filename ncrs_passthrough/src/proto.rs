//! The helper's wire format: one fixed-size request per SOCK_SEQPACKET
//! message, one fixed-size reply. Native byte order (both ends run on the same
//! host, one spawned by the other).
//!
//! Request (16 bytes): `magic u32 | op u32 | id u32 | reserved u32 (= 0)`, with
//! exactly one descriptor attached (SCM_RIGHTS) for `Setup` and `Open` and
//! none otherwise.
//!
//! Reply (8 bytes): `errno u32 (0 = success) | value u32`. `value` is the
//! backing id for `Open`, 1/0 for "holds CAP_SYS_ADMIN" for `Setup`, 0 else.

/// "NCPT": guards against a stray writer on the wrong descriptor.
pub const MAGIC: u32 = 0x4e43_5054;
pub const REQ_LEN: usize = 16;
pub const REPLY_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Op {
    /// Attach the connection's /dev/fuse descriptor. Once per helper.
    Setup = 1,
    /// Accept `CLOSE <id>` for an id a previous daemon registered on this
    /// connection (restart survival). Only before the first `Open`.
    Adopt = 2,
    /// Register the attached read-only regular file as a backing file.
    Open = 3,
    /// Release a backing id this helper handed out or adopted.
    Close = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Setup,
    Adopt(u32),
    Open,
    Close(u32),
}

impl Request {
    fn op_id(self) -> (Op, u32) {
        match self {
            Request::Setup => (Op::Setup, 0),
            Request::Adopt(id) => (Op::Adopt, id),
            Request::Open => (Op::Open, 0),
            Request::Close(id) => (Op::Close, id),
        }
    }

    /// Whether this request travels with exactly one descriptor.
    pub fn carries_fd(self) -> bool {
        matches!(self, Request::Setup | Request::Open)
    }

    pub fn encode(self) -> [u8; REQ_LEN] {
        let (op, id) = self.op_id();
        let mut b = [0u8; REQ_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_ne_bytes());
        b[4..8].copy_from_slice(&(op as u32).to_ne_bytes());
        b[8..12].copy_from_slice(&id.to_ne_bytes());
        b
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parses one received message. `nfds` is how many descriptors arrived with
/// it, `truncated` whether the kernel cut the payload or the control data
/// (MSG_TRUNC / MSG_CTRUNC). Errors are the errno to answer with.
pub fn parse_request(buf: &[u8], nfds: usize, truncated: bool) -> Result<Request, i32> {
    if truncated || buf.len() != REQ_LEN {
        return Err(libc::EINVAL);
    }
    if u32_at(buf, 0) != MAGIC {
        return Err(libc::EPROTO);
    }
    if u32_at(buf, 12) != 0 {
        return Err(libc::EINVAL);
    }
    let id = u32_at(buf, 8);
    let req = match u32_at(buf, 4) {
        1 => Request::Setup,
        2 => Request::Adopt(id),
        3 => Request::Open,
        4 => Request::Close(id),
        _ => return Err(libc::EOPNOTSUPP),
    };
    let id_ok = match req {
        // The kernel's backing ids start at 1 (idr_alloc_cyclic(.., 1, 0)).
        Request::Adopt(id) | Request::Close(id) => id != 0,
        Request::Setup | Request::Open => id == 0,
    };
    let fds_ok = nfds == if req.carries_fd() { 1 } else { 0 };
    if !id_ok || !fds_ok {
        return Err(libc::EINVAL);
    }
    Ok(req)
}

pub fn encode_reply(r: Result<u32, i32>) -> [u8; REPLY_LEN] {
    let (err, val) = match r {
        Ok(v) => (0u32, v),
        Err(e) => (e.max(1) as u32, 0),
    };
    let mut b = [0u8; REPLY_LEN];
    b[0..4].copy_from_slice(&err.to_ne_bytes());
    b[4..8].copy_from_slice(&val.to_ne_bytes());
    b
}

/// None for a reply of the wrong size.
pub fn decode_reply(buf: &[u8]) -> Option<Result<u32, i32>> {
    if buf.len() != REPLY_LEN {
        return None;
    }
    let err = u32_at(buf, 0);
    let val = u32_at(buf, 4);
    Some(if err == 0 { Ok(val) } else { Err(err as i32) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_round_trips() {
        for r in [Request::Setup, Request::Adopt(7), Request::Open, Request::Close(u32::MAX)] {
            let nfds = if r.carries_fd() { 1 } else { 0 };
            assert_eq!(parse_request(&r.encode(), nfds, false), Ok(r));
        }
    }

    #[test]
    fn wrong_length_is_refused() {
        let b = Request::Open.encode();
        assert_eq!(parse_request(&b[..15], 1, false), Err(libc::EINVAL));
        let mut long = b.to_vec();
        long.push(0);
        assert_eq!(parse_request(&long, 1, false), Err(libc::EINVAL));
        assert_eq!(parse_request(&[], 0, false), Err(libc::EINVAL));
    }

    #[test]
    fn truncated_message_is_refused() {
        assert_eq!(parse_request(&Request::Open.encode(), 1, true), Err(libc::EINVAL));
    }

    #[test]
    fn bad_magic_reserved_and_op_are_refused() {
        let mut b = Request::Close(3).encode();
        b[0] ^= 1;
        assert_eq!(parse_request(&b, 0, false), Err(libc::EPROTO));
        let mut b = Request::Close(3).encode();
        b[13] = 1;
        assert_eq!(parse_request(&b, 0, false), Err(libc::EINVAL));
        let mut b = Request::Close(3).encode();
        b[4..8].copy_from_slice(&99u32.to_ne_bytes());
        assert_eq!(parse_request(&b, 0, false), Err(libc::EOPNOTSUPP));
    }

    #[test]
    fn descriptor_count_must_match_the_op() {
        assert_eq!(parse_request(&Request::Open.encode(), 0, false), Err(libc::EINVAL));
        assert_eq!(parse_request(&Request::Setup.encode(), 2, false), Err(libc::EINVAL));
        assert_eq!(parse_request(&Request::Close(1).encode(), 1, false), Err(libc::EINVAL));
        assert_eq!(parse_request(&Request::Adopt(1).encode(), 1, false), Err(libc::EINVAL));
    }

    #[test]
    fn id_zero_and_stray_ids_are_refused() {
        assert_eq!(parse_request(&Request::Close(0).encode(), 0, false), Err(libc::EINVAL));
        assert_eq!(parse_request(&Request::Adopt(0).encode(), 0, false), Err(libc::EINVAL));
        let mut b = Request::Open.encode();
        b[8..12].copy_from_slice(&5u32.to_ne_bytes());
        assert_eq!(parse_request(&b, 1, false), Err(libc::EINVAL));
    }

    #[test]
    fn replies_round_trip() {
        assert_eq!(decode_reply(&encode_reply(Ok(42))), Some(Ok(42)));
        assert_eq!(decode_reply(&encode_reply(Err(libc::EPERM))), Some(Err(libc::EPERM)));
        assert_eq!(decode_reply(&[0u8; 7]), None);
    }
}
