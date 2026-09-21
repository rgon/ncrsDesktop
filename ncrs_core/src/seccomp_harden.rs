//! Best-effort seccomp hardening for the FUSE daemon process.
//!
//! `ncrs` is granted CAP_SYS_ADMIN (a file capability — see
//! packaging/maintainer-scripts/postinst) purely so it can request kernel
//! FUSE_PASSTHROUGH. That capability also unlocks a long list of unrelated
//! privileged syscalls that a memory-safety bug could otherwise pivot into.
//! This denies the ones ncrs never legitimately calls, while deliberately
//! leaving `mount`, `umount2` and `ioctl` untouched: `fusermount3`, spawned
//! as a child to unmount, still needs the former two, and FUSE_PASSTHROUGH's
//! own setup calls need the latter.
use std::collections::BTreeMap;
use std::convert::TryInto;

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule};

/// Syscalls with zero legitimate call site anywhere in ncrs, gated behind
/// CAP_SYS_ADMIN, that a compromised process could otherwise pivot into.
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
/// Best-effort: silently does nothing if the process lacks CAP_SYS_ADMIN (no
/// setcap — a dev build, or a minimal install where libcap2-bin was absent).
/// FUSE_PASSTHROUGH is unavailable for the same reason in that case, so there
/// is nothing extra to protect.
///
/// Deliberately does not go through `seccompiler::apply_filter*`: those
/// unconditionally call `prctl(PR_SET_NO_NEW_PRIVS, 1)` first, which would
/// strip `fusermount3`'s own setuid bit the next time ncrs shells out to it
/// to unmount. Installing a filter while already holding CAP_SYS_ADMIN
/// satisfies seccomp(2)'s permission check on its own, so NO_NEW_PRIVS is
/// never needed (or wanted) here.
pub fn install() {
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
        // No CAP_SYS_ADMIN (setcap absent) and NO_NEW_PRIVS unset — expected
        // on a dev build or a minimal install; passthrough is unavailable for
        // the same reason, so this is not a regression.
        log::debug!(
            "seccomp hardening: not installed ({}) — likely missing CAP_SYS_ADMIN",
            std::io::Error::last_os_error()
        );
    }
}
