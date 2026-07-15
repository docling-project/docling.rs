//! Lightweight, env-gated per-stage timing for profiling the PDF pipeline.
//!
//! Set `DOCLING_RS_TIMING=1` to accumulate wall-clock time per named stage
//! across all pages and print a sorted breakdown at process exit. Zero cost
//! when the env var is unset (the `Instant` is still taken but nothing is
//! recorded — call sites guard the hot path themselves where it matters).

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("DOCLING_RS_TIMING").is_ok())
}

fn store() -> &'static Mutex<BTreeMap<&'static str, (u128, u64)>> {
    static S: OnceLock<Mutex<BTreeMap<&'static str, (u128, u64)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Record `elapsed` nanoseconds against `stage`. No-op unless timing is enabled.
pub fn record(stage: &'static str, nanos: u128) {
    if !enabled() {
        return;
    }
    let mut g = store().lock().unwrap();
    let e = g.entry(stage).or_insert((0, 0));
    e.0 += nanos;
    e.1 += 1;
}

/// Time a closure and attribute its wall-clock to `stage`.
pub fn timed<T>(stage: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let t = Instant::now();
    let out = f();
    record(stage, t.elapsed().as_nanos());
    out
}

/// Print the accumulated breakdown (descending by total time). Call once at the
/// end of a conversion. No-op unless timing is enabled.
pub fn report() {
    if !enabled() {
        return;
    }
    let g = store().lock().unwrap();
    let mut rows: Vec<_> = g.iter().map(|(k, v)| (*k, v.0, v.1)).collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    let total: u128 = rows.iter().map(|r| r.1).sum();
    eprintln!("=== DOCLING_RS timing (per stage, wall-clock) ===");
    for (stage, nanos, calls) in &rows {
        let ms = *nanos as f64 / 1e6;
        let pct = if total > 0 {
            *nanos as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        eprintln!("  {stage:<22} {ms:>9.1} ms  {pct:>5.1}%  ({calls} calls)");
    }
    eprintln!("  {:<22} {:>9.1} ms", "TOTAL (summed)", total as f64 / 1e6);
}
