//! Lazy shared frame hashing for semantic screen predicates and events.

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::linux::video::{LatestFrame, RawFrame, RawPixelFormat};

const WATCH_POLL: Duration = Duration::from_millis(100);
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub screen_sequence: u64,
    pub frame_serial: u64,
    pub hash: u64,
    pub changed_at: Instant,
    pub observed_at: Instant,
}

impl ScreenSnapshot {
    pub fn hash_hex(self) -> String {
        format!("{:016x}", self.hash)
    }
}

#[derive(Default)]
struct WatchState {
    running: bool,
    sampled_consumers: usize,
    exact_consumers: usize,
    sampled: Option<ScreenSnapshot>,
    exact: Option<ScreenSnapshot>,
    error: Option<String>,
}

struct WatchInner {
    latest: Arc<LatestFrame>,
    state: Mutex<WatchState>,
    changed: Condvar,
}

#[derive(Clone)]
pub struct ScreenWatch {
    inner: Arc<WatchInner>,
}

pub struct WatchLease {
    inner: Arc<WatchInner>,
    exact: bool,
}

impl ScreenWatch {
    pub fn new(latest: Arc<LatestFrame>) -> Self {
        Self {
            inner: Arc::new(WatchInner {
                latest,
                state: Mutex::new(WatchState::default()),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn acquire(&self, exact: bool) -> io::Result<WatchLease> {
        let start = {
            let mut state = lock(&self.inner.state);
            if let Some(error) = &state.error {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
            }
            if exact {
                state.exact_consumers = state.exact_consumers.saturating_add(1);
            } else {
                state.sampled_consumers = state.sampled_consumers.saturating_add(1);
            }
            if state.running {
                false
            } else {
                state.running = true;
                true
            }
        };
        if start {
            let inner = self.inner.clone();
            if let Err(error) = thread::Builder::new()
                .name("vvland-screen-watch".into())
                .spawn(move || watch_loop(inner))
            {
                let mut state = lock(&self.inner.state);
                state.running = false;
                if exact {
                    state.exact_consumers = state.exact_consumers.saturating_sub(1);
                } else {
                    state.sampled_consumers = state.sampled_consumers.saturating_sub(1);
                }
                return Err(error);
            }
        }
        Ok(WatchLease {
            inner: self.inner.clone(),
            exact,
        })
    }

    pub fn snapshot(&self, exact: bool) -> io::Result<Option<ScreenSnapshot>> {
        let state = lock(&self.inner.state);
        if let Some(error) = &state.error {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
        }
        Ok(if exact { state.exact } else { state.sampled })
    }

    pub fn wait_change(
        &self,
        exact: bool,
        after_screen: Option<u64>,
        timeout: Duration,
    ) -> io::Result<Option<ScreenSnapshot>> {
        let _lease = self.acquire(exact)?;
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.inner.state);
        let baseline = loop {
            if let Some(error) = &state.error {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
            }
            if let Some(snapshot) = mode_snapshot(&state, exact) {
                break after_screen.unwrap_or(snapshot.screen_sequence);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            state = wait(&self.inner.changed, state, remaining);
        };
        loop {
            if let Some(error) = &state.error {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
            }
            if let Some(snapshot) = mode_snapshot(&state, exact)
                && snapshot.screen_sequence > baseline
            {
                return Ok(Some(snapshot));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            state = wait(&self.inner.changed, state, remaining);
        }
    }

    pub fn wait_stable(
        &self,
        exact: bool,
        quiet: Duration,
        after_screen: Option<u64>,
        timeout: Duration,
    ) -> io::Result<Option<ScreenSnapshot>> {
        let _lease = self.acquire(exact)?;
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.inner.state);
        loop {
            if let Some(error) = &state.error {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
            }
            if let Some(snapshot) = mode_snapshot(&state, exact) {
                let now = Instant::now();
                if stable_ready(snapshot, after_screen, quiet, now) {
                    return Ok(Some(snapshot));
                }
                let eligible = after_screen.is_none_or(|after| snapshot.screen_sequence > after);
                let wake_at = if eligible {
                    deadline.min(snapshot.changed_at + quiet)
                } else {
                    deadline
                };
                let remaining = wake_at.saturating_duration_since(now);
                if remaining.is_zero() && now >= deadline {
                    return Ok(None);
                }
                state = wait(&self.inner.changed, state, remaining);
            } else {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                state = wait(&self.inner.changed, state, remaining);
            }
        }
    }
}

impl Drop for WatchLease {
    fn drop(&mut self) {
        let mut state = lock(&self.inner.state);
        if self.exact {
            state.exact_consumers = state.exact_consumers.saturating_sub(1);
        } else {
            state.sampled_consumers = state.sampled_consumers.saturating_sub(1);
        }
        self.inner.changed.notify_all();
    }
}

fn watch_loop(inner: Arc<WatchInner>) {
    let mut serial = 0;
    loop {
        let (sampled, exact) = {
            let mut state = lock(&inner.state);
            if state.sampled_consumers == 0 && state.exact_consumers == 0 {
                state.running = false;
                inner.changed.notify_all();
                return;
            }
            (state.sampled_consumers > 0, state.exact_consumers > 0)
        };
        match inner.latest.wait_next(&mut serial, WATCH_POLL) {
            Ok(Some(frame)) => {
                let observed_at = Instant::now();
                let sampled_hash = sampled.then(|| frame_hash(&frame, false)).transpose();
                let exact_hash = exact.then(|| frame_hash(&frame, true)).transpose();
                let (sampled_hash, exact_hash) = match (sampled_hash, exact_hash) {
                    (Ok(sampled_hash), Ok(exact_hash)) => (sampled_hash, exact_hash),
                    (Err(error), _) | (_, Err(error)) => {
                        let mut state = lock(&inner.state);
                        state.error = Some(error.to_string());
                        state.running = false;
                        inner.changed.notify_all();
                        return;
                    }
                };
                let mut state = lock(&inner.state);
                if let Some(hash) = sampled_hash {
                    publish(&mut state.sampled, serial, hash, observed_at);
                }
                if let Some(hash) = exact_hash {
                    publish(&mut state.exact, serial, hash, observed_at);
                }
                inner.changed.notify_all();
            }
            Ok(None) => {}
            Err(error) => {
                let mut state = lock(&inner.state);
                state.error = Some(error.to_string());
                state.running = false;
                inner.changed.notify_all();
                return;
            }
        }
    }
}

fn publish(slot: &mut Option<ScreenSnapshot>, frame_serial: u64, hash: u64, now: Instant) {
    *slot = Some(match *slot {
        Some(previous) if previous.hash == hash => ScreenSnapshot {
            frame_serial,
            observed_at: now,
            ..previous
        },
        Some(previous) => ScreenSnapshot {
            screen_sequence: previous.screen_sequence.saturating_add(1),
            frame_serial,
            hash,
            changed_at: now,
            observed_at: now,
        },
        None => ScreenSnapshot {
            screen_sequence: 1,
            frame_serial,
            hash,
            changed_at: now,
            observed_at: now,
        },
    });
}

fn stable_ready(
    snapshot: ScreenSnapshot,
    after_screen: Option<u64>,
    quiet: Duration,
    now: Instant,
) -> bool {
    after_screen.is_none_or(|after| snapshot.screen_sequence > after)
        && now.duration_since(snapshot.changed_at) >= quiet
}

fn mode_snapshot(state: &WatchState, exact: bool) -> Option<ScreenSnapshot> {
    if exact { state.exact } else { state.sampled }
}

fn frame_hash(frame: &RawFrame, exact: bool) -> io::Result<u64> {
    let expected = usize::try_from(frame.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .and_then(|stride| {
            usize::try_from(frame.height)
                .ok()
                .and_then(|height| stride.checked_mul(height))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame size overflow"))?;
    if frame.data.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length does not match its dimensions",
        ));
    }
    let mut hash = FNV_OFFSET;
    for byte in frame
        .width
        .to_le_bytes()
        .into_iter()
        .chain(frame.height.to_le_bytes())
    {
        hash = fnv_byte(hash, byte);
    }
    hash = fnv_byte(hash, format_tag(frame.format));
    if exact {
        for &byte in frame.data.iter() {
            hash = fnv_byte(hash, byte);
        }
        return Ok(hash);
    }
    let width = usize::try_from(frame.width).unwrap_or(0);
    let height = usize::try_from(frame.height).unwrap_or(0);
    for y in (0..height).step_by(4) {
        for x in (0..width).step_by(4) {
            let start = (y * width + x) * 4;
            for &byte in &frame.data[start..start + 4] {
                hash = fnv_byte(hash, byte);
            }
        }
    }
    Ok(hash)
}

fn fnv_byte(hash: u64, byte: u8) -> u64 {
    (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
}

fn format_tag(format: RawPixelFormat) -> u8 {
    match format {
        RawPixelFormat::Bgrx => 0,
        RawPixelFormat::Rgbx => 1,
        RawPixelFormat::Bgra => 2,
        RawPixelFormat::Rgba => 3,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait<'a, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
    duration: Duration,
) -> std::sync::MutexGuard<'a, T> {
    condvar
        .wait_timeout(guard, duration)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: Vec<u8>) -> RawFrame {
        RawFrame {
            format: RawPixelFormat::Bgrx,
            width: 8,
            height: 8,
            pts_us: 0,
            data: data.into(),
        }
    }

    #[test]
    fn identical_frames_keep_sequence_and_one_sampled_pixel_changes_it() {
        let now = Instant::now();
        let mut snapshot = None;
        let first = frame(vec![0; 8 * 8 * 4]);
        publish(&mut snapshot, 1, frame_hash(&first, false).unwrap(), now);
        publish(
            &mut snapshot,
            2,
            frame_hash(&first, false).unwrap(),
            now + Duration::from_millis(1),
        );
        assert_eq!(snapshot.unwrap().screen_sequence, 1);
        assert_eq!(snapshot.unwrap().frame_serial, 2);

        let mut changed = vec![0; 8 * 8 * 4];
        changed[(4 * 8 + 4) * 4] = 1;
        publish(
            &mut snapshot,
            3,
            frame_hash(&frame(changed), false).unwrap(),
            now + Duration::from_millis(2),
        );
        assert_eq!(snapshot.unwrap().screen_sequence, 2);
    }

    #[test]
    fn exact_hash_sees_pixels_deliberately_skipped_by_sampling() {
        let first = frame(vec![0; 8 * 8 * 4]);
        let mut changed = vec![0; 8 * 8 * 4];
        changed[(8 + 1) * 4] = 1;
        let changed = frame(changed);
        assert_eq!(
            frame_hash(&first, false).unwrap(),
            frame_hash(&changed, false).unwrap()
        );
        assert_ne!(
            frame_hash(&first, true).unwrap(),
            frame_hash(&changed, true).unwrap()
        );
    }

    #[test]
    fn stability_uses_the_exact_quiet_deadline() {
        let changed_at = Instant::now();
        let snapshot = ScreenSnapshot {
            screen_sequence: 4,
            frame_serial: 9,
            hash: 1,
            changed_at,
            observed_at: changed_at,
        };
        let quiet = Duration::from_millis(250);
        assert!(!stable_ready(
            snapshot,
            Some(3),
            quiet,
            changed_at + quiet - Duration::from_nanos(1)
        ));
        assert!(stable_ready(snapshot, Some(3), quiet, changed_at + quiet));
        assert!(!stable_ready(snapshot, Some(4), quiet, changed_at + quiet));
    }
}
