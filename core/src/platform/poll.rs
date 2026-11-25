//! What the plug-in watchers share: the handle that stops a watcher, the debounce that turns a
//! burst of events into one wake-up, and the slow poll every system keeps as a fallback.
//!
//! A watcher only says "something changed"; the caller lists the devices again to see what.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What a watcher calls when the devices may have changed. Called from the watcher's own thread.
pub type Wake = Box<dyn Fn() + Send + Sync + 'static>;

/// A running watcher. Dropping it stops it (the thread ends within about a second).
pub struct Watcher {
    stop: Arc<AtomicBool>,
    /// Extra things to release on drop, such as a Windows notification registration.
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl Watcher {
    pub(crate) fn new(stop: Arc<AtomicBool>, on_drop: Option<Box<dyn FnOnce() + Send>>) -> Self {
        Self { stop, on_drop }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

/// How long events must stop arriving before the wake-up (a stick raises several in a row).
pub const QUIET: Duration = Duration::from_millis(600);

/// Collects events and says when a burst has ended: `quiet` after the last one.
#[derive(Debug, Default)]
pub struct Debounce {
    last: Option<Instant>,
}

impl Debounce {
    pub fn event(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// `true` once, when `quiet` has passed since the last event.
    pub fn due(&mut self, now: Instant, quiet: Duration) -> bool {
        match self.last {
            Some(t) if now.duration_since(t) >= quiet => {
                self.last = None;
                true
            }
            _ => false,
        }
    }

    pub fn pending(&self) -> bool {
        self.last.is_some()
    }
}

/// Whether two snapshots (device or mount identifiers) differ, ignoring their order.
pub fn changed(before: &[String], now: &[String]) -> bool {
    let a: BTreeSet<&String> = before.iter().collect();
    let b: BTreeSet<&String> = now.iter().collect();
    a != b
}

/// Looks at `snapshot` every `every` and calls `wake` when it differs from the last one. The
/// first snapshot is only remembered. For systems with no event source, and as a backstop.
pub fn spawn_poll(
    name: &str,
    every: Duration,
    mut snapshot: impl FnMut() -> Vec<String> + Send + 'static,
    wake: Wake,
) -> Result<Watcher, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut last = snapshot();
            while !flag.load(Ordering::Relaxed) {
                // Short sleeps, so a stop is noticed quickly.
                let until = Instant::now() + every;
                while Instant::now() < until && !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let now = snapshot();
                if changed(&last, &now) {
                    last = now;
                    wake();
                }
            }
        })
        .map_err(|e| format!("could not start the device watcher: {e}"))?;
    Ok(Watcher::new(stop, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn snapshots_differ_only_when_their_contents_do() {
        assert!(!changed(&v(&["a", "b"]), &v(&["b", "a"])));
        assert!(changed(&v(&["a"]), &v(&["a", "b"])));
        assert!(changed(&v(&["a", "b"]), &v(&["a"])));
        assert!(!changed(&[], &[]));
    }

    #[test]
    fn a_burst_of_events_gives_one_wake_up_after_it_goes_quiet() {
        let mut d = Debounce::default();
        let t0 = Instant::now();
        assert!(!d.due(t0, QUIET), "nothing happened");
        d.event(t0);
        d.event(t0 + Duration::from_millis(300));
        assert!(!d.due(t0 + Duration::from_millis(700), QUIET));
        assert!(d.due(t0 + Duration::from_millis(900), QUIET));
        assert!(!d.due(t0 + Duration::from_secs(5), QUIET), "only once");
        assert!(!d.pending());
    }

    #[test]
    fn the_poll_wakes_on_a_change_and_stops_when_dropped() {
        let n = Arc::new(AtomicUsize::new(0));
        let counter = n.clone();
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        let watcher = spawn_poll(
            "test-poll",
            Duration::from_millis(100),
            move || {
                let i = counter.fetch_add(1, Ordering::SeqCst);
                if i < 2 { v(&["a"]) } else { v(&["a", "b"]) }
            },
            Box::new(move || {
                w.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while wakes.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        drop(watcher);
    }
}
