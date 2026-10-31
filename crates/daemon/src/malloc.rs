//! glibc malloc arena bound for the daemon process.
//!
//! The daemon's tokio blocking pool and worker threads allocate and free on
//! many threads, and glibc gives each busy thread its own arena (up to eight
//! per core). Under the allocation churn of a daemon with hundreds of live
//! sessions the arenas fragment and keep their pages: 400 live workers held
//! 650-740 MB of RSS against a ~110 MB peak live heap, and 183 MB with two
//! arenas (<https://github.com/zajca/pohunek/issues/1672>). The bound is set
//! once, before any thread exists, so every later thread shares the capped
//! set of arenas.
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
    /// `mallopt(M_ARENA_MAX, limit)` succeeded.
    Applied { limit: i32 },
    /// The operator set the limit through the environment; it is left alone.
    OperatorOverride,
    /// `mallopt` refused the value; glibc keeps its default.
    Rejected { limit: i32 },
    /// This platform has no glibc arenas.
    Unsupported,
}

static OUTCOME: OnceLock<ArenaBound> = OnceLock::new();

/// Bounds the glibc malloc arenas unless the operator already chose a limit.
///
/// Must be the first call of `main`, before the async runtime or any other
/// thread exists: arenas already handed to running threads stay in use.
pub fn bound_arenas() -> ArenaBound {
    *OUTCOME.get_or_init(|| {
        apply(
            std::env::var_os(OPERATOR_ARENA_VARIABLE)
                .is_some_and(|value| is_positive_integer(&value)),
            std::env::var_os(GLIBC_TUNABLES_VARIABLE)
                .is_some_and(|tunables| tunables.to_string_lossy().contains(ARENA_TUNABLE)),
        )
    })
}

/// Whether glibc takes `value` as an arena limit: it ignores an empty,
/// non-numeric or zero `MALLOC_ARENA_MAX`.
fn is_positive_integer(value: &std::ffi::OsStr) -> bool {
    value
        .to_str()
        .and_then(|text| text.parse::<u32>().ok())
        .is_some_and(|limit| limit > 0)
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
    // SAFETY: `mallopt` is MT-safe and only stores a tunable in the allocator.
    // Running before any other thread exists is what makes the limit apply to
    // every thread.
    #[expect(unsafe_code, reason = "glibc mallopt FFI")]
    let accepted = unsafe { libc::mallopt(libc::M_ARENA_MAX, MALLOC_ARENA_MAX) } == 1;
    if accepted {
        ArenaBound::Applied {
            limit: MALLOC_ARENA_MAX,
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
