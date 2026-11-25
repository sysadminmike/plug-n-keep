//! The app's log. [`Logger`] receives every record sent through the `log` facade and keeps the
//! last [`CAPACITY`] lines in memory, for the Log panel (`log_view.rs`). Nothing is written to
//! disk.
//!
//! - Lines from our own crates (`pnk_core`, `plug_n_keep`) are kept at or above the
//!   recording level, the `log_level` setting.
//! - Lines from the libraries underneath (eframe, egui, winit, glow…) are kept only as warnings
//!   and errors.
//!
//! Every kept record passes through [`Logger::log`], so a file sink would be one more consumer
//! there, next to the ring.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// The most lines the log keeps. The oldest go first.
pub const CAPACITY: usize = 100_000;

/// While the Log panel is open, new lines ask for a repaint at most this often.
pub const REPAINT_EVERY: Duration = Duration::from_millis(250);

const OUR_CRATES: [&str; 2] = ["pnk_core", "plug_n_keep"];

/// Sources that need no allocation: our modules, and the libraries most likely to log.
const KNOWN_SOURCES: &[&str] = &[
    "main",
    "core",
    "app",
    "worker",
    "slow",
    "logging",
    "log_view",
    "theme",
    "format",
    "sync",
    "plan",
    "portable",
    "platform",
    "windows",
    "linux",
    "freebsd",
    "unix",
    "state",
    "owner",
    "paths",
    "device",
    "fake",
    "fs",
    "eframe",
    "egui",
    "egui_glow",
    "egui_winit",
    "egui_extras",
    "winit",
    "glow",
    "accesskit_windows",
    "accesskit_winit",
    "rfd",
];

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Counts up from 0 over the app's run and is never reused, so a view can tell which lines
    /// are new and which have gone.
    pub seq: u64,
    /// Local time of day, in milliseconds since midnight.
    pub time_ms: u32,
    pub level: Level,
    /// Where it came from: one of our modules (`sync`, `plan`, `wpd`, `worker`, `app`…) or
    /// another crate (`eframe`, `winit`…).
    pub source: Cow<'static, str>,
    /// One line: line breaks in the record become spaces.
    pub message: String,
}

impl Entry {
    /// `HH:MM:SS.mmm`
    pub fn time(&self) -> String {
        let t = self.time_ms;
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            t / 3_600_000,
            t / 60_000 % 60,
            t / 1000 % 60,
            t % 1000
        )
    }

    /// The line as plain text, as Copy puts it on the clipboard.
    pub fn line(&self) -> String {
        format!(
            "{}  {:<5}  {:<7}  {}",
            self.time(),
            self.level,
            self.source,
            self.message
        )
    }
}

/// The last `capacity` lines, oldest first.
pub struct Ring {
    entries: VecDeque<Entry>,
    capacity: usize,
    next_seq: u64,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            next_seq: 0,
        }
    }

    pub fn push(&mut self, time_ms: u32, level: Level, source: Cow<'static, str>, message: String) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            seq: self.next_seq,
            time_ms,
            level,
            source,
            message,
        });
        self.next_seq += 1;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The sequence number of the oldest line kept (`next_seq` when there are none).
    pub fn first_seq(&self) -> u64 {
        self.next_seq - self.entries.len() as u64
    }

    /// The sequence number the next line will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn get(&self, seq: u64) -> Option<&Entry> {
        let i = usize::try_from(seq.checked_sub(self.first_seq())?).ok()?;
        self.entries.get(i)
    }

    /// The lines from `seq` on, oldest first.
    pub fn since(&self, seq: u64) -> impl Iterator<Item = &Entry> {
        let skip = seq.saturating_sub(self.first_seq());
        let skip = usize::try_from(skip).map_or(self.entries.len(), |s| s.min(self.entries.len()));
        self.entries.range(skip..)
    }

    /// Drop every line. Sequence numbers carry on from where they were.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Which levels a view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    Errors,
    Warnings,
    Info,
    Everything,
}

impl Show {
    pub const ALL: [Show; 4] = [Show::Errors, Show::Warnings, Show::Info, Show::Everything];

    pub fn label(self) -> &'static str {
        match self {
            Show::Errors => "Errors",
            Show::Warnings => "Warnings and up",
            Show::Info => "Info and up",
            Show::Everything => "Everything",
        }
    }

    pub fn allows(self, level: Level) -> bool {
        match self {
            Show::Errors => level <= Level::Error,
            Show::Warnings => level <= Level::Warn,
            Show::Info => level <= Level::Info,
            Show::Everything => true,
        }
    }
}

/// What [`Filtered::update`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Update {
    /// The filter changed, so every line was looked at again.
    pub rebuilt: bool,
    /// Lines that match, added at the newest end since the last update.
    pub added: usize,
}

/// The lines a view shows, as sequence numbers, oldest first. [`Filtered::update`] only looks at
/// lines added since the last update, unless the filter changed; lines the ring has dropped are
/// dropped from the front.
pub struct Filtered {
    show: Show,
    /// Lower case.
    needle: String,
    seqs: VecDeque<u64>,
    /// Every line before this one has been looked at.
    scanned: u64,
    stale: bool,
}

impl Filtered {
    pub fn new(show: Show) -> Self {
        Self {
            show,
            needle: String::new(),
            seqs: VecDeque::new(),
            scanned: 0,
            stale: true,
        }
    }

    /// Takes effect at the next [`Filtered::update`].
    pub fn set(&mut self, show: Show, text: &str) {
        let needle = text.trim().to_lowercase();
        if show != self.show || needle != self.needle {
            self.show = show;
            self.needle = needle;
            self.stale = true;
        }
    }

    pub fn update(&mut self, ring: &Ring) -> Update {
        let first = ring.first_seq();
        let mut out = Update::default();
        if self.stale {
            self.seqs.clear();
            self.scanned = first;
            self.stale = false;
            out.rebuilt = true;
        }
        while self.seqs.front().is_some_and(|&s| s < first) {
            self.seqs.pop_front();
        }
        for e in ring.since(self.scanned.max(first)) {
            if self.matches(e) {
                self.seqs.push_back(e.seq);
                out.added += 1;
            }
        }
        self.scanned = ring.next_seq();
        out
    }

    pub fn matches(&self, e: &Entry) -> bool {
        self.show.allows(e.level)
            && (contains_folded(&e.message, &self.needle)
                || contains_folded(&e.source, &self.needle))
    }

    pub fn len(&self) -> usize {
        self.seqs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seqs.is_empty()
    }

    /// The `row`th line shown, counting from the newest.
    pub fn newest(&self, row: usize) -> Option<u64> {
        let i = self.seqs.len().checked_sub(row + 1)?;
        self.seqs.get(i).copied()
    }

    /// The lines shown, newest first.
    pub fn newest_first(&self) -> impl Iterator<Item = u64> + '_ {
        self.seqs.iter().rev().copied()
    }
}

/// Whether `hay` contains `needle` (already lower case), ignoring case. No allocation when the
/// needle is ASCII, which keeps refiltering 100,000 lines quick.
fn contains_folded(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        n.len() <= h.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

/// `target` is one of our crates or a module in one.
fn is_ours(target: &str) -> bool {
    OUR_CRATES.iter().any(|c| {
        target
            .strip_prefix(c)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// Whether a record is kept: our crates' at or above `recording`, other crates' only as warnings
/// and errors.
pub fn keeps(recording: LevelFilter, target: &str, level: Level) -> bool {
    if is_ours(target) {
        level <= recording
    } else {
        level <= Level::Warn
    }
}

/// `pnk_core::sync` -> `sync`, `plug_n_keep` (main.rs) -> `main`,
/// `winit::platform_impl::windows` -> `winit`.
pub fn short_source(target: &str) -> Cow<'static, str> {
    let name = if is_ours(target) {
        match target.rsplit("::").next().unwrap_or(target) {
            "plug_n_keep" => "main",
            "pnk_core" => "core",
            last => last,
        }
    } else {
        target.split("::").next().unwrap_or(target)
    };
    match KNOWN_SOURCES.iter().find(|k| **k == name) {
        Some(k) => Cow::Borrowed(k),
        None => Cow::Owned(name.to_string()),
    }
}

fn filter_from(n: usize) -> LevelFilter {
    match n {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

pub struct Logger {
    ring: Mutex<Ring>,
    /// The recording level for our crates, as a `LevelFilter` number.
    level: AtomicUsize,
    /// The Log panel is open: new lines should get it repainted.
    watching: AtomicBool,
    waker: OnceLock<Box<dyn Fn() + Send + Sync>>,
    clock: Instant,
    /// Milliseconds since `clock`, plus 1, of the last wake-up; 0 for none yet.
    last_wake: AtomicU64,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Install the app's logger, recording our crates' lines at `level` and above. Call once, at
/// start-up.
pub fn install(level: LevelFilter) -> &'static Logger {
    let logger = LOGGER.get_or_init(|| Logger::new(CAPACITY, level));
    if log::set_logger(logger).is_err() {
        eprintln!("Another logger was installed first; the Log panel will stay empty.");
    }
    logger.set_level(level);
    logger
}

impl Logger {
    pub fn new(capacity: usize, level: LevelFilter) -> Self {
        Self {
            ring: Mutex::new(Ring::new(capacity)),
            level: AtomicUsize::new(level as usize),
            watching: AtomicBool::new(false),
            waker: OnceLock::new(),
            clock: Instant::now(),
            last_wake: AtomicU64::new(0),
        }
    }

    /// Record our crates' lines at `level` and above from now on.
    pub fn set_level(&self, level: LevelFilter) {
        self.level.store(level as usize, Ordering::Relaxed);
        // The `log` macros skip formatting anything above the global maximum.
        if LOGGER.get().is_some_and(|l| std::ptr::eq(l, self)) {
            log::set_max_level(level.max(LevelFilter::Warn));
        }
    }

    pub fn level(&self) -> LevelFilter {
        filter_from(self.level.load(Ordering::Relaxed))
    }

    /// The lines. Hold the guard briefly, and never across egui calls: a library may log while
    /// egui runs, and that would wait for the guard.
    pub fn lines(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What to call for a repaint when new lines arrive while [`Logger::set_watching`] is on.
    pub fn set_waker(&self, wake: impl Fn() + Send + Sync + 'static) {
        let _ = self.waker.set(Box::new(wake));
    }

    pub fn set_watching(&self, on: bool) {
        self.watching.store(on, Ordering::Relaxed);
    }

    /// Ask for a repaint, at most every [`REPAINT_EVERY`]. A line that comes in between is
    /// covered by the repaint already asked for.
    fn wake(&self) {
        if !self.watching.load(Ordering::Relaxed) {
            return;
        }
        let Some(wake) = self.waker.get() else {
            return;
        };
        let now = self.clock.elapsed().as_millis() as u64 + 1;
        let last = self.last_wake.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < REPAINT_EVERY.as_millis() as u64 {
            return;
        }
        if self
            .last_wake
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            wake();
        }
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        keeps(self.level(), metadata.target(), metadata.level())
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Formatted before taking the lock.
        let mut message = record.args().to_string();
        if message.contains(['\r', '\n']) {
            message = message
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
        }
        let source = short_source(record.target());
        let time_ms = pnk_core::local_time_ms();
        self.lines().push(time_ms, record.level(), source, message);
        // Only our own lines wake the window: a library might log while it holds egui's lock,
        // and asking egui for a repaint then would wait for that lock forever.
        if is_ours(record.target()) {
            self.wake();
        }
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn push(ring: &mut Ring, level: Level, source: &'static str, message: &str) {
        ring.push(0, level, Cow::Borrowed(source), message.to_string());
    }

    fn messages<'a>(ring: &'a Ring, f: &Filtered) -> Vec<&'a str> {
        f.newest_first()
            .map(|s| ring.get(s).expect("shown line is kept").message.as_str())
            .collect()
    }

    fn record(logger: &Logger, target: &str, level: Level, message: &str) {
        logger.log(
            &Record::builder()
                .target(target)
                .level(level)
                .args(format_args!("{message}"))
                .build(),
        );
    }

    #[test]
    fn the_ring_keeps_the_newest_lines_in_order_with_their_numbers() {
        let mut ring = Ring::new(3);
        assert_eq!((ring.len(), ring.first_seq(), ring.next_seq()), (0, 0, 0));
        for i in 0..5 {
            push(&mut ring, Level::Info, "sync", &format!("line {i}"));
        }
        assert_eq!(ring.len(), 3);
        assert_eq!((ring.first_seq(), ring.next_seq()), (2, 5));
        assert!(ring.get(1).is_none(), "evicted");
        assert!(ring.get(5).is_none(), "not written yet");
        assert_eq!(ring.get(2).unwrap().message, "line 2");
        let all: Vec<(u64, &str)> = ring.since(0).map(|e| (e.seq, e.message.as_str())).collect();
        assert_eq!(all, [(2, "line 2"), (3, "line 3"), (4, "line 4")]);
        let tail: Vec<u64> = ring.since(4).map(|e| e.seq).collect();
        assert_eq!(tail, [4]);
        assert_eq!(ring.since(9).count(), 0);
        // Clearing drops the lines but not the numbering.
        ring.clear();
        assert_eq!((ring.len(), ring.first_seq(), ring.next_seq()), (0, 5, 5));
        push(&mut ring, Level::Warn, "plan", "after");
        assert_eq!(ring.get(5).unwrap().message, "after");
    }

    #[test]
    fn the_full_size_ring_drops_the_oldest_line_at_100000() {
        let mut ring = Ring::new(CAPACITY);
        for i in 0..CAPACITY as u64 + 7 {
            ring.push(0, Level::Debug, Cow::Borrowed("plan"), i.to_string());
        }
        assert_eq!(ring.len(), 100_000);
        assert_eq!(ring.first_seq(), 7);
        assert_eq!(ring.get(7).unwrap().message, "7");
        assert_eq!(ring.get(100_006).unwrap().message, "100006");
    }

    #[test]
    fn our_lines_follow_the_recording_level_and_others_only_warn_and_error() {
        let info = LevelFilter::Info;
        assert!(keeps(info, "pnk_core::sync", Level::Info));
        assert!(!keeps(info, "pnk_core::sync", Level::Debug));
        assert!(keeps(LevelFilter::Debug, "pnk_core::plan", Level::Debug));
        assert!(keeps(info, "plug_n_keep", Level::Error));
        assert!(keeps(info, "plug_n_keep::worker", Level::Info));
        assert!(!keeps(LevelFilter::Error, "plug_n_keep::app", Level::Warn));
        assert!(!keeps(
            LevelFilter::Debug,
            "pnk_core::platform::windows::portable",
            Level::Trace
        ));
        // Other crates: warnings and errors whatever the setting.
        for recording in [LevelFilter::Error, LevelFilter::Debug] {
            assert!(keeps(recording, "eframe::native::run", Level::Warn));
            assert!(keeps(recording, "winit", Level::Error));
            assert!(!keeps(recording, "egui_glow::painter", Level::Info));
            assert!(!keeps(recording, "glow", Level::Debug));
        }
        // A name that only starts like ours isn't ours.
        assert!(!keeps(LevelFilter::Debug, "pnk_core_extra", Level::Info));
        assert!(!keeps(LevelFilter::Debug, "plug_n_keep2::x", Level::Info));
    }

    #[test]
    fn sources_are_short() {
        assert_eq!(short_source("pnk_core::sync"), "sync");
        assert_eq!(short_source("pnk_core"), "core");
        assert_eq!(short_source("plug_n_keep"), "main");
        assert_eq!(short_source("plug_n_keep::worker::slow"), "slow");
        assert_eq!(short_source("winit::platform_impl::windows"), "winit");
        assert_eq!(short_source("somecrate::deep"), "somecrate");
        assert!(matches!(
            short_source("pnk_core::platform::windows::portable"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn the_logger_records_by_level_and_applies_a_new_level_at_once() {
        let logger = Logger::new(100, LevelFilter::Info);
        record(&logger, "pnk_core::sync", Level::Info, "copied");
        record(&logger, "pnk_core::plan", Level::Debug, "classified");
        record(&logger, "eframe::native", Level::Info, "frame");
        record(&logger, "eframe::native", Level::Warn, "slow frame");
        logger.set_level(LevelFilter::Debug);
        assert_eq!(logger.level(), LevelFilter::Debug);
        record(&logger, "pnk_core::plan", Level::Debug, "new: a.jpg");
        record(
            &logger,
            "plug_n_keep::app",
            Level::Error,
            "two\r\n  lines\n",
        );
        logger.set_level(LevelFilter::Error);
        record(&logger, "plug_n_keep::worker", Level::Warn, "dropped");
        let ring = logger.lines();
        let got: Vec<(Level, &str, &str)> = ring
            .since(0)
            .map(|e| (e.level, e.source.as_ref(), e.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Level::Info, "sync", "copied"),
                (Level::Warn, "eframe", "slow frame"),
                (Level::Debug, "plan", "new: a.jpg"),
                (Level::Error, "app", "two lines"),
            ]
        );
    }

    #[test]
    fn new_lines_wake_the_window_only_while_watched_and_not_too_often() {
        let logger = Logger::new(100, LevelFilter::Info);
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        logger.set_waker(move || {
            w.fetch_add(1, Ordering::Relaxed);
        });
        record(&logger, "pnk_core::sync", Level::Info, "not watched");
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        logger.set_watching(true);
        for _ in 0..50 {
            record(&logger, "pnk_core::sync", Level::Info, "watched");
        }
        assert_eq!(wakes.load(Ordering::Relaxed), 1, "throttled");
        // Other crates' lines never wake it (they may come from inside egui).
        logger.last_wake.store(0, Ordering::Relaxed);
        record(&logger, "egui::context", Level::Warn, "from egui");
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_filtered_view_follows_new_lines_and_evictions_without_rescanning() {
        let mut ring = Ring::new(4);
        let mut f = Filtered::new(Show::Everything);
        assert_eq!(
            f.update(&ring),
            Update {
                rebuilt: true,
                added: 0
            }
        );
        push(&mut ring, Level::Info, "sync", "a");
        push(&mut ring, Level::Debug, "plan", "b");
        assert_eq!(
            f.update(&ring),
            Update {
                rebuilt: false,
                added: 2
            }
        );
        assert_eq!(messages(&ring, &f), ["b", "a"], "newest first");
        assert_eq!(f.update(&ring), Update::default(), "nothing new");
        // Three more: the ring (4) drops "a"; the view drops it too.
        push(&mut ring, Level::Warn, "fs", "c");
        push(&mut ring, Level::Error, "sync", "d");
        push(&mut ring, Level::Info, "app", "e");
        assert_eq!(f.update(&ring).added, 3);
        assert_eq!(messages(&ring, &f), ["e", "d", "c", "b"]);
        assert_eq!(
            (f.newest(0), f.newest(3), f.newest(4)),
            (Some(4), Some(1), None)
        );
        // A new filter looks at every line again.
        f.set(Show::Warnings, "");
        let u = f.update(&ring);
        assert!(u.rebuilt);
        assert_eq!(messages(&ring, &f), ["d", "c"]);
        // The same filter again changes nothing.
        f.set(Show::Warnings, "  ");
        assert!(!f.update(&ring).rebuilt);
        // Text: case-insensitive, in the message or the source.
        f.set(Show::Everything, "SYNC");
        f.update(&ring);
        assert_eq!(messages(&ring, &f), ["d"]);
        f.set(Show::Everything, "E");
        f.update(&ring);
        assert_eq!(messages(&ring, &f), ["e"]);
        // Clearing the ring empties the view at the next update.
        ring.clear();
        assert_eq!(f.update(&ring).added, 0);
        assert!(f.is_empty());
        push(&mut ring, Level::Info, "app", "Every line");
        assert_eq!(f.update(&ring).added, 1);
        assert_eq!(messages(&ring, &f), ["Every line"]);
    }

    #[test]
    fn levels_shown() {
        assert!(Show::Errors.allows(Level::Error));
        assert!(!Show::Errors.allows(Level::Warn));
        assert!(Show::Warnings.allows(Level::Warn) && !Show::Warnings.allows(Level::Info));
        assert!(Show::Info.allows(Level::Info) && !Show::Info.allows(Level::Debug));
        assert!(Show::Everything.allows(Level::Debug));
    }

    #[test]
    fn text_matching_ignores_case() {
        assert!(contains_folded("Copied 202611_a/IMG_1.HEIC", "img_1.heic"));
        assert!(contains_folded("anything", ""));
        assert!(!contains_folded("ab", "abc"));
        assert!(contains_folded("Fichier ÉTÉ.jpg", "été"));
    }

    #[test]
    fn a_line_as_text() {
        let e = Entry {
            seq: 0,
            time_ms: ((13 * 60 + 5) * 60 + 9) * 1000 + 42,
            level: Level::Warn,
            source: Cow::Borrowed("sync"),
            message: "Skipped a.jpg".into(),
        };
        assert_eq!(e.time(), "13:05:09.042");
        assert_eq!(e.line(), "13:05:09.042  WARN   sync     Skipped a.jpg");
    }
}
