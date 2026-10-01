//! Resolves the executable search path of a supervised installation.
//!
//! Processes started by a service manager or the desktop shell (launchd, a
//! Finder-launched app) carry a minimal `PATH`, so the agents a user installed
//! with Homebrew, `cargo`, `bun`, or `~/.local/bin` are invisible to them. This
//! module owns the single policy that decides which directories those
//! processes search. It is target-neutral: only its callers decide whether the
//! login-shell tier applies (Darwin), so every tier is testable on any Unix.
//!
//! # Policy
//!
//! Resolution order, highest priority first:
//!
//! 1. **A configured absolute executable.** A program name containing a `/`
//!    must be absolute and is used as is; no search happens
//!    ([`resolve_executable`]).
//! 2. **An explicitly supplied environment `PATH`.** A validated
//!    [`SearchPath`] passed by the caller (a GUI launched from a shell hands
//!    over its inherited one) is authoritative: no discovery runs. The
//!    installer supplies none.
//! 3. **Bounded login-shell discovery** ([`discover_login_shell_path`]). One
//!    non-interactive login shell (`$SHELL -l -c`, never `-i`) prints `PATH`
//!    between random sentinels. It runs with a null stdin, one hard deadline
//!    over the whole discovery, a process-group kill, and an output bound, and
//!    only when the caller supplies a [`LoginShellSpec`]. A login shell reads
//!    profile files but not interactive ones (`.zshrc`), so the trusted
//!    fallback directories it lacks are appended after the discovered ones.
//! 4. **A fixed fallback directory list** ([`DARWIN_FALLBACK_DIRECTORIES`]),
//!    used alone when tier 3 is disabled or fails. The failure is reported in
//!    [`PathResolution::login_shell_failure`] instead of being hidden.
//!
//! Every tier yields a [`SearchPath`]: absolute, normalized, control-character
//! free, deduplicated directories. Discovery and the fallback list keep only
//! trusted directories ([`trusted_directory`]): existing, owned by the user or
//! root, and not writable by group or others along the whole canonical path.
//! A shell is never used to run commands; the only shell invocation is the
//! discovery above, as the installing user.
//!
//! # Examples
//!
//! ```
//! use pohunek_platform::shell_env::{
//!     resolve_search_path, PathPolicy, PathSource, DARWIN_FALLBACK_DIRECTORIES,
//! };
//!
//! let resolution = resolve_search_path(&PathPolicy {
//!     configured: None,
//!     login_shell: None,
//!     fallback_directories: DARWIN_FALLBACK_DIRECTORIES,
//!     home: None,
//! })?;
//! assert_eq!(resolution.source, PathSource::FallbackDirectories);
//! # Ok::<(), pohunek_platform::shell_env::ResolveError>(())
//! ```

// Rust guideline compliant 2026-09-30

mod executable;
mod login_shell;
mod policy;
mod search_path;
#[cfg(test)]
mod test_support;

#[doc(inline)]
pub use executable::{
    is_trusted_executable_file, resolve_executable, resolve_executable_in_path_value,
    ExecutableError,
};
#[doc(inline)]
pub use login_shell::{
    discover_login_shell_path, LoginShellDiscovery, LoginShellError, LoginShellSpec,
    DEFAULT_LOGIN_SHELL, LOGIN_SHELL_OUTPUT, LOGIN_SHELL_TIMEOUT, PRINTENV_EXECUTABLE,
    PROBE_BASELINE_PATH,
};
#[doc(inline)]
pub use policy::{resolve_search_path, PathPolicy, PathResolution, PathSource, ResolveError};
#[doc(inline)]
pub use search_path::{
    fallback_search_path, trusted_directory, validate_search_directory, DroppedEntry,
    SanitizedPath, SearchPath, SearchPathError, TrustError, DARWIN_FALLBACK_DIRECTORIES,
    MAX_SEARCH_PATH_BYTES,
};
