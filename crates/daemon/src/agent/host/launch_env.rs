//! A runtime descriptor's `[launch_env]`: literal environment pairs every
//! launch and version probe of the runtime gets.
//!
//! ```toml
//! [launch_env]
//! ACME_NO_AUTO_UPDATE = "1"
//! ```
//!
//! The pairs are behaviour flags: a value is one of `0`, `1`, `true` or
//! `false`, never expanded, so a pair says what the agent does and cannot name
//! where credentials, configuration, data, the install or a socket live, which
//! program to run or which session it is. Widening the value set needs a new
//! feature id.
//! A name is accepted only inside the runtime's own namespace, derived from its
//! runtime id (`acme` gives `ACME_`, `my-agent.x` gives `MY_AGENT_X_`) with at
//! least one character after the prefix. The core's own allowlist outside the
//! namespace is empty. A runtime whose namespace collides with a variable family
//! of the loader, the owner's tools or the platform (`SSH_`, `AWS_`, `GIT_`,
//! `HTTPS_`, ...) has no usable `[launch_env]`. Inside the namespace the
//! service-manager names, the descriptor's own config home variables,
//! any secret-shaped word and any location, identity or code-shaped word
//! (`_DIR`, `_CMD`, `_OPTS`, `_PRELOAD`, ...) are refused. The word lists are a
//! second layer behind the value rule (`0` and `1` are also valid file names),
//! not a complete vocabulary.
//! A host profile `[env]` is owner-authored and not subject to these rules; it
//! overrides a pair of the same name. The values reach only the declaring
//! runtime's own launches and probes, and nothing persists them, so each launch
//! reads them from the definition that serves it. The trust model stays
//! same-UID (see "Runtime packages and trust"): this guards against shadowing
//! or redirecting owner or core state, it is not a sandbox.

// Rust guideline compliant 2026-10-27

use std::collections::BTreeMap;

use pohunek_worker_protocol::is_denylisted;

use super::definition::DefinitionError;
use crate::agent::env_name_problem;

/// Maximum number of `[launch_env]` entries.
///
/// A launch variable switches one behavior of the agent; more entries are a
/// descriptor that configures the agent wholesale.
pub const MAX_LAUNCH_ENV_ENTRIES: usize = 16;

/// The only values a `[launch_env]` pair can hold: flag literals. A value
/// outside this set could name a program, a path, a host or a credential, which
/// no list of names can rule out; widening the set needs a new feature id.
pub const LAUNCH_ENV_VALUES: &[&str] = &["0", "1", "true", "false"];

/// Separator that ends a runtime namespace.
const NAMESPACE_SEPARATOR: char = '_';

/// Namespace prefixes no runtime can use: a runtime id that maps into one of
/// these would name variables of the loader, the owner's tools, or an
/// interpreter, package manager, privilege or network tool the agent runs, whose
/// variables execute code or change trust (`BASH_ENV`, `NODE_OPTIONS`,
/// `SUDO_ASKPASS`, `PYTHONSTARTUP`, `CURL_CA_BUNDLE`). The list only grows.
pub const REFUSED_NAMESPACES: &[&str] = &[
    // The core, the loader and the platform.
    "POHUNEK_",
    "LD_",
    "DYLD_",
    "XDG_",
    "LC_",
    "SYSTEMD_",
    "DBUS_",
    "WAYLAND_",
    "DISPLAY_",
    // The owner's credentials, remotes and clouds.
    "SSH_",
    "GPG_",
    "GNUPG",
    "AWS_",
    "AZURE_",
    "GOOGLE_",
    "GCLOUD_",
    "CLOUDSDK_",
    "OCI_",
    "ALIBABA_",
    "ALICLOUD_",
    "DIGITALOCEAN_",
    "HCLOUD_",
    "TERRAFORM_",
    "TF_",
    "VAULT_",
    "ANSIBLE_",
    "GIT_",
    "GH_",
    "GITHUB_",
    "GITLAB_",
    "DOCKER_",
    "KUBE",
    "HELM_",
    "PODMAN_",
    "BUILDAH_",
    "CONTAINER_",
    "CONTAINERS_",
    "CONTAINERD_",
    "COMPOSE_",
    // Network clients and TLS.
    "HTTP_",
    "HTTPS_",
    "FTP_",
    "ALL_",
    "NO_",
    "SOCKS",
    "CURL_",
    "WGET_",
    "REQUESTS_",
    "SSL_",
    "OPENSSL_",
    "GNUTLS_",
    "NSS_",
    "SSLKEYLOGFILE",
    // Shells and privilege.
    "BASH_",
    "ZSH_",
    "SH_",
    "ZDOTDIR",
    "SUDO_",
    // Interpreters and package managers.
    "NODE_",
    "NPM_",
    "YARN_",
    "PNPM_",
    "JAVA_",
    "JDK_",
    "PYTHON",
    "PIP_",
    "PERL",
    "RUBY",
    "GEM_",
    "CARGO_",
    "RUST",
    "GO_",
    "GOFLAGS",
    "GOPATH",
    "GOROOT",
    "GOPROXY",
    "GONOSUMDB",
    "GONOPROXY",
    "GOSUMDB",
    "GOINSECURE",
    "GOPRIVATE",
    "GOTOOLCHAIN",
    "GOCACHE",
    "GOMODCACHE",
    "GOENV",
    "GOWORK",
    "CGO_",
    "LUA_",
    "DOTNET_",
    "NUGET_",
    "MSBUILD",
    "PHP_",
    "PHPRC",
    "COMPOSER_",
    "BUNDLE_",
    "CONDA_",
    "UV_",
    "BUN_",
    "DENO_",
    "GRADLE_",
    "MAVEN_",
    // The C library and the system allocator.
    "GLIBC_",
    "MALLOC_",
    // Version and environment managers that rewrite the toolchain on PATH.
    "NIX_",
    "GUIX_",
    "HOMEBREW_",
    "PYENV_",
    "RBENV_",
    "NVM_",
    "VOLTA_",
    "ASDF_",
    "MISE_",
    // Tools whose boolean variables change trust or behaviour.
    "RSYNC_",
    "CCACHE_",
    "SCCACHE_",
    "CMAKE_",
    "ELECTRON_",
    "SVN_",
    "BORG_",
    "RESTIC_",
    "HEX_",
    "R_",
];

/// Words of secret-shaped names, which only the owner's profile sets.
pub const SECRET_WORDS: &[&str] = &[
    "KEY",
    "KEYS",
    "TOKEN",
    "TOKENS",
    "SECRET",
    "SECRETS",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "PASSPHRASES",
    "CREDENTIAL",
    "CREDENTIALS",
    "AUTH",
];

/// Words of location-, identity- and code-shaped names: where something
/// lives, which program, library, plugin or hook to run, with which options, or
/// which session this is is never a behaviour flag.
pub const LOCATION_WORDS: &[&str] = &[
    "PATH",
    "PATHS",
    "DIR",
    "DIRS",
    "DIRECTORY",
    "FOLDER",
    "HOME",
    "ROOT",
    "FILE",
    "FILES",
    "SOCK",
    "SOCKET",
    "SOCKETS",
    "URL",
    "URLS",
    "URI",
    "HOST",
    "HOSTS",
    "SERVER",
    "ADDR",
    "ENDPOINT",
    "REGISTRY",
    "PROXY",
    "COMMAND",
    "CMD",
    "BIN",
    "BINARY",
    "EXECUTABLE",
    "SCRIPT",
    "OPTS",
    "OPTIONS",
    "ARGS",
    "FLAGS",
    "EXEC",
    "SHELL",
    "EDITOR",
    "HOOK",
    "HOOKS",
    "PLUGIN",
    "PLUGINS",
    "MODULES",
    "LIB",
    "LIBS",
    "PRELOAD",
    "CERT",
    "CERTS",
    "CA",
    "KEYRING",
    "PROFILE",
    "CONF",
    "CONFIG",
    "ID",
];

/// Endings of a longer word that make it location- or code-shaped when the
/// separator is missing (`TMPDIR`, `CAFILE`, `LIBPATH`, `AFLAGS`). Short words
/// such as `ID` or `CA` are matched whole only, so `VALID` is not touched.
pub const GLUED_ENDINGS: &[&str] = &[
    "PATH", "DIR", "FILE", "HOME", "ROOT", "FLAGS", "OPTS", "CONF", "CERT", "CERTS",
];

/// Whether `word` is a location word, or a longer word ending in one of
/// `GLUED_ENDINGS`.
fn location_shaped(word: &str) -> bool {
    LOCATION_WORDS.contains(&word)
        || GLUED_ENDINGS
            .iter()
            .any(|ending| word.len() > ending.len() && word.ends_with(ending))
}

/// The validated `[launch_env]` pairs of a definition, ordered by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescriptorLaunchEnv(Vec<(String, String)>);

/// The namespace of `runtime_id`: upper case, `-` and `.` mapped to `_`, ending
/// in `_`; `None` when it is not a usable variable-name prefix or is one of
/// [`REFUSED_NAMESPACES`].
fn namespace_of(runtime_id: &str) -> Option<String> {
    let mut namespace: String = runtime_id
        .chars()
        .map(|c| match c {
            '-' | '.' => NAMESPACE_SEPARATOR,
            other => other.to_ascii_uppercase(),
        })
        .collect();
    namespace.push(NAMESPACE_SEPARATOR);
    let usable = env_name_problem(&format!("{namespace}X")).is_none();
    let colliding = REFUSED_NAMESPACES
        .iter()
        .any(|refused| namespace.starts_with(refused));
    (usable && !colliding).then_some(namespace)
}

impl DescriptorLaunchEnv {
    /// Validates the pairs `declared` by the runtime `runtime_id`.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming `launch_env`, never a name
    /// or a value, for more than [`MAX_LAUNCH_ENV_ENTRIES`] entries, a runtime
    /// whose namespace is unusable or refused, a name outside the namespace or
    /// with nothing after it, a name that is not an upper-case variable outside
    /// the `POHUNEK_` namespace, a service-manager, secret-shaped or
    /// location-shaped name, or a value outside [`LAUNCH_ENV_VALUES`].
    pub fn new(
        runtime_id: &str,
        declared: BTreeMap<String, String>,
    ) -> Result<Self, DefinitionError> {
        let field = |reason| DefinitionError::Field {
            field: "launch_env",
            reason,
        };
        if declared.len() > MAX_LAUNCH_ENV_ENTRIES {
            return Err(field("has too many entries"));
        }
        if declared.is_empty() {
            return Ok(Self::default());
        }
        let namespace = namespace_of(runtime_id)
            .ok_or_else(|| field("is not available to a runtime with this id"))?;
        for (name, value) in &declared {
            let Some(rest) = name
                .strip_prefix(&namespace)
                .filter(|rest| !rest.is_empty())
            else {
                return Err(field("names must start with the runtime's own namespace"));
            };
            if let Some(reason) = env_name_problem(name) {
                return Err(field(reason));
            }
            if is_denylisted(name) {
                return Err(field("cannot name a service-manager variable"));
            }
            if rest
                .split(NAMESPACE_SEPARATOR)
                .any(|word| SECRET_WORDS.contains(&word))
            {
                return Err(field("cannot name a secret-shaped variable"));
            }
            if rest.split(NAMESPACE_SEPARATOR).any(location_shaped) {
                return Err(field("cannot name a location or identity variable"));
            }
            if !LAUNCH_ENV_VALUES.contains(&value.as_str()) {
                return Err(field("values must be one of 0, 1, true or false"));
            }
        }
        Ok(Self(declared.into_iter().collect()))
    }

    /// Refuses a pair that names one of `owned`, the variables the same
    /// descriptor reads its config home or conversation root from: the launch
    /// would give the agent a home the daemon's own resolution never sees.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] naming `launch_env`.
    pub fn refuse_owned<'a>(
        &self,
        owned: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), DefinitionError> {
        let owned: Vec<&str> = owned.into_iter().collect();
        if self
            .0
            .iter()
            .any(|(name, _)| owned.contains(&name.as_str()))
        {
            return Err(DefinitionError::Field {
                field: "launch_env",
                reason: "cannot name a config home or conversation root variable",
            });
        }
        Ok(())
    }

    /// The pairs in name order.
    #[must_use]
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.0
    }

    /// Whether the descriptor declares no pair.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NAME entry` lines of every frozen list, in table order.
    fn current() -> Vec<String> {
        let lists: [(&str, &[&str]); 4] = [
            ("REFUSED_NAMESPACES", REFUSED_NAMESPACES),
            ("SECRET_WORDS", SECRET_WORDS),
            ("LOCATION_WORDS", LOCATION_WORDS),
            ("GLUED_ENDINGS", GLUED_ENDINGS),
        ];
        lists
            .into_iter()
            .flat_map(|(name, entries)| entries.iter().map(move |entry| format!("{name} {entry}")))
            .collect()
    }

    #[test]
    fn the_frozen_lists_equal_the_snapshot() {
        let snapshot: Vec<&str> = include_str!("fixtures/launch_env_lists.txt")
            .lines()
            .collect();
        assert_eq!(
            current(),
            snapshot,
            "a list entry is removed, renamed or added without an issue naming the variable; \
             edit fixtures/launch_env_lists.txt only to append an entry"
        );
    }
}
