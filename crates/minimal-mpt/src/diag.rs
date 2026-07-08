//! Centralized diagnostics: the crate's `MMPT_*` env switches and its two
//! timing primitives, split by what a measurement means:
//!
//! - **Per-event** ([`event`]): rare occurrences where every single one is
//!   individually meaningful — cold-start recovery phases (once per process),
//!   the boundary merge (once per snapshot period, on an ever-growing trie).
//!   Each occurrence prints its own `[mmpt]` line. Gated by `MMPT_TIMING=1`.
//! - **Aggregate** ([`profile`]): hot-path operations that run millions of
//!   times, where a single sample is meaningless and only counts / totals /
//!   averages inform — reads, writes, prefix scans, the per-commit delta root.
//!   Accumulated in static counters and dumped cumulatively at each snapshot
//!   boundary. Gated by `MMPT_PROFILE=1`.
//!
//! Both are guards, so the measured statement stays untouched:
//! `let t = diag::event(..); work; t.finish(format_args!(..))`, and
//! `let _t = profile::op(..); work` (records on drop, covering early returns).

use std::sync::OnceLock;
use std::time::Instant;

/// Cache "is `name` present in the environment?" in `cache`, reading the
/// environment at most once. Shared backing for the boolean switches.
fn cached_flag(cache: &'static OnceLock<bool>, name: &str) -> bool {
    *cache.get_or_init(|| std::env::var_os(name).is_some())
}

/// `MMPT_TIMING=1`: print the per-event timing lines. Cached (read once).
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    cached_flag(&ON, "MMPT_TIMING")
}

/// `MMPT_SHARD_TIMING=1`: print the sharded cold rebuild's per-probe lines
/// (see [`shard_probe`]). Cached (read once).
fn shard_timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    cached_flag(&ON, "MMPT_SHARD_TIMING")
}

/// `MMPT_BULK_W`: batch width `W` for the dense snapshot bulk-load. Read fresh
/// and parsed as a positive `usize`; defaults to 1_000_000 when unset, empty,
/// unparseable, or zero.
pub(crate) fn bulk_w() -> usize {
    std::env::var("MMPT_BULK_W")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&w| w > 0)
        .unwrap_or(1_000_000)
}

/// Start a per-event timer for a rare, individually-meaningful occurrence.
/// End it with [`EventTimer::finish`]. Events are rare by definition, so the
/// clock is read unconditionally.
pub(crate) fn event(name: &'static str) -> EventTimer {
    EventTimer {
        name,
        start: Instant::now(),
        on: timing(),
    }
}

/// Per-event timer for one materialization probe of the sharded cold rebuild
/// (up to `SHARD_CAP + 1` entries each). Only **overflow** probes finish it —
/// each line is a `cap + 1` fill that got discarded into a dispatch, i.e. the
/// scheme's pure waste (depth / entry count / fill time). Same shape as
/// [`event`] but gated by `MMPT_SHARD_TIMING=1`: per-probe lines are noisier
/// than the once-per-phase events, so they get their own switch.
pub(crate) fn shard_probe() -> EventTimer {
    EventTimer {
        name: "shard.probe",
        start: Instant::now(),
        on: shard_timing(),
    }
}

pub(crate) struct EventTimer {
    name: &'static str,
    start: Instant,
    /// Whether this event's switch was on at construction; `finish` prints
    /// only when set. Decided by the constructor ([`event`] / [`shard_probe`]).
    on: bool,
}

impl EventTimer {
    /// Finish the event: if its switch is on, print
    /// `[mmpt] <name> <detail> elapsed=<ms>ms`, where `detail` carries the
    /// event's `key=value` fields.
    pub(crate) fn finish(self, detail: std::fmt::Arguments<'_>) {
        if self.on {
            eprintln!(
                "[mmpt] {} {} elapsed={}ms",
                self.name,
                detail,
                self.start.elapsed().as_millis()
            );
        }
    }
}

/// Env-gated (`MMPT_PROFILE=1`) per-operation count+time+entries accumulators,
/// so a replay run reports where backend time goes (read / write / scan /
/// merge / delta-root) and per-op cost. Zero cost when disabled: [`op`] is one
/// cached-bool branch and never reads the clock. Categories are cumulative;
/// [`dump`]ed per snapshot period from `State::commit`.
pub(crate) mod profile {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::OnceLock;
    use std::time::Instant;

    /// Operation categories. The order below is the dump row order — kept
    /// stable so op-profile columns stay comparable across runs.
    #[derive(Clone, Copy)]
    pub(crate) enum Op {
        /// `get()`, whole call.
        Read = 0,
        /// The snapshot lookup within a `get()` that missed delta+intermediate.
        SnapGet = 1,
        /// `set()`.
        Write = 2,
        /// The boundary snapshot merge (entries = intermediate size absorbed).
        Merge = 3,
        /// A prefix read/delete (entries = result size).
        Scan = 4,
        /// The incremental delta root per commit (entries = delta size).
        DeltaRoot = 5,
    }
    const N: usize = 6;
    const NAMES: [&str; N] = ["read", "snap_get", "write", "merge", "scan", "delta_root"];

    static NS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
    static CNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
    static ENT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

    pub(crate) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        super::cached_flag(&ON, "MMPT_PROFILE")
    }

    /// Guard for one operation: created at the start, records elapsed +
    /// `entries` on drop, so every exit path (including early returns) is
    /// covered. `entries` defaults to 1; overwrite it where the op's size is
    /// only known later (e.g. a scan's result length).
    pub(crate) struct OpTimer {
        op: Op,
        pub(crate) entries: u64,
        start: Option<Instant>,
    }

    #[inline]
    pub(crate) fn op(op: Op) -> OpTimer {
        OpTimer {
            op,
            entries: 1,
            start: on().then(Instant::now),
        }
    }

    impl Drop for OpTimer {
        #[inline]
        fn drop(&mut self) {
            if let Some(start) = self.start {
                let i = self.op as usize;
                NS[i].fetch_add(start.elapsed().as_nanos() as u64, Relaxed);
                CNT[i].fetch_add(1, Relaxed);
                ENT[i].fetch_add(self.entries, Relaxed);
            }
        }
    }

    /// Print the cumulative per-category table.
    pub(crate) fn dump(tag: &str) {
        eprintln!("[profile] {tag}");
        for i in 0..N {
            let (ns, cnt, ent) = (
                NS[i].load(Relaxed),
                CNT[i].load(Relaxed),
                ENT[i].load(Relaxed),
            );
            if cnt > 0 {
                eprintln!(
                    "  {:<10} calls={:<11} entries={:<11} total={:>7}ms  per_call={:>6}ns  per_entry={:>5}ns",
                    NAMES[i],
                    cnt,
                    ent,
                    ns / 1_000_000,
                    ns / cnt,
                    if ent > 0 { ns / ent } else { 0 }
                );
            }
        }
    }
}
