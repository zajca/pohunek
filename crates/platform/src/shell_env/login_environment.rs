//! The environment a login-shell `PATH` probe starts from.
//!
//! The probe runs from an empty environment, so everything a startup profile
//! may branch on has to be passed explicitly: the user identity variables, the
//! variables that relocate a shell's startup files, and `$SHELL` itself. The
//! CLI's service installer and other executable resolvers build that
//! environment here, so they validate it by the same rules.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::login_shell::DEFAULT_LOGIN_SHELL;

/// Variables naming the user, which a shell needs to find its startup files.
const IDENTITY_VARIABLES: [&str; 3] = ["HOME", "USER", "LOGNAME"];

/// Variables that name where a login shell reads its profile from.
///
/// `ZDOTDIR` moves zsh's startup files and `XDG_CONFIG_HOME` moves fish's and
/// other shells'; without them a custom profile location is never read and the
/// probe would succeed with the baseline `PATH`.
const PROFILE_SELECTORS: [&str; 2] = ["ZDOTDIR", "XDG_CONFIG_HOME"];

/// Reports why the probe environment cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LoginEnvironmentError {
    /// A variable is set but not UTF-8; passing it on lossily would make the
    /// shell read another user's startup files.
    #[error("{var} is not valid UTF-8")]
    NonUtf8 {
        /// The variable.
        var: &'static str,
        /// The rejected value.
        value: PathBuf,
    },
    /// A variable that must name an absolute path does not.
    #[error("{var} is not an absolute path")]
    NotAbsolute {
        /// The variable.
        var: &'static str,
        /// The rejected value.
        value: PathBuf,
    },
    /// A value holds a NUL byte, which no process environment can carry.
    #[error("{var} contains a NUL byte")]
    NulByte {
        /// The variable.
        var: &'static str,
    },
}

/// The validated inputs of one login-shell probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginEnvironment {
    /// Absolute path of the login shell.
    pub shell: PathBuf,
    /// Whether `shell` is the platform default because `$SHELL` was unset.
    pub shell_defaulted: bool,
    /// The variables the probe starts with, `SHELL` last.
    pub variables: Vec<(String, String)>,
}

fn text(var: &'static str, value: OsString) -> Result<String, LoginEnvironmentError> {
    let text = value
        .into_string()
        .map_err(|value| LoginEnvironmentError::NonUtf8 {
            var,
            value: PathBuf::from(value),
        })?;
    if text.contains('\0') {
        return Err(LoginEnvironmentError::NulByte { var });
    }
    Ok(text)
}

/// Builds the probe environment from a variable lookup.
///
/// `HOME`, `USER`, and `LOGNAME` are passed when set and must be UTF-8. A set,
/// non-empty `ZDOTDIR` or `XDG_CONFIG_HOME` is passed and must be UTF-8 and
/// absolute (an empty one counts as unset, as shells treat it). `$SHELL` must
/// be UTF-8 and absolute; when unset, [`DEFAULT_LOGIN_SHELL`] is used and
/// [`LoginEnvironment::shell_defaulted`] says so. The shell is passed on as
/// `SHELL` too, so a profile that branches on it still sets its `PATH`.
/// The lookup is injected so the validation is testable on every target.
///
/// # Examples
///
/// ```
/// use pohunek_platform::shell_env::login_environment;
///
/// let env = login_environment(|name| (name == "HOME").then(|| "/home/u".into()))?;
/// assert!(env.shell_defaulted);
/// assert_eq!(env.variables[0], ("HOME".to_owned(), "/home/u".to_owned()));
/// # Ok::<(), pohunek_platform::shell_env::LoginEnvironmentError>(())
/// ```
///
/// # Errors
///
/// Returns [`LoginEnvironmentError`] for a value that is not UTF-8, holds a NUL
/// byte, or is not absolute where it must be.
pub fn login_environment(
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<LoginEnvironment, LoginEnvironmentError> {
    let mut variables = Vec::new();
    for name in IDENTITY_VARIABLES {
        if let Some(value) = lookup(name) {
            variables.push((name.to_owned(), text(name, value)?));
        }
    }
    for name in PROFILE_SELECTORS {
        let Some(value) = lookup(name).filter(|value| !value.is_empty()) else {
            continue;
        };
        let value = text(name, value)?;
        if !Path::new(&value).is_absolute() {
            return Err(LoginEnvironmentError::NotAbsolute {
                var: name,
                value: PathBuf::from(value),
            });
        }
        variables.push((name.to_owned(), value));
    }
    login_environment_with(lookup("SHELL").map(PathBuf::from), variables)
}

/// Validates `shell` and appends it to `variables` as `SHELL`.
///
/// # Errors
///
/// Returns [`LoginEnvironmentError`] for a `shell` that is not UTF-8, holds a
/// NUL byte, or is not absolute: a set but unusable `$SHELL` is refused instead
/// of replaced by a guess.
pub fn login_environment_with(
    shell: Option<PathBuf>,
    mut variables: Vec<(String, String)>,
) -> Result<LoginEnvironment, LoginEnvironmentError> {
    let shell_defaulted = shell.is_none();
    let shell = match shell {
        Some(shell) => {
            let value = text("SHELL", shell.into_os_string())?;
            if !Path::new(&value).is_absolute() {
                return Err(LoginEnvironmentError::NotAbsolute {
                    var: "SHELL",
                    value: PathBuf::from(value),
                });
            }
            PathBuf::from(value)
        }
        None => PathBuf::from(DEFAULT_LOGIN_SHELL),
    };
    variables.push((
        "SHELL".to_owned(),
        shell
            .to_str()
            .expect("the shell path was validated as UTF-8")
            .to_owned(),
    ));
    Ok(LoginEnvironment {
        shell,
        shell_defaulted,
        variables,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt as _;

    use super::*;

    fn lookup(pairs: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn identity_selectors_and_shell_are_passed_in_order() {
        let env = login_environment(lookup(vec![
            ("HOME", "/home/u".into()),
            ("USER", "u".into()),
            ("ZDOTDIR", "/home/u/.config/zsh".into()),
            ("XDG_CONFIG_HOME", "/home/u/.config".into()),
            ("SHELL", "/usr/bin/fish".into()),
            ("AWS_SECRET_ACCESS_KEY", "secret".into()),
        ]))
        .expect("environment");

        assert_eq!(env.shell, PathBuf::from("/usr/bin/fish"));
        assert!(!env.shell_defaulted);
        assert_eq!(
            env.variables,
            [
                ("HOME", "/home/u"),
                ("USER", "u"),
                ("ZDOTDIR", "/home/u/.config/zsh"),
                ("XDG_CONFIG_HOME", "/home/u/.config"),
                ("SHELL", "/usr/bin/fish"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        );
    }

    #[test]
    fn an_unset_shell_is_the_default_and_an_empty_selector_is_unset() {
        let env =
            login_environment(lookup(vec![("ZDOTDIR", OsString::new())])).expect("environment");

        assert!(env.shell_defaulted);
        assert_eq!(env.shell, PathBuf::from(DEFAULT_LOGIN_SHELL));
        assert_eq!(
            env.variables,
            [("SHELL".to_owned(), DEFAULT_LOGIN_SHELL.to_owned())]
        );
    }

    #[test]
    fn unusable_values_are_refused_with_a_typed_reason() {
        let cases: Vec<(&'static str, OsString)> = vec![
            ("ZDOTDIR", "relative/zsh".into()),
            ("XDG_CONFIG_HOME", "cfg".into()),
            ("SHELL", "zsh".into()),
            ("SHELL", OsString::from_vec(b"/bin/\xff".to_vec())),
            ("HOME", OsString::from_vec(b"/h\xff".to_vec())),
            ("USER", OsString::from_vec(b"u\0v".to_vec())),
        ];
        for (var, value) in cases {
            let error = login_environment(lookup(vec![(var, value)])).expect_err(var);
            assert!(
                matches!(
                    &error,
                    LoginEnvironmentError::NonUtf8 { var: found, .. }
                        | LoginEnvironmentError::NotAbsolute { var: found, .. }
                        | LoginEnvironmentError::NulByte { var: found } if *found == var
                ),
                "{var}: {error:?}"
            );
        }
    }
}
