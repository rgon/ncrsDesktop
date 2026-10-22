//! Privilege separation for kernel FUSE passthrough.
//!
//! The kernel only lets a task holding CAP_SYS_ADMIN register a passthrough
//! backing file on a FUSE connection (`FUSE_DEV_IOC_BACKING_OPEN` /
//! `FUSE_DEV_IOC_BACKING_CLOSE` on the /dev/fuse descriptor). Granting that
//! capability to the whole network-facing `ncrs` daemon also let fuser mount
//! with a direct mount(2), bypassing fusermount3's policy, and turned any
//! remote code execution in the daemon into root. So the capability lives on
//! one tiny binary instead, `ncrs-passthrough` (src/main.rs), which does
//! nothing but those two ioctls:
//!
//! - The daemon spawns it with one end of a `socketpair(AF_UNIX,
//!   SOCK_SEQPACKET)` as fd 3 — no listening socket, no path anyone else can
//!   connect to — and sends it the /dev/fuse descriptor ([`proto::Op::Setup`]).
//! - For every passthrough open the daemon sends the opened cache file
//!   ([`proto::Op::Open`]); the helper checks it ([`server::validate_backing_fd`]),
//!   registers it, and answers the kernel's backing id. File data never passes
//!   through the helper: after `opened_passthrough` the kernel reads the
//!   backing file directly.
//! - [`proto::Op::Close`] releases an id, but only one this helper handed out
//!   (or adopted from a predecessor before its first open, after a daemon
//!   restart — [`proto::Op::Adopt`]).
//!
//! The helper drops every other capability, sets NO_NEW_PRIVS and installs a
//! seccomp allowlist before it reads its first message (src/main.rs).

pub mod caps;
pub mod client;
pub mod proto;
pub mod server;
