//! The `PATH` a launchd job of the user domain runs with when its definition
//! records none.
//!
//! launchd resolves it from its active state, in this order:
//!
//! 1. the domain's `environment`, which `launchctl setenv PATH` sets for the
//!    whole `gui/<uid>` domain. `launchctl print gui/<uid>` shows it, and that
//!    answer is the same whichever process asks and whether or not the job is
//!    loaded;
//! 2. otherwise the `default environment` launchd gives a loaded job, which
//!    `launchctl print gui/<uid>/<label>` shows (`_PATH_STDPATH` when nothing
//!    else is set).
//!
//! The persisted `user.plist` of the launchd configuration is not read here: a
//! `launchctl config user path` value takes effect only after a reboot. A job
//! that is not loaded has no default section, so [`JobPathSources::default()`]
//! tells the caller to fall back to the best evidence it has.
//!
//! `launchctl print` output is not a stable interface, so each section is read
//! strictly and anything unrecognized is an error rather than a guess. Values
//! are kept byte for byte apart from the line terminator.

use std::ffi::OsStr;
use std::time::Duration;

use super::super::Error;
use super::launchctl::{Launchctl, Status};
use super::{run_error, status_error};

/// Operation label of the active `PATH` read, in supervisor errors.
const OPERATION: &str = "read_launchd_job_path";

/// Name of the environment variable launchd resolves.
const PATH_NAME: &str = "PATH";

/// Heading of the domain environment in `launchctl print gui/<uid>`. launchd
/// omits it when the domain sets no variables.
const DOMAIN_SECTION: &str = "environment = {";

/// Heading of the services of `launchctl print gui/<uid>`, a top-level section
/// that identifies the output as a domain print.
const DOMAIN_SERVICES: &str = "services = {";

/// Heading of the default environment of a loaded job in
/// `launchctl print gui/<uid>/<label>`.
const DEFAULT_SECTION: &str = "default environment = {";

/// Line that closes a section of `launchctl print`.
const SECTION_END: &str = "}";

/// Separator between a variable and its value in a `launchctl print` section.
const ENTRY_SEPARATOR: &str = " => ";

/// Most bytes of `launchctl print` output that are read whole.
///
/// `launchctl print gui/<uid>` lists every service of the domain, so the
/// default cap of the runner is too small for it. A print longer than this is
/// refused, never read partially.
const PRINT_OUTPUT_CAP: usize = 1024 * 1024;

/// Longest part of an unrecognized line that an error repeats.
const EXCERPT_CHARS: usize = 160;

/// What launchd reports for a job's `PATH`, from both of its sources.
///
/// Both are read whatever the other says: the domain's value is readable whether
/// or not the job is loaded, and the job's default exists only while it is
/// loaded. The caller decides their order.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct JobPathSources {
    /// The domain's `PATH`, set with `launchctl setenv`, when the domain has one.
    pub domain: Option<String>,
    /// The `default environment` `PATH` of the loaded job; `None` when the job
    /// is not loaded (or launchd reports no such section for it).
    pub loaded_default: Option<String>,
    /// Every variable of the domain's `environment`, in launchd's order,
    /// `PATH` included. Empty when the domain sets none.
    pub domain_environment: Vec<(String, String)>,
}

// The domain's variables are printed by name only: their values may be secrets.
impl std::fmt::Debug for JobPathSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self
            .domain_environment
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        f.debug_struct("JobPathSources")
            .field("domain", &self.domain)
            .field("loaded_default", &self.loaded_default)
            .field("domain_environment", &names)
            .finish_non_exhaustive()
    }
}

/// Reads the `PATH` the job `label` of `gui/<uid>` runs with when its
/// definition sets none.
///
/// `deadline` bounds each `launchctl` command.
///
/// # Errors
///
/// Returns [`Error::Unavailable`], [`Error::Timeout`], [`Error::Operation`], or
/// [`Error::DomainUnavailable`] when `launchctl` cannot answer, and
/// [`Error::InvalidData`] when `launchctl print` output has an unrecognized shape
/// or is larger than [`PRINT_OUTPUT_CAP`].
pub async fn job_path_sources(
    uid: u32,
    label: &str,
    deadline: Duration,
) -> Result<JobPathSources, Error> {
    let launchctl = Launchctl::new(deadline).with_output_cap(PRINT_OUTPUT_CAP);
    resolve(&launchctl, &format!("gui/{uid}"), label).await
}

/// Reads the variables of the user domain `gui/<uid>`, `PATH` included, without
/// naming any job. A domain with no GUI session has none.
///
/// # Errors
///
/// Returns the errors of [`job_path_sources`] for the domain's own `launchctl print`.
pub async fn domain_environment_of(
    uid: u32,
    deadline: Duration,
) -> Result<Vec<(String, String)>, Error> {
    let launchctl = Launchctl::new(deadline).with_output_cap(PRINT_OUTPUT_CAP);
    domain_environment(&launchctl, &format!("gui/{uid}")).await
}

/// [`job_path_sources`] through the given runner, so tests can answer for launchd.
pub(crate) async fn resolve(
    launchctl: &Launchctl,
    domain: &str,
    label: &str,
) -> Result<JobPathSources, Error> {
    let domain_environment = domain_environment(launchctl, domain).await?;
    let domain_value = named_path(
        domain_environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
        false,
    )?;
    let target = format!("{domain}/{label}");
    let completion = launchctl
        .run("print", &[OsStr::new(&target)])
        .await
        .map_err(|error| run_error(OPERATION, error))?;
    let loaded_default = match completion.status {
        Status::Success => {
            let output = printed(&completion)?;
            Some(default_path(output)?)
        }
        // A domain with no GUI session loads no job, so the job is not loaded.
        Status::NoSuchService | Status::NoSuchDomain => None,
        _ => return Err(status_error(OPERATION, domain, &completion)),
    };
    Ok(JobPathSources {
        domain: domain_value,
        loaded_default,
        domain_environment,
    })
}

/// The variables of the domain's environment, empty when the domain sets none or
/// has no GUI session.
async fn domain_environment(
    launchctl: &Launchctl,
    domain: &str,
) -> Result<Vec<(String, String)>, Error> {
    let completion = launchctl
        .run("print", &[OsStr::new(domain)])
        .await
        .map_err(|error| run_error(OPERATION, error))?;
    match completion.status {
        Status::Success => domain_entries(printed(&completion)?),
        Status::NoSuchDomain => Ok(Vec::new()),
        _ => Err(status_error(OPERATION, domain, &completion)),
    }
}

/// The output of a successful `launchctl print`, refused when it was cut short.
fn printed(completion: &super::launchctl::Completion) -> Result<&str, Error> {
    if completion.stdout_truncated {
        return Err(truncated());
    }
    utf8(OPERATION, completion.stdout())
}

/// The refusal of a `launchctl print` output that was cut short.
fn truncated() -> Error {
    Error::InvalidData {
        operation: OPERATION,
        detail: format!("launchctl print output is larger than {PRINT_OUTPUT_CAP} bytes"),
    }
}

fn utf8<'a>(operation: &'static str, bytes: &'a [u8]) -> Result<&'a str, Error> {
    std::str::from_utf8(bytes).map_err(|error| Error::InvalidData {
        operation,
        detail: format!("launchctl printed output that is not UTF-8: {error}"),
    })
}

/// The variables of the domain environment of `launchctl print gui/<uid>` output.
///
/// Output is recognized as a domain print by its top-level `services` section,
/// which every domain print has. launchd omits the `environment` section of a
/// domain that sets no variables, so its absence means the domain has no
/// `PATH`. A present section occurs at most once and names `PATH` at most once
/// with a non-empty value; without `PATH` the domain falls through to the job.
///
/// # Errors
///
/// Returns [`Error::InvalidData`] for output without a `services` section, for
/// a repeated environment section, and for an unrecognized entry.
fn domain_entries(output: &str) -> Result<Vec<(String, String)>, Error> {
    let services = output
        .lines()
        .filter(|line| line.trim_start() == DOMAIN_SERVICES)
        .count();
    if services != 1 {
        return Err(unrecognized(
            "launchctl print",
            &format!("{services} services sections in a domain print"),
        ));
    }
    let mut found = sections(output, DOMAIN_SECTION)?;
    match found.len() {
        0 => Ok(Vec::new()),
        1 => Ok(found
            .remove(0)
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()),
        count => Err(unrecognized(
            "launchctl print",
            &format!("{count} domain environment sections"),
        )),
    }
}

/// The `PATH` of the default environment of `launchctl print gui/<uid>/<label>`
/// output.
///
/// The section must occur exactly once and name `PATH` exactly once with a
/// non-empty value. Its absence is a refusal, not a fall-through: a loaded job
/// without it has no default this module can report. Unlike the domain
/// environment, launchd's omission rule for empty sections is not verified for
/// this section, so the fail-closed reading is kept.
///
/// # Errors
///
/// Returns [`Error::InvalidData`] for output of any other shape.
fn default_path(output: &str) -> Result<String, Error> {
    let entries = single_section(output, DEFAULT_SECTION)?;
    named_path(entries.iter().copied(), true)?
        .ok_or_else(|| unrecognized("launchctl print", "the default environment names no PATH"))
}

/// The entries of the one section headed `heading`.
fn single_section<'a>(output: &'a str, heading: &str) -> Result<Vec<(&'a str, &'a str)>, Error> {
    let mut found = sections(output, heading)?;
    if found.len() != 1 {
        return Err(unrecognized(
            "launchctl print",
            &format!("{} sections headed `{heading}`", found.len()),
        ));
    }
    Ok(found.remove(0))
}

/// The value of `PATH` among `entries`.
///
/// `required` makes a missing `PATH` an error; `PATH` may occur at most once.
fn named_path<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    required: bool,
) -> Result<Option<String>, Error> {
    let mut path: Option<&str> = None;
    for (name, value) in entries {
        if name != PATH_NAME {
            continue;
        }
        if path.is_some() || value.is_empty() {
            return Err(unrecognized(
                "launchctl print",
                &format!("{name}{ENTRY_SEPARATOR}{value}"),
            ));
        }
        path = Some(value);
    }
    match path {
        Some(value) => Ok(Some(value.to_owned())),
        None if required => Err(unrecognized("launchctl print", "the section names no PATH")),
        None => Ok(None),
    }
}

/// The entries of each section headed `heading` in `output`.
///
/// Only the indentation before a line is removed, so an entry value keeps its
/// trailing spaces. A section that is never closed is an error.
fn sections<'a>(output: &'a str, heading: &str) -> Result<Vec<Vec<(&'a str, &'a str)>>, Error> {
    let mut sections = Vec::new();
    let mut open: Option<Vec<(&'a str, &'a str)>> = None;
    for line in output.lines() {
        let body = line.trim_start();
        if open.is_none() {
            if body == heading {
                open = Some(Vec::new());
            }
            continue;
        }
        if body == SECTION_END {
            sections.push(open.take().unwrap_or_default());
            continue;
        }
        let Some((name, value)) = body.split_once(ENTRY_SEPARATOR) else {
            return Err(unrecognized("launchctl print", line));
        };
        open.get_or_insert_with(Vec::new).push((name, value));
    }
    if open.is_some() {
        return Err(unrecognized("launchctl print", "a section is not closed"));
    }
    Ok(sections)
}

fn unrecognized(source: &str, text: &str) -> Error {
    let excerpt: String = text.chars().take(EXCERPT_CHARS).collect();
    Error::InvalidData {
        operation: OPERATION,
        detail: format!("unrecognized {source} output: `{excerpt}`"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// `launchctl print gui/501` of a domain whose environment sets `PATH`
    /// (trimmed to the environment section; the trailing space is part of the
    /// value and must survive).
    const DOMAIN_WITH_PATH: &str = "gui/501 = {\n\tenvironment = {\n\t\tSSH_AUTH_SOCK => /work/com.apple.launchd.example/Listeners\n\t\tPATH => /opt/set/bin:/usr/bin \n\t}\n\tservices = {\n\t}\n}\n";

    /// `launchctl print gui/501` of a domain with no variables: launchd omits
    /// its `environment` section, so only the services section is printed.
    const DOMAIN_WITHOUT_ENVIRONMENT: &str =
        "gui/501 = {\n\tsecurity context = {\n\t}\n\tservices = {\n\t}\n}\n";

    /// `launchctl print gui/501` of a domain that sets no `PATH` (the zajcaair
    /// shape: the environment holds `SSH_AUTH_SOCK` only).
    const DOMAIN_WITHOUT_PATH: &str = "gui/501 = {\n\tenvironment = {\n\t\tSSH_AUTH_SOCK => /work/com.apple.launchd.example/Listeners\n\t}\n\tservices = {\n\t}\n}\n";

    /// `launchctl print gui/501` of a domain whose `PATH` is empty.
    const DOMAIN_EMPTY_PATH: &str =
        "gui/501 = {\n\tenvironment = {\n\t\tPATH => \n\t}\n\tservices = {\n\t}\n}\n";

    /// `launchctl print gui/501/<label>` of a loaded job, as captured on the
    /// macOS 27.0.1 host (trimmed to the three sections; the job header lines
    /// are omitted).
    const LOADED_JOB: &str = "\tinherited environment = {\n\t\tSSH_AUTH_SOCK => /work/com.apple.launchd.example/Listeners\n\t}\n\tdefault environment = {\n\t\tPATH => /usr/bin:/bin:/usr/sbin:/sbin\n\t}\n\tenvironment = {\n\t\tPATH => /opt/pohunek-test/bin:/usr/bin\n\t}\n";

    /// A loaded job whose default `PATH` ends in a space, which must be kept.
    const LOADED_JOB_TRAILING_SPACE: &str =
        "\tdefault environment = {\n\t\tPATH => /usr/bin:/bin \n\t}\n";

    /// A reply of the `launchctl` stand-in: the text it prints, or the status it exits with.
    #[derive(Clone, Copy)]
    enum Reply {
        Output(&'static str),
        Status(i32),
    }

    /// A `launchctl` stand-in answering the domain print (`gui/<uid>`) with
    /// `domain` and the job print (`gui/<uid>/<label>`) with `job`.
    fn fake(dir: &Path, domain: Reply, job: Reply, cap: Option<usize>) -> Launchctl {
        let reply = |name: &str, reply: &Reply| match reply {
            Reply::Output(text) => {
                let path = dir.join(name);
                std::fs::write(&path, text).expect("print output");
                format!("cat '{}'", path.display())
            }
            Reply::Status(code) => format!("exit {code}"),
        };
        let script = format!(
            "case \"$2\" in gui/*/*) {} ;; *) {} ;; esac",
            reply("job.out", &job),
            reply("domain.out", &domain)
        );
        let runner = Launchctl::with_program(
            Path::new("/bin/sh"),
            &["-c", &script, "launchctl"],
            Duration::from_secs(10),
        );
        match cap {
            Some(cap) => runner.with_output_cap(cap),
            None => runner,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime starts")
    }

    #[test]
    fn the_default_section_of_a_loaded_job_is_read_and_the_others_are_not() {
        assert_eq!(
            default_path(LOADED_JOB).expect("fixture"),
            "/usr/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn a_value_keeps_its_trailing_space() {
        assert_eq!(
            default_path(LOADED_JOB_TRAILING_SPACE).expect("fixture"),
            "/usr/bin:/bin "
        );
    }

    #[test]
    fn output_of_another_shape_is_refused_with_the_line_that_does_not_fit() {
        let no_section = "\tenvironment = {\n\t\tPATH => /usr/bin\n\t}\n";
        let open = "\tdefault environment = {\n\t\tPATH => /usr/bin\n";
        let no_path = "\tdefault environment = {\n\t\tHOME => /Users/test\n\t}\n";
        let empty = "\tdefault environment = {\n\t\tPATH => \n\t}\n";
        let unknown = "\tdefault environment = {\n\t\tPATH /usr/bin\n\t}\n";
        let twice = "\tdefault environment = {\n\t\tPATH => /usr/bin\n\t}\n\tdefault environment = {\n\t}\n";
        for text in [no_section, open, no_path, empty, unknown, twice] {
            let error = default_path(text).expect_err("unrecognized output");
            let Error::InvalidData { detail, .. } = &error else {
                panic!("{error:?}");
            };
            assert!(
                detail.starts_with("unrecognized launchctl print output"),
                "{detail}"
            );
        }
    }

    #[test]
    fn a_domain_path_wins_over_the_default_of_a_loaded_job() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_WITH_PATH),
                Reply::Output(LOADED_JOB),
                None,
            );
            assert_eq!(
                resolve(&launchctl, "gui/501", "job")
                    .await
                    .expect("resolved"),
                JobPathSources {
                    domain: Some("/opt/set/bin:/usr/bin ".to_owned()),
                    loaded_default: Some("/usr/bin:/bin:/usr/sbin:/sbin".to_owned()),
                    domain_environment: vec![
                        (
                            "SSH_AUTH_SOCK".to_owned(),
                            "/work/com.apple.launchd.example/Listeners".to_owned(),
                        ),
                        ("PATH".to_owned(), "/opt/set/bin:/usr/bin ".to_owned()),
                    ],
                }
            );
        });
    }

    #[test]
    fn a_domain_without_a_path_falls_through_to_the_loaded_jobs_default() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_WITHOUT_PATH),
                Reply::Output(LOADED_JOB),
                None,
            );
            assert_eq!(
                resolve(&launchctl, "gui/501", "job")
                    .await
                    .expect("resolved"),
                JobPathSources {
                    domain: None,
                    loaded_default: Some("/usr/bin:/bin:/usr/sbin:/sbin".to_owned()),
                    domain_environment: vec![(
                        "SSH_AUTH_SOCK".to_owned(),
                        "/work/com.apple.launchd.example/Listeners".to_owned(),
                    )],
                }
            );
        });
    }

    #[test]
    fn a_loaded_jobs_default_keeps_its_trailing_space() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_WITHOUT_PATH),
                Reply::Output(LOADED_JOB_TRAILING_SPACE),
                None,
            );
            assert_eq!(
                resolve(&launchctl, "gui/501", "job")
                    .await
                    .expect("resolved"),
                JobPathSources {
                    domain: None,
                    loaded_default: Some("/usr/bin:/bin ".to_owned()),
                    domain_environment: vec![(
                        "SSH_AUTH_SOCK".to_owned(),
                        "/work/com.apple.launchd.example/Listeners".to_owned(),
                    )],
                }
            );
        });
    }

    #[test]
    fn a_job_that_is_not_loaded_with_no_domain_path_reports_not_loaded() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_WITHOUT_PATH),
                Reply::Status(113),
                None,
            );
            assert_eq!(
                resolve(&launchctl, "gui/501", "job")
                    .await
                    .expect("resolved"),
                JobPathSources {
                    domain_environment: vec![(
                        "SSH_AUTH_SOCK".to_owned(),
                        "/work/com.apple.launchd.example/Listeners".to_owned(),
                    )],
                    ..JobPathSources::default()
                }
            );
        });
    }

    #[test]
    fn the_debug_form_of_the_domain_sources_names_the_variables_and_never_their_values() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let domain = "gui/501 = {\n\tenvironment = {\n\t\tSECRET_STORE => /sentinel/secret-value\n\t}\n\tservices = {\n\t}\n}\n";
            let launchctl = fake(dir.path(), Reply::Output(domain), Reply::Status(113), None);
            let sources = resolve(&launchctl, "gui/501", "job").await.expect("resolved");

            let debug = format!("{sources:?}");
            assert!(debug.contains("SECRET_STORE"), "the name is shown: {debug}");
            assert!(!debug.contains("/sentinel/secret-value"), "no value: {debug}");
        });
    }

    #[test]
    fn a_domain_without_a_gui_session_loads_no_job_so_the_job_is_not_loaded() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(dir.path(), Reply::Status(112), Reply::Status(113), None);
            assert_eq!(
                resolve(&launchctl, "gui/9999", "job")
                    .await
                    .expect("resolved"),
                JobPathSources::default()
            );
        });
    }

    #[test]
    fn a_domain_without_an_environment_section_falls_through_to_the_loaded_jobs_default() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_WITHOUT_ENVIRONMENT),
                Reply::Output(LOADED_JOB),
                None,
            );
            assert_eq!(
                resolve(&launchctl, "gui/501", "job")
                    .await
                    .expect("resolved"),
                JobPathSources {
                    domain: None,
                    loaded_default: Some("/usr/bin:/bin:/usr/sbin:/sbin".to_owned()),
                    domain_environment: Vec::new(),
                }
            );
        });
    }

    #[test]
    fn a_print_without_a_services_section_is_not_a_domain_print_and_is_refused() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output("gui/501 = {\n\tsecurity context = {\n\t}\n}\n"),
                Reply::Output(LOADED_JOB),
                None,
            );
            assert!(matches!(
                resolve(&launchctl, "gui/501", "job").await,
                Err(Error::InvalidData { .. })
            ));
        });
    }

    #[test]
    fn an_unrecognized_domain_print_is_refused() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output("launchctl changed its output\n"),
                Reply::Output(LOADED_JOB),
                None,
            );
            let error = resolve(&launchctl, "gui/501", "job")
                .await
                .expect_err("no environment section");
            assert!(matches!(error, Error::InvalidData { .. }), "{error:?}");
        });
    }

    #[test]
    fn an_empty_domain_path_is_refused_rather_than_read_as_unset() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_EMPTY_PATH),
                Reply::Output(LOADED_JOB),
                None,
            );
            assert!(matches!(
                resolve(&launchctl, "gui/501", "job").await,
                Err(Error::InvalidData { .. })
            ));
        });
    }

    #[test]
    fn an_unrecognized_loaded_job_print_is_refused() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_WITHOUT_PATH),
                Reply::Output("launchctl changed its output\n"),
                None,
            );
            assert!(matches!(
                resolve(&launchctl, "gui/501", "job").await,
                Err(Error::InvalidData { .. })
            ));
        });
    }

    #[test]
    fn a_print_longer_than_its_cap_is_refused_not_read_partially() {
        runtime().block_on(async {
            let dir = pohunek_test_support::tempdir().expect("scratch directory");
            let launchctl = fake(
                dir.path(),
                Reply::Output(DOMAIN_WITH_PATH),
                Reply::Output(LOADED_JOB),
                Some(16),
            );
            let error = resolve(&launchctl, "gui/501", "job")
                .await
                .expect_err("the domain print is longer than the cap");
            let Error::InvalidData { detail, .. } = &error else {
                panic!("{error:?}");
            };
            assert!(detail.contains("larger than"), "{detail}");
        });
    }
}
