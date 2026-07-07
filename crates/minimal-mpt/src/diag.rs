//! Centralized diagnostics: the crate's `MMPT_*` env-gated timing switches and
//! their readers, gathered in one place instead of scattered across the state,
//! rotation, and root modules. Each reader preserves its original semantics
//! byte-for-byte: the `*_TIMING` boolean switches that were `OnceLock`-cached
//! stay cached; the ones read fresh per call stay uncached.

use std::sync::OnceLock;

/// Cache "is `name` present in the environment?" in `cache`, reading the
/// environment at most once. Shared backing for the cached boolean switches.
fn cached_flag(cache: &'static OnceLock<bool>, name: &str) -> bool {
    *cache.get_or_init(|| std::env::var_os(name).is_some())
}

/// `MMPT_MERGE_TIMING=1`: enable the in-place snapshot-merge `[merge]`
/// instrumentation in `State::advance_after_commit`. Cached (read once).
pub(crate) fn merge_timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    cached_flag(&ON, "MMPT_MERGE_TIMING")
}

/// `MMPT_DELTA_TIMING=1`: enable the incremental delta-root `[delta-root]`
/// accumulator in `State::commit`. Cached (read once).
pub(crate) fn delta_timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    cached_flag(&ON, "MMPT_DELTA_TIMING")
}

/// `MMPT_RECOVER_TIMING=1`: enable the cold-start `[recover]` timing prints on
/// the load / rebuild paths. Read fresh on each call (matching the original
/// per-call `var_os`), so intentionally NOT cached.
pub(crate) fn recover_timing() -> bool {
    std::env::var_os("MMPT_RECOVER_TIMING").is_some()
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
