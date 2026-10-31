//! glibc malloc arena bound for the daemon process.
//!
//! The daemon's tokio blocking pool and worker threads allocate and free on
//! many threads, and glibc gives each busy thread its own arena (up to eight
//! per core). Under the allocation churn of a daemon with hundreds of live
//! sessions the arenas fragment and keep their pages: with the store churn
//! removed, 400 live workers still ended at 327 MB of RSS against a 97 MB peak
//! live heap, and at 97 MB with two arenas
//! (<https://github.com/zajca/pohunek/issues/1672>). The bound is set once,
//! before any thread exists, so every later thread shares the capped set of
//! arenas.
//!
//! Only glibc has `mallopt` and arenas: musl's allocator has none and macOS
//! libmalloc has no such knob, so the bound is compiled out there.

use std::sync::OnceLock;

/// Number of malloc arenas the daemon allows (platform constant, not a
/// `service.toml` setting). See <https://github.com/zajca/pohunek/issues/1672>.
pub const MALLOC_ARENA_MAX: i32 = 2;

/// Environment variable glibc reads for the arena limit; an operator value
/// wins over [`MALLOC_ARENA_MAX`].
pub const OPERATOR_ARENA_VARIABLE: &str = "MALLOC_ARENA_MAX";

/// Environment variable of glibc tunables; `glibc.malloc.arena_max` in it is
/// an operator choice as well.
const GLIBC_TUNABLES_VARIABLE: &str = "GLIBC_TUNABLES";

/// Tunable name of the arena limit inside [`GLIBC_TUNABLES_VARIABLE`].
const ARENA_TUNABLE: &str = "glibc.malloc.arena_max";

/// What [`bound_arenas`] did at process start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaBound {
    /// `mallopt(M_ARENA_MAX, limit)` succeeded while `threads` threads existed
    /// (`None` when the count is unreadable).
    Applied { limit: i32, threads: Option<usize> },
    /// The operator's environment names the limit; `mallopt` is not called.
    OperatorOverride,
    /// `mallopt` refused the value; glibc keeps its default.
    Rejected { limit: i32 },
    /// This platform has no glibc arenas.
    Unsupported,
}

static OUTCOME: OnceLock<ArenaBound> = OnceLock::new();

/// Bounds the glibc malloc arenas unless the operator's environment names the limit.
///
/// The presence of `MALLOC_ARENA_MAX`, or of `glibc.malloc.arena_max` in
/// `GLIBC_TUNABLES`, is taken as the operator's intent; the value is not parsed,
/// so glibc alone decides whether it takes effect.
///
/// # Safety
///
/// No other thread may exist: `mallopt` is MT-Unsafe in glibc, and arenas
/// already handed to running threads stay in use. Call it as the first
/// statement of `main`, before the async runtime is built.
#[expect(
    unsafe_code,
    reason = "the allocator call needs a single-threaded process"
)]
pub unsafe fn bound_arenas() -> ArenaBound {
    *OUTCOME.get_or_init(|| {
        apply(
            std::env::var_os(OPERATOR_ARENA_VARIABLE).is_some(),
            std::env::var_os(GLIBC_TUNABLES_VARIABLE)
                .is_some_and(|tunables| tunables.to_string_lossy().contains(ARENA_TUNABLE)),
        )
    })
}

/// The outcome recorded by [`bound_arenas`], for the startup log line.
pub fn recorded_bound() -> Option<ArenaBound> {
    OUTCOME.get().copied()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn apply(operator_variable: bool, operator_tunable: bool) -> ArenaBound {
    if operator_variable || operator_tunable {
        return ArenaBound::OperatorOverride;
    }
    // SAFETY: `mallopt` takes integers only, and the caller of `bound_arenas`
    // guarantees that no other thread allocates or reads the allocator
    // parameters concurrently.
    #[expect(unsafe_code, reason = "glibc mallopt FFI")]
    let accepted = unsafe { libc::mallopt(libc::M_ARENA_MAX, MALLOC_ARENA_MAX) } == 1;
    if accepted {
        ArenaBound::Applied {
            limit: MALLOC_ARENA_MAX,
            threads: std::fs::read_dir("/proc/self/task")
                .ok()
                .map(Iterator::count),
        }
    } else {
        ArenaBound::Rejected {
            limit: MALLOC_ARENA_MAX,
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn apply(_operator_variable: bool, _operator_tunable: bool) -> ArenaBound {
    ArenaBound::Unsupported
}
