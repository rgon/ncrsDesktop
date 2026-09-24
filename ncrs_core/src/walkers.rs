//! Who is walking the tree, and a speed limit for them.
//!
//! A recursive walk (`find /`, `rg ~`, `du`, a backup tool) turns into one
//! server listing per directory it enters. On 2026-09-24 backgrounded
//! `find / -name …` commands from coding agents drove ~20 listings/s through
//! the mount for hours, and the logs could not say who was asking: the FUSE
//! handlers ignored the requester.
//!
//! [`WalkerTracker`] resolves each requesting pid to its command and parent
//! chain (`find←bash←claude`), counts the *uncached* listings it causes, warns
//! once when a process looks like a crawler, and gives every process a token
//! bucket for uncached listings. A process over budget waits on its (pool)
//! worker before its listing is fetched. Cached answers are never limited, so
//! browsing stays instant; only a sustained crawl of cold directories slows to
//! the refill rate.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Uncached listings a process may cause back-to-back.
pub const BURST: f64 = 50.0;
/// Sustained uncached listings per second per process once the burst is spent.
pub const REFILL_PER_SEC: f64 = 10.0;
/// Longest a single listing is held back, so a throttled walker keeps moving.
pub const MAX_WAIT: Duration = Duration::from_secs(2);
/// A process causing more uncached listings than this per minute is a walker.
pub const WALKER_PER_MIN: u32 = 60;
/// Processes remembered at once; the least recently seen are dropped.
const MAX_TRACKED: usize = 256;
const WARN_EVERY: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
struct Requester {
    /// `/proc/<pid>/stat` start time: tells a reused pid from the original.
    start_time: u64,
    chain: String,
    tokens: f64,
    refilled_at: Instant,
    window_start: Instant,
    in_window: u32,
    last_rate: u32,
    total_uncached: u64,
    throttled: u64,
    last_seen: Instant,
    warned_at: Option<Instant>,
}

/// What a walker looks like from here, for logs and IPC.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WalkerStats {
    pub pid: u32,
    pub chain: String,
    pub uncached_last_min: u32,
    pub total_uncached: u64,
    pub throttled: u64,
}

pub struct WalkerTracker {
    inner: Mutex<HashMap<u32, Requester>>,
    /// Our own pid: the daemon's internal requests are never limited.
    own_pid: u32,
    enabled: bool,
}

impl WalkerTracker {
    pub fn new(enabled: bool) -> Self {
        WalkerTracker { inner: Mutex::new(HashMap::new()), own_pid: std::process::id(), enabled }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Requester>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records that `pid` caused a listing we don't have cached, and returns
    /// how long it should wait before we fetch it (zero when within budget).
    pub fn note_uncached(&self, pid: u32, now: Instant) -> Duration {
        if pid == 0 || pid == self.own_pid {
            return Duration::ZERO;
        }
        let start_time = proc_start_time(pid).unwrap_or(0);
        let mut m = self.lock();
        if m.len() >= MAX_TRACKED && !m.contains_key(&pid) {
            if let Some(oldest) = m.iter().min_by_key(|(_, r)| r.last_seen).map(|(p, _)| *p) {
                m.remove(&oldest);
            }
        }
        let r = m.entry(pid).or_insert_with(|| fresh(pid, start_time, now));
        if r.start_time != start_time {
            *r = fresh(pid, start_time, now);
        }
        r.last_seen = now;
        r.total_uncached += 1;
        if now.duration_since(r.window_start) >= Duration::from_secs(60) {
            r.last_rate = r.in_window;
            r.window_start = now;
            r.in_window = 0;
        }
        r.in_window += 1;
        let rate = r.in_window.max(r.last_rate);
        if rate > WALKER_PER_MIN && r.warned_at.is_none_or(|t| now.duration_since(t) >= WARN_EVERY) {
            r.warned_at = Some(now);
            log::warn!(
                "WALKER pid={} {} is crawling the mount: {} uncached listings in the last minute{}",
                pid, r.chain, rate,
                if self.enabled {
                    format!(" — limiting it to {}/s (cached folders stay instant)", REFILL_PER_SEC)
                } else {
                    String::new()
                }
            );
        }
        if !self.enabled {
            return Duration::ZERO;
        }
        let elapsed = now.duration_since(r.refilled_at).as_secs_f64();
        r.tokens = (r.tokens + elapsed * REFILL_PER_SEC).min(BURST);
        r.refilled_at = now;
        if r.tokens >= 1.0 {
            r.tokens -= 1.0;
            return Duration::ZERO;
        }
        // Borrow the token we're about to wait for, so concurrent requests
        // from the same walker queue up behind each other.
        let wait = Duration::from_secs_f64((1.0 - r.tokens) / REFILL_PER_SEC).min(MAX_WAIT);
        r.tokens -= 1.0;
        r.throttled += 1;
        wait
    }

    /// True when `pid` is currently crawling: it gets no speculative work
    /// (thumbnails, revalidation) on its behalf — a `find` never looks at images.
    pub fn is_walker(&self, pid: u32, now: Instant) -> bool {
        self.lock().get(&pid).is_some_and(|r| {
            now.duration_since(r.last_seen) < Duration::from_secs(60) && r.in_window.max(r.last_rate) > WALKER_PER_MIN
        })
    }

    /// Processes that crawled in the last minute, busiest first.
    pub fn active(&self, now: Instant) -> Vec<WalkerStats> {
        let m = self.lock();
        let mut v: Vec<_> = m
            .iter()
            .filter(|(_, r)| now.duration_since(r.last_seen) < Duration::from_secs(60))
            .map(|(pid, r)| WalkerStats {
                pid: *pid,
                chain: r.chain.clone(),
                uncached_last_min: r.in_window.max(r.last_rate),
                total_uncached: r.total_uncached,
                throttled: r.throttled,
            })
            .filter(|w| w.uncached_last_min > WALKER_PER_MIN)
            .collect();
        v.sort_by(|a, b| b.uncached_last_min.cmp(&a.uncached_last_min));
        v
    }
}

fn fresh(pid: u32, start_time: u64, now: Instant) -> Requester {
    Requester {
        start_time,
        chain: process_chain(pid),
        tokens: BURST,
        refilled_at: now,
        window_start: now,
        in_window: 0,
        last_rate: 0,
        total_uncached: 0,
        throttled: 0,
        last_seen: now,
        warned_at: None,
    }
}

/// `comm(pid)←comm(ppid)←…`, up to four levels, e.g. `find(812)←bash(790)←claude(655)`.
pub fn process_chain(pid: u32) -> String {
    let mut parts = Vec::new();
    let mut cur = pid;
    for _ in 0..4 {
        if cur <= 1 {
            break;
        }
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", cur)).unwrap_or_default();
        let comm = comm.trim();
        parts.push(format!("{}({})", if comm.is_empty() { "?" } else { comm }, cur));
        match proc_stat_field(cur, 4).and_then(|p| p.parse().ok()) {
            Some(ppid) => cur = ppid,
            None => break,
        }
    }
    if parts.is_empty() {
        format!("?({})", pid)
    } else {
        parts.join("←")
    }
}

fn proc_start_time(pid: u32) -> Option<u64> {
    proc_stat_field(pid, 22)?.parse().ok()
}

/// Field `n` (1-based, as in proc(5)) of `/proc/<pid>/stat`. The command name
/// (field 2) may contain spaces and parentheses, so fields are counted from
/// after its closing parenthesis.
fn proc_stat_field(pid: u32, n: usize) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(n.checked_sub(3)?).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PID: u32 = 1; // never tracked as "own"; its /proc entry always exists

    #[test]
    fn a_burst_is_free_then_the_rate_is_limited() {
        let w = WalkerTracker::new(true);
        let t = Instant::now();
        for i in 0..BURST as usize {
            assert_eq!(w.note_uncached(PID, t), Duration::ZERO, "request {i} is within the burst");
        }
        let wait = w.note_uncached(PID, t);
        assert!(wait > Duration::ZERO && wait <= MAX_WAIT, "{wait:?}");
        // After a second of refill, about REFILL_PER_SEC more are free again.
        let later = t + Duration::from_secs(2);
        let free = (0..40).take_while(|_| w.note_uncached(PID, later).is_zero()).count();
        assert!((REFILL_PER_SEC as usize..=2 * REFILL_PER_SEC as usize + 1).contains(&free), "{free}");
    }

    #[test]
    fn a_crawler_is_flagged_and_a_browser_is_not() {
        let w = WalkerTracker::new(true);
        let t = Instant::now();
        for _ in 0..=WALKER_PER_MIN {
            let _ = w.note_uncached(PID, t);
        }
        assert!(w.is_walker(PID, t));
        assert!(!w.is_walker(2, t));
        assert!(!w.is_walker(PID, t + Duration::from_secs(120)));
    }

    #[test]
    fn disabled_tracker_only_observes() {
        let w = WalkerTracker::new(false);
        let t = Instant::now();
        for _ in 0..500 {
            assert_eq!(w.note_uncached(PID, t), Duration::ZERO);
        }
        let active = w.active(t);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].uncached_last_min, 500);
        assert_eq!(active[0].throttled, 0);
    }

    #[test]
    fn kernel_and_own_requests_are_never_limited() {
        let w = WalkerTracker::new(true);
        let t = Instant::now();
        for _ in 0..500 {
            assert!(w.note_uncached(0, t).is_zero());
            assert!(w.note_uncached(std::process::id(), t).is_zero());
        }
        assert!(w.active(t).is_empty());
    }

    #[test]
    fn a_slow_reader_is_not_a_walker() {
        let w = WalkerTracker::new(true);
        let mut t = Instant::now();
        for _ in 0..200 {
            assert!(w.note_uncached(PID, t).is_zero());
            t += Duration::from_secs(2);
        }
        assert!(w.active(t).is_empty());
    }

    #[test]
    fn chain_names_the_process_and_its_parents() {
        let me = process_chain(std::process::id());
        assert!(me.contains(&format!("({})", std::process::id())), "{me}");
        assert!(me.contains('←'), "a test process has a parent: {me}");
    }

    #[test]
    fn stat_fields_survive_odd_command_names() {
        // Field 3 (state) and 4 (ppid) of our own stat parse.
        assert!(proc_stat_field(std::process::id(), 3).is_some());
        assert!(proc_stat_field(std::process::id(), 4).and_then(|p| p.parse::<u32>().ok()).is_some());
    }
}
