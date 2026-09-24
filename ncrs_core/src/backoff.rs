//! Backing off from a server that answers badly.
//!
//! Two layers, both about *answers* (HTTP 5xx/429), not outages — an unreachable
//! server is the connectivity monitor's job and flips the mount offline:
//!
//! * [`PathBackoff`] remembers, per directory, that the last listing failed and
//!   refuses to ask again until a growing cooldown passes (5 s, doubling to
//!   5 min, jittered). A directory that 500s deterministically — Nextcloud does
//!   this for some invalid filenames — then costs one request per cooldown
//!   instead of one per `readdir`/`lookup`.
//! * [`ServerBreaker`] watches the answers across all paths. When the server
//!   is failing a large share of requests it opens for 30–120 s, and while it
//!   is open background work (revalidation, TTL refreshes, prefetch, thumbnails)
//!   is skipped so foreground requests get the server's remaining capacity. It
//!   never sets the offline flag.
//!
//! During the 2026-09-24 incident a `find /` produced ~20 listings/s, the
//! server started answering 500, and every failure was retried and
//! re-requested on the next access; nothing here existed.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BASE_COOLDOWN: Duration = Duration::from_secs(5);
const MAX_COOLDOWN: Duration = Duration::from_secs(300);
/// Entries kept before expired ones are pruned; bounds memory under a walk.
const PRUNE_AT: usize = 4096;

/// ±20% jitter so thousands of directories that failed together don't retry together.
fn jitter(d: Duration, salt: impl Hash) -> Duration {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    salt.hash(&mut h);
    let unit = (h.finish() % 1000) as f64 / 1000.0; // [0, 1)
    d.mul_f64(0.8 + 0.4 * unit)
}

#[derive(Debug, Clone, Copy)]
struct Failure {
    until: Instant,
    failures: u32,
    code: u16,
}

/// Per-path cooldown after a failed listing. Cleared by a success or by a
/// change notification for the path.
#[derive(Default)]
pub struct PathBackoff {
    inner: Mutex<HashMap<PathBuf, Failure>>,
}

impl PathBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Failure>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a failed request for `path` with HTTP status `code` and returns
    /// how long it is now blocked for.
    pub fn record_failure(&self, path: &Path, code: u16, now: Instant) -> Duration {
        let mut m = self.lock();
        if m.len() >= PRUNE_AT {
            m.retain(|_, f| f.until > now);
        }
        let failures = m.get(path).map_or(0, |f| f.failures).saturating_add(1);
        let base = BASE_COOLDOWN.saturating_mul(1u32 << (failures - 1).min(10)).min(MAX_COOLDOWN);
        let cooldown = jitter(base, (path, failures));
        m.insert(path.to_path_buf(), Failure { until: now + cooldown, failures, code });
        cooldown
    }

    pub fn clear(&self, path: &Path) {
        self.lock().remove(path);
    }

    /// `Some((remaining, last_code))` while `path` is cooling down.
    pub fn blocked(&self, path: &Path, now: Instant) -> Option<(Duration, u16)> {
        let m = self.lock();
        let f = m.get(path)?;
        (f.until > now).then(|| (f.until - now, f.code))
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Sliding-window view of how the server has been answering.
const WINDOW: Duration = Duration::from_secs(30);
/// Open after this many server errors in [`WINDOW`]…
const TRIP_ERRORS: usize = 20;
/// …or when at least this share of ≥ [`TRIP_MIN_SAMPLES`] answers were errors.
const TRIP_RATIO: f64 = 0.5;
const TRIP_MIN_SAMPLES: usize = 10;
const OPEN_BASE: Duration = Duration::from_secs(30);
const OPEN_MAX: Duration = Duration::from_secs(120);
/// Samples kept; older ones fall out even inside the window.
const MAX_SAMPLES: usize = 512;

struct BreakerState {
    samples: VecDeque<(Instant, bool)>,
    open_until: Option<Instant>,
    /// How many times it opened without a clean window in between: grows the open time.
    consecutive_trips: u32,
    trips_total: u64,
}

pub struct ServerBreaker {
    state: Mutex<BreakerState>,
}

impl Default for ServerBreaker {
    fn default() -> Self {
        ServerBreaker {
            state: Mutex::new(BreakerState {
                samples: VecDeque::new(),
                open_until: None,
                consecutive_trips: 0,
                trips_total: 0,
            }),
        }
    }
}

/// What the breaker's latest decision was, for logs and IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BreakerStats {
    pub open: bool,
    pub open_for_ms: u64,
    pub errors_in_window: usize,
    pub samples_in_window: usize,
    pub trips_total: u64,
}

impl ServerBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BreakerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records one answer: `server_error` is a 5xx/429.
    pub fn record(&self, server_error: bool, now: Instant) {
        let mut st = self.lock();
        st.samples.push_back((now, server_error));
        while st.samples.len() > MAX_SAMPLES || st.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > WINDOW) {
            st.samples.pop_front();
        }
        if st.open_until.is_some_and(|t| t > now) {
            return;
        }
        let errors = st.samples.iter().filter(|(_, e)| *e).count();
        let n = st.samples.len();
        let trip = errors >= TRIP_ERRORS || (n >= TRIP_MIN_SAMPLES && errors as f64 / n as f64 >= TRIP_RATIO);
        if trip {
            let base = OPEN_BASE.saturating_mul(1u32 << st.consecutive_trips.min(4)).min(OPEN_MAX);
            let open_for = jitter(base, st.trips_total);
            st.open_until = Some(now + open_for);
            st.consecutive_trips += 1;
            st.trips_total += 1;
            // Start the next window clean: the answers that tripped it are spent.
            st.samples.clear();
            log::warn!(
                "SERVER_BREAKER open for {:?}: {} of {} answers in the last {:?} were server errors — pausing background listing work",
                open_for, errors, n, WINDOW
            );
        } else if !server_error && errors == 0 && n >= TRIP_MIN_SAMPLES {
            st.consecutive_trips = 0;
        }
    }

    /// True while background work should stand down.
    pub fn is_open(&self, now: Instant) -> bool {
        let mut st = self.lock();
        match st.open_until {
            Some(t) if t > now => true,
            Some(_) => {
                st.open_until = None;
                log::info!("SERVER_BREAKER closed — resuming background listing work");
                false
            }
            None => false,
        }
    }

    pub fn stats(&self, now: Instant) -> BreakerStats {
        let st = self.lock();
        let in_window: Vec<_> = st.samples.iter().filter(|(t, _)| now.duration_since(*t) <= WINDOW).collect();
        BreakerStats {
            open: st.open_until.is_some_and(|t| t > now),
            open_for_ms: st.open_until.map_or(0, |t| t.saturating_duration_since(now).as_millis() as u64),
            errors_in_window: in_window.iter().filter(|(_, e)| *e).count(),
            samples_in_window: in_window.len(),
            trips_total: st.trips_total,
        }
    }
}

/// True for the HTTP statuses both layers treat as "the server is struggling".
pub fn is_struggling(code: u16) -> bool {
    code == 429 || (500..=599).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_cooldown_grows_and_caps() {
        let b = PathBackoff::new();
        let p = Path::new("/Music/a");
        let t = Instant::now();
        let mut last = Duration::ZERO;
        for i in 0..12 {
            let d = b.record_failure(p, 500, t);
            assert!(d <= MAX_COOLDOWN.mul_f64(1.2), "round {i}: {d:?}");
            if i < 5 {
                assert!(d > last.mul_f64(1.2), "round {i}: {d:?} did not grow from {last:?}");
            }
            last = d;
        }
        assert!(last >= MAX_COOLDOWN.mul_f64(0.8));
    }

    #[test]
    fn path_is_blocked_until_the_cooldown_passes_and_clear_unblocks() {
        let b = PathBackoff::new();
        let p = Path::new("/x");
        let t = Instant::now();
        let d = b.record_failure(p, 503, t);
        assert_eq!(b.blocked(p, t).map(|(_, c)| c), Some(503));
        assert!(b.blocked(p, t + d + Duration::from_millis(1)).is_none());
        b.record_failure(p, 500, t);
        b.clear(p);
        assert!(b.blocked(p, t).is_none());
        assert!(b.blocked(Path::new("/other"), t).is_none());
    }

    #[test]
    fn path_backoff_prunes_expired_entries() {
        let b = PathBackoff::new();
        let t = Instant::now();
        for i in 0..PRUNE_AT {
            b.record_failure(&PathBuf::from(format!("/d{i}")), 500, t);
        }
        b.record_failure(Path::new("/late"), 500, t + MAX_COOLDOWN * 2);
        assert!(b.len() < PRUNE_AT);
    }

    #[test]
    fn breaker_opens_on_an_error_storm_and_closes_after() {
        let br = ServerBreaker::new();
        let t = Instant::now();
        // Enough healthy answers first that the ratio rule can't trip before the count rule.
        for _ in 0..(TRIP_ERRORS + 5) {
            br.record(false, t);
        }
        for i in 0..TRIP_ERRORS {
            assert!(!br.is_open(t), "opened early at {i}");
            br.record(true, t);
        }
        assert!(br.is_open(t));
        assert!(!br.is_open(t + OPEN_MAX.mul_f64(1.3)));
    }

    #[test]
    fn breaker_opens_on_a_high_error_ratio() {
        let br = ServerBreaker::new();
        let t = Instant::now();
        for i in 0..TRIP_MIN_SAMPLES {
            br.record(i % 2 == 0, t);
        }
        assert!(br.is_open(t));
    }

    #[test]
    fn breaker_ignores_healthy_traffic_and_old_errors() {
        let br = ServerBreaker::new();
        let t = Instant::now();
        // Just under both trip rules: 19 errors among 45 answers.
        for _ in 0..(TRIP_ERRORS + 6) {
            br.record(false, t);
        }
        for _ in 0..(TRIP_ERRORS - 1) {
            br.record(true, t);
        }
        assert!(!br.is_open(t));
        // Errors age out of the window before the next ones arrive.
        let later = t + WINDOW + Duration::from_secs(1);
        for _ in 0..200 {
            br.record(false, later);
        }
        br.record(true, later);
        assert!(!br.is_open(later));
        assert_eq!(br.stats(later).errors_in_window, 1);
    }

    #[test]
    fn repeated_trips_stay_open_longer_up_to_the_cap() {
        let br = ServerBreaker::new();
        let mut t = Instant::now();
        let mut open_for = Vec::new();
        for _ in 0..6 {
            for _ in 0..TRIP_ERRORS {
                br.record(true, t);
            }
            let s = br.stats(t);
            assert!(s.open);
            open_for.push(s.open_for_ms);
            t += Duration::from_millis(s.open_for_ms + 1);
            assert!(!br.is_open(t));
        }
        assert!(open_for.iter().all(|&ms| ms as f64 <= OPEN_MAX.as_millis() as f64 * 1.2));
        assert!(open_for[2] as f64 > open_for[0] as f64 * 1.5);
    }

    #[test]
    fn struggling_statuses() {
        assert!(is_struggling(500) && is_struggling(503) && is_struggling(429));
        assert!(!is_struggling(404) && !is_struggling(401) && !is_struggling(207));
    }
}
