//! The per-launch input that keeps a Cline session in the TUI's process tree.
//!
//! Without sandbox mode Cline attaches a session to a shared per-machine hub
//! daemon, or starts one that outlives the session. `CLINE_SANDBOX=1` forces
//! the local runtime and, unless `CLINE_SANDBOX_DATA_DIR` names a data root,
//! moves the session store to the private `TMPDIR`; the contribution therefore
//! always carries both variables, with the data root Cline would have used.

// Rust guideline compliant 2026-11-06

use std::path::Path;

use protocol::{ErrorClass, ProtocolError};

use crate::integration::launch_input::{LaunchContributor, LaunchInput, LaunchInputContext};

/// Variable that selects Cline's sandbox mode.
const SANDBOX_ENV: &str = "CLINE_SANDBOX";

/// Variable that names the data root sandbox mode keeps.
const SANDBOX_DATA_DIR_ENV: &str = "CLINE_SANDBOX_DATA_DIR";

/// Variable that moves Cline's data root.
const DATA_DIR_ENV: &str = "CLINE_DATA_DIR";

/// Subdirectory of the config home that holds the data root by default.
const DEFAULT_DATA_SUBDIR: &str = "data";

/// Variables sandbox mode overrides or that the handler sets itself. A launch
/// that carries one would run with a store the handler did not resolve.
const REFUSED_ENV: &[&str] = &[
    SANDBOX_ENV,
    SANDBOX_DATA_DIR_ENV,
    "CLINE_SESSION_DATA_DIR",
    "CLINE_DB_DATA_DIR",
    "CLINE_PROVIDER_SETTINGS_PATH",
];

/// Flags that move the config or data root inside the process.
const REFUSED_FLAGS: &[&str] = &["--data-dir", "--config"];

/// Error code of a launch refused for a flag.
pub(super) const ARG_REFUSED_CODE: &str = "cline_launch_arg_refused";

/// Error code of a launch refused for an environment variable.
pub(super) const ENV_REFUSED_CODE: &str = "cline_launch_env_refused";

/// Error code of a launch whose data root cannot be resolved.
pub(super) const DATA_ROOT_INVALID_CODE: &str = "cline_data_root_invalid";

/// Error code of a launch of a runtime that declares no config home.
pub(super) const CONFIG_HOME_MISSING_CODE: &str = "cline_config_home_missing";

/// Contributes the sandbox variables to every Cline launch.
#[derive(Debug)]
pub(super) struct ClineLaunch;

impl LaunchContributor for ClineLaunch {
    fn contribute(&self, ctx: &LaunchInputContext<'_>) -> Result<LaunchInput, ProtocolError> {
        refuse_flags(ctx.args)?;
        refuse_env(ctx.env)?;
        let root = data_root(ctx)?;
        Ok(LaunchInput {
            env: vec![
                (SANDBOX_ENV.to_owned(), "1".to_owned()),
                (SANDBOX_DATA_DIR_ENV.to_owned(), root),
            ],
            ..LaunchInput::default()
        })
    }
}

/// Refuses a flag that relocates the config or data root.
fn refuse_flags(args: &[String]) -> Result<(), ProtocolError> {
    for arg in args {
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        if REFUSED_FLAGS.contains(&name) {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                ARG_REFUSED_CODE,
                format!("Cline sandbox mode overrides {name}, so a launch with it is refused"),
                Some(format!(
                    "remove {name} from the runtime or profile arguments; set CLINE_DIR or CLINE_DATA_DIR in the profile env instead"
                )),
            ));
        }
    }
    Ok(())
}

/// Refuses an environment variable that sandbox mode overrides. The message
/// names the variable, never its value.
fn refuse_env(env: &[(String, String)]) -> Result<(), ProtocolError> {
    for name in REFUSED_ENV {
        if env.iter().any(|(set, _value)| set == name) {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                ENV_REFUSED_CODE,
                format!("Cline sandbox mode overrides {name}, so a launch that sets it is refused"),
                Some(format!(
                    "remove {name} from the profile env or the service environment"
                )),
            ));
        }
    }
    Ok(())
}

/// The data root Cline would use: `CLINE_DATA_DIR`, else `<config home>/data`.
fn data_root(ctx: &LaunchInputContext<'_>) -> Result<String, ProtocolError> {
    let Some(config_home) = ctx.config_home else {
        return Err(ProtocolError::new(
            ErrorClass::Runtime,
            CONFIG_HOME_MISSING_CODE,
            "the Cline runtime declares no config home, so its data root cannot be resolved",
            Some("declare [config_home] env = \"CLINE_DIR\" in the runtime descriptor".to_owned()),
        ));
    };
    let explicit = ctx
        .env
        .iter()
        .rev()
        .find(|(name, _value)| name == DATA_DIR_ENV)
        .map(|(_name, value)| value.as_str());
    match explicit {
        Some(value) if Path::new(value).is_absolute() => Ok(value.to_owned()),
        Some(_) => Err(ProtocolError::new(
            ErrorClass::Runtime,
            DATA_ROOT_INVALID_CODE,
            format!("{DATA_DIR_ENV} must be an absolute path"),
            Some(format!(
                "set {DATA_DIR_ENV} to an absolute path in the profile env"
            )),
        )),
        None => config_home
            .join(DEFAULT_DATA_SUBDIR)
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorClass::Runtime,
                    DATA_ROOT_INVALID_CODE,
                    "the Cline config home is not valid UTF-8",
                    None,
                )
            }),
    }
}
