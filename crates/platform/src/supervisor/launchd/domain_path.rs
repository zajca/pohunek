//! The persisted launchd user-domain `PATH`, read from `user.plist`.
//!
//! `launchctl config user path <value>` stores the value in a property list
//! of the launchd configuration directory. launchd applies it only after a
//! reboot, so this file is not the `PATH` of a running job: the active state is
//! in [`super::job_path`]. The CLI reads this file only for a job that is not
//! loaded, as the best evidence it has then.

use std::fs::File;
use std::io::{BufReader, ErrorKind};
use std::path::Path;
use std::time::SystemTime;

use ::plist::Value;

use super::super::Error;

/// Operation label of the domain `PATH` read, in supervisor errors.
const OPERATION: &str = "read_launchd_domain_path";

/// Operation label of the configuration's modification-time read.
const MODIFIED_OPERATION: &str = "read_launchd_domain_config_time";

/// The launchd user-domain configuration, which `launchctl config user path`
/// writes (see `launchctl(1)`).
pub const USER_DOMAIN_CONFIG: &str = "/private/var/db/com.apple.xpc.launchd/config/user.plist";

/// The property list key that holds the user domain's `PATH`.
const PATH_KEY: &str = "PathEnvironmentVariable";

/// The `PATH` launchd gives a job whose property list sets no `PATH`.
///
/// This is `_PATH_STDPATH` of the macOS `<paths.h>` (`/usr/include/paths.h`),
/// the default of the launchd domain when `launchctl config user path` has set
/// no value.
pub const STANDARD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Reads the user domain's `PATH` from the launchd configuration on this host.
///
/// # Errors
///
/// See [`read_user_domain_path`].
pub fn user_domain_path() -> Result<String, Error> {
    read_user_domain_path(Path::new(USER_DOMAIN_CONFIG))
}

/// Returns when the user domain's configuration at `config` last changed.
///
/// A symbolic link is dated by the later of the link and its target. An absent
/// file is dated by its directory, whose modification time moves
/// when the file is created, replaced or removed: the file was gone since then.
///
/// # Errors
///
/// Returns [`Error::Unavailable`] when neither the file nor its directory can
/// be dated.
pub fn user_domain_config_modified(config: &Path) -> Result<SystemTime, Error> {
    let modified = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .map_err(|source| Error::Unavailable {
                operation: MODIFIED_OPERATION,
                source: Box::new(source),
            })
    };
    match std::fs::symlink_metadata(config) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            modified(config.parent().unwrap_or_else(|| Path::new(".")))
        }
        // A link is replaced without touching its target, and a target is
        // rewritten without touching the link: the later of the two dates it.
        Ok(link) if link.file_type().is_symlink() => {
            let link = link.modified().map_err(|source| Error::Unavailable {
                operation: MODIFIED_OPERATION,
                source: Box::new(source),
            })?;
            Ok(link.max(modified(config)?))
        }
        _ => modified(config),
    }
}

/// Reads the user domain's `PATH` from the property list at `config`.
///
/// An absent file, or one without the `PathEnvironmentVariable` key, means
/// launchd's [`STANDARD_PATH`]. The file may be binary or XML.
///
/// # Errors
///
/// Returns [`Error::Unavailable`] when the file exists but cannot be opened or
/// read,
/// and [`Error::InvalidData`] when it does not parse as a property list with a
/// dictionary whose `PathEnvironmentVariable` is a string.
pub fn read_user_domain_path(config: &Path) -> Result<String, Error> {
    let file = match File::open(config) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(STANDARD_PATH.to_owned());
        }
        Err(source) => {
            return Err(Error::Unavailable {
                operation: OPERATION,
                source: Box::new(source),
            });
        }
    };
    let value =
        Value::from_reader(BufReader::new(file)).map_err(|error| match error.into_io() {
            Ok(source) => Error::Unavailable {
                operation: OPERATION,
                source: Box::new(source),
            },
            Err(error) => Error::InvalidData {
                operation: OPERATION,
                detail: format!(
                    "the launchd user domain configuration is not a property list: {error}"
                ),
            },
        })?;
    let dictionary = value.as_dictionary().ok_or_else(|| Error::InvalidData {
        operation: OPERATION,
        detail: "the launchd user domain configuration is not a dictionary".to_owned(),
    })?;
    match dictionary.get(PATH_KEY) {
        None => Ok(STANDARD_PATH.to_owned()),
        Some(value) => value
            .as_string()
            .map(str::to_owned)
            .ok_or_else(|| Error::InvalidData {
                operation: OPERATION,
                detail: format!("the launchd user domain's {PATH_KEY} is not a string"),
            }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Writes `bytes` as the configuration file `name` of the test directory.
    fn config_in(temp: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = temp.path().join(name);
        std::fs::write(&path, bytes).expect("write configuration");
        path
    }

    fn dictionary_xml(entries: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>{entries}</dict></plist>\n"
        )
        .into_bytes()
    }

    #[test]
    fn the_configuration_is_dated_by_its_file_or_else_its_directory() {
        let temp = pohunek_test_support::tempdir().expect("scratch directory");
        let epoch = |seconds: u64| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        let date = |path: &Path, when: SystemTime| {
            File::open(path)
                .and_then(|file| file.set_modified(when))
                .expect("date the path");
        };

        let config = config_in(&temp, "user.plist", &dictionary_xml(""));
        date(&config, epoch(1_000));
        date(temp.path(), epoch(2_000));
        assert_eq!(
            user_domain_config_modified(&config).expect("file"),
            epoch(1_000)
        );

        std::fs::remove_file(&config).expect("remove");
        date(temp.path(), epoch(3_000));
        assert_eq!(
            user_domain_config_modified(&config).expect("directory"),
            epoch(3_000)
        );

        // A link made after the boot to a target older than it is a recent change.
        let target = config_in(&temp, "target.plist", &dictionary_xml(""));
        date(&target, epoch(1_000));
        std::os::unix::fs::symlink(&target, &config).expect("link");
        let linked = std::fs::symlink_metadata(&config)
            .and_then(|metadata| metadata.modified())
            .expect("link time");
        assert!(linked > epoch(1_000));
        assert_eq!(user_domain_config_modified(&config).expect("link"), linked);

        let orphan = temp.path().join("absent").join("user.plist");
        assert!(matches!(
            user_domain_config_modified(&orphan),
            Err(Error::Unavailable { .. })
        ));
    }

    #[test]
    fn the_domain_path_scenarios_read_the_configuration_as_launchd_writes_it() {
        let temp = pohunek_test_support::tempdir().expect("scratch directory");

        let absent = temp.path().join("user.plist");
        assert_eq!(
            read_user_domain_path(&absent).expect("absent"),
            STANDARD_PATH
        );

        let with_key = config_in(
            &temp,
            "with-key.plist",
            &dictionary_xml(&format!(
                "<key>{PATH_KEY}</key><string>/opt/tools/bin:/usr/bin</string>"
            )),
        );
        assert_eq!(
            read_user_domain_path(&with_key).expect("with key"),
            "/opt/tools/bin:/usr/bin"
        );

        let without_key = config_in(
            &temp,
            "without-key.plist",
            &dictionary_xml("<key>Other</key><string>x</string>"),
        );
        assert_eq!(
            read_user_domain_path(&without_key).expect("without key"),
            STANDARD_PATH
        );

        let corrupt = config_in(&temp, "corrupt.plist", b"not a property list");
        assert!(matches!(
            read_user_domain_path(&corrupt),
            Err(Error::InvalidData { .. })
        ));

        let not_a_string = config_in(
            &temp,
            "not-a-string.plist",
            &dictionary_xml(&format!("<key>{PATH_KEY}</key><integer>1</integer>")),
        );
        assert!(matches!(
            read_user_domain_path(&not_a_string),
            Err(Error::InvalidData { .. })
        ));

        let unreadable_dir = temp.path().join("directory.plist");
        std::fs::create_dir(&unreadable_dir).expect("directory");
        assert!(matches!(
            read_user_domain_path(&unreadable_dir),
            Err(Error::Unavailable { .. })
        ));
    }

    #[test]
    fn a_binary_property_list_is_read_like_an_xml_one() {
        let temp = pohunek_test_support::tempdir().expect("scratch directory");
        let mut dictionary = ::plist::Dictionary::new();
        dictionary.insert(PATH_KEY.to_owned(), Value::String("/bin".to_owned()));
        let path = temp.path().join("binary.plist");
        Value::Dictionary(dictionary)
            .to_file_binary(&path)
            .expect("write binary plist");
        assert_eq!(read_user_domain_path(&path).expect("binary"), "/bin");
    }
}
