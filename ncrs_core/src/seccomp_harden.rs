//! Best-effort seccomp hardening for the FUSE daemon process — only for a
//! daemon that (still) holds CAP_SYS_ADMIN itself.
//!
//! Packaged installs no longer give `ncrs` any capability: the one privileged
//! operation it needs, registering FUSE passthrough backing files, is done by
//! the separate `ncrs-passthrough` helper (see passthrough_broker.rs), which
//! runs under its own NO_NEW_PRIVS + seccomp allowlist. A capability-less
//! daemon cannot install a filter at all, and that is deliberate:
//!
//! - seccomp(2) installs a filter only for a task that has NO_NEW_PRIVS set
//!   or holds CAP_SYS_ADMIN. Without the capability, NO_NEW_PRIVS is the only
//!   way in, and it is inherited and irreversible. It would make the kernel
//!   ignore the set-uid bit of `fusermount3`, which the daemon runs to mount
//!   (now that it cannot mount(2) itself) and to unmount and detach stale
//!   mounts; and it would keep the helper's own file capability from taking
//!   effect when the daemon spawns it. Mounting, unmounting and passthrough
//!   would all break.
//! - A filter is also inherited by every child, `fusermount3` included, so
//!   it could never deny `mount`/`umount2` either.
//!
//! What the denied syscalls below guarded was mostly the capability itself
//! (pivot_root, swapon, quotactl, setns all need privileges an unprivileged
//! daemon does not have); without it a compromised daemon has the user's own
//! rights and nothing more, which is the baseline for any per-user process.
//!
//! When the daemon does hold CAP_SYS_ADMIN — a binary somebody `setcap`ed by
//! hand, or one left over from a package that predates the helper — the
//! deny-list is installed as before (the capability satisfies seccomp's
//! permission check, so no NO_NEW_PRIVS is needed), leaving `mount`, `umount2`
//! and `ioctl` alone: with the capability fuser mounts with mount(2) itself,
//! `fusermount3` children inherit the filter, and passthrough's backing
//! ioctls run in-process.
use std::collections::BTreeMap;
use std::convert::TryInto;

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule};

/// Syscalls with zero legitimate call site anywhere in ncrs that a compromised
/// process holding CAP_SYS_ADMIN could otherwise pivot into.
const DENY_LIST: &[i64] = &[
    libc::SYS_pivot_root,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_quotactl,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_keyctl,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// Install the deny-list filter, synced across every thread in the process
/// (there should only be the main thread at this point — this must run
/// before any worker threads are spawned, i.e. as the first thing
/// `mount_ncfs` does).
///
/// Only when the process holds CAP_SYS_ADMIN (see the module comment for why
/// a capability-less daemon installs nothing). Deliberately does not go
/// through `seccompiler::apply_filter*`: those unconditionally call
/// `prctl(PR_SET_NO_NEW_PRIVS, 1)` first, which would strip `fusermount3`'s
/// set-uid bit and the passthrough helper's file capability.
pub fn install() {
    if !ncrs_passthrough::caps::effective_has(ncrs_passthrough::caps::CAP_SYS_ADMIN) {
        log::debug!("seccomp hardening: not installed — ncrs runs without capabilities (see seccomp_harden.rs)");
        return;
    }
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for &syscall in DENY_LIST {
        rules.insert(syscall, vec![]);
    }

    let target_arch = match std::env::consts::ARCH.try_into() {
        Ok(a) => a,
        Err(_) => {
            log::debug!("seccomp hardening: unsupported architecture, skipping");
            return;
        }
    };

    let filter = match SeccompFilter::new(
        rules,
        SeccompAction::Allow,                     // mismatch: anything not in the deny list
        SeccompAction::Errno(libc::EPERM as u32),  // match: the deny list itself
        target_arch,
    ) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("seccomp hardening: failed to build filter: {:?}", e);
            return;
        }
    };

    let bpf_prog: BpfProgram = match filter.try_into() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("seccomp hardening: failed to compile filter: {:?}", e);
            return;
        }
    };

    let prog = libc::sock_fprog {
        len: bpf_prog.len() as u16,
        filter: bpf_prog.as_ptr() as *mut libc::sock_filter,
    };

    // SAFETY: `prog` borrows `bpf_prog` only for the duration of this syscall;
    // the kernel copies the filter in and does not retain the pointer.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC as libc::c_ulong,
            &prog as *const libc::sock_fprog,
        )
    };

    if rc == 0 {
        log::info!("seccomp hardening installed ({} syscalls denied)", DENY_LIST.len());
    } else {
        log::warn!("seccomp hardening: not installed ({})", std::io::Error::last_os_error());
    }
}
