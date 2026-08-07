// Rust guideline compliant 2026-08-06

#![expect(
    clippy::map_err_ignore,
    reason = "target errors intentionally redact rejected filesystem details"
)]
#![expect(
    clippy::needless_pass_by_value,
    reason = "the context constructor consumes caller-owned selections at the trust boundary"
)]

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use nix::unistd::Uid;

use super::error::Error;

/// The directory containing Hermes profiles below the explicitly selected root.
const PROFILES_DIR: &str = "profiles";
/// The fixed plugin location below one validated Hermes home.
const PLUGIN_COMPONENTS: [&str; 3] = ["plugins", "operators", "pohunek"];
/// Hermes accepts one normalized profile component with at most 64 ASCII bytes.
const MAX_PROFILE_BYTES: usize = 64;
/// Hermes reserves these profile labels for runtime-managed or unsafe uses.
const RESERVED_PROFILE_NAMES: [&str; 5] = ["hermes", "test", "tmp", "root", "sudo"];
/// Any group or other write bit can let another local user replace managed files.
const UNSAFE_WRITE_BITS: u32 = 0o022;
/// Sticky shared directories such as `/tmp` safely isolate entry replacement.
const STICKY_BIT: u32 = 0o1000;

/// A normalized Hermes profile name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProfileName(String);

impl ProfileName {
    /// Validates one explicitly selected Hermes profile name.
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into().trim().to_ascii_lowercase();
        if matches!(value.as_str(), "." | "..") {
            return Err(Error::ReservedProfile);
        }
        if RESERVED_PROFILE_NAMES.contains(&value.as_str()) {
            return Err(Error::ReservedProfile);
        }
        let bytes = value.as_bytes();
        let valid = !bytes.is_empty()
            && bytes.len() <= MAX_PROFILE_BYTES
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.iter().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(Error::InvalidProfile)
        }
    }

    /// Returns the normalized profile name.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProfileName {
    fn default() -> Self {
        // `default` is part of Hermes's supported profile grammar.
        Self("default".to_owned())
    }
}

/// Exactly one operator-selected Hermes target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetSelection {
    /// A profile below the command's explicit default Hermes root.
    Profile(ProfileName),
    /// An explicitly selected absolute Hermes home.
    CustomHome(PathBuf),
}

/// Fixed invocation semantics retained for the future Hermes subprocess wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HermesInvocation {
    /// Invoke Hermes with a normalized named profile.
    Profile(ProfileName),
    /// Invoke Hermes with an explicitly selected custom home.
    CustomHome,
}

/// A validated Hermes home and its managed plugin destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTarget {
    hermes_home: PathBuf,
    plugin_root: PathBuf,
    invocation: HermesInvocation,
}

impl ResolvedTarget {
    /// Returns the canonical Hermes home selected by the operator.
    #[must_use]
    pub(crate) fn hermes_home(&self) -> &Path {
        &self.hermes_home
    }

    /// Returns the fixed managed Pohunek plugin root.
    #[must_use]
    pub(crate) fn plugin_root(&self) -> &Path {
        &self.plugin_root
    }

    /// Returns the fixed semantics the Hermes runner must use.
    #[must_use]
    pub(crate) fn invocation(&self) -> &HermesInvocation {
        &self.invocation
    }
}

/// Explicit process-independent inputs needed to resolve a Hermes target.
#[derive(Debug, Clone)]
pub(crate) struct TargetContext {
    default_hermes_root: PathBuf,
    user_home: PathBuf,
    workspace_roots: Vec<PathBuf>,
    uid: u32,
}

impl TargetContext {
    /// Creates a resolver context from explicit caller-owned paths.
    ///
    /// This constructor deliberately does not read environment variables or a
    /// Hermes database. The command layer supplies every security boundary.
    pub(crate) fn new(
        default_hermes_root: PathBuf,
        user_home: PathBuf,
        workspace_roots: Vec<PathBuf>,
    ) -> Result<Self, Error> {
        if !default_hermes_root.is_absolute()
            || !user_home.is_absolute()
            || workspace_roots.iter().any(|path| !path.is_absolute())
        {
            return Err(Error::RelativePath);
        }
        Ok(Self {
            default_hermes_root: canonical_with_missing_tail(&default_hermes_root)?,
            user_home: canonical_with_missing_tail(&user_home)?,
            workspace_roots: workspace_roots
                .iter()
                .map(|path| canonical_with_missing_tail(path))
                .collect::<Result<_, _>>()?,
            uid: Uid::effective().as_raw(),
        })
    }

    /// Resolves exactly one selected target without reading Hermes contents.
    pub(crate) fn resolve(&self, selection: TargetSelection) -> Result<ResolvedTarget, Error> {
        let (hermes_home, invocation) = match selection {
            TargetSelection::Profile(profile) if profile == ProfileName::default() => {
                self.validate_target(&self.default_hermes_root)?;
                (
                    self.default_hermes_root.clone(),
                    HermesInvocation::Profile(profile),
                )
            }
            TargetSelection::Profile(profile) => {
                self.validate_target(&self.default_hermes_root)?;
                let requested = self
                    .default_hermes_root
                    .join(PROFILES_DIR)
                    .join(profile.as_str());
                reject_symlink_components(&requested)?;
                let home = canonical_existing_dir(&requested, self.uid)?;
                if !home.starts_with(&self.default_hermes_root) {
                    return Err(Error::UnsafeTarget);
                }
                self.validate_target(&home)?;
                (home, HermesInvocation::Profile(profile))
            }
            TargetSelection::CustomHome(home) => {
                if !home.is_absolute() {
                    return Err(Error::RelativePath);
                }
                reject_symlink_components(&home)?;
                let home = canonical_with_missing_tail(&home)?;
                if has_git_workspace_ancestor(&home)? {
                    return Err(Error::UnsafeTarget);
                }
                self.validate_target(&home)?;
                (home, HermesInvocation::CustomHome)
            }
        };
        let plugin_root = canonical_contained_child(&hermes_home, &PLUGIN_COMPONENTS)?;
        Ok(ResolvedTarget {
            hermes_home,
            plugin_root,
            invocation,
        })
    }

    fn validate_target(&self, home: &Path) -> Result<(), Error> {
        if is_broad_path(home)
            || home == self.user_home
            || self
                .workspace_roots
                .iter()
                .any(|root| paths_intersect(home, root))
        {
            return Err(Error::UnsafeTarget);
        }
        let existing = nearest_existing(home)?;
        let metadata = fs::metadata(&existing)?;
        if !metadata.is_dir() || metadata.uid() != self.uid {
            return Err(Error::UnsafePermissions);
        }
        validate_ancestor_permissions(&existing)?;
        Ok(())
    }
}

fn has_git_workspace_ancestor(path: &Path) -> Result<bool, Error> {
    let existing = nearest_existing(path)?;
    for ancestor in existing.ancestors() {
        match fs::symlink_metadata(ancestor.join(".git")) {
            Ok(marker) if marker.is_dir() || marker.is_file() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) fn isolated_test_temp_root() -> PathBuf {
    let standard = std::env::temp_dir();
    if !has_git_workspace_ancestor(&standard).unwrap_or(true) {
        return standard;
    }
    // `/var/tmp` is the standard persistent Unix temporary root and keeps
    // custom-target fixtures outside an ambient repository rooted at `/tmp`.
    let fallback = PathBuf::from("/var/tmp");
    assert!(
        !has_git_workspace_ancestor(&fallback).unwrap_or(true),
        "no temporary root outside a Git workspace"
    );
    fallback
}

fn canonical_existing_dir(path: &Path, uid: u32) -> Result<PathBuf, Error> {
    if fs::symlink_metadata(path).is_err() {
        return Err(Error::UnsafeTarget);
    }
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(Error::UnsafePermissions);
    }
    validate_ancestor_permissions(&canonical)?;
    Ok(canonical)
}

fn canonical_contained_child(base: &Path, components: &[&str]) -> Result<PathBuf, Error> {
    let child = components
        .iter()
        .fold(base.to_owned(), |path, component| path.join(component));
    reject_symlink_components(&child)?;
    let canonical = canonical_with_missing_tail(&child)?;
    if canonical.starts_with(base) {
        Ok(canonical)
    } else {
        Err(Error::UnsafeTarget)
    }
}

fn canonical_with_missing_tail(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::RelativePath);
    }
    let existing = nearest_existing(path)?;
    let canonical = fs::canonicalize(&existing)?;
    let tail = path
        .strip_prefix(&existing)
        .map_err(|_| Error::UnsafeTarget)?;
    Ok(canonical.join(tail))
}

fn reject_symlink_components(path: &Path) -> Result<(), Error> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(Error::UnsafeTarget),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn nearest_existing(path: &Path) -> Result<PathBuf, Error> {
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => return Ok(current.to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = current.parent().ok_or(Error::UnsafeTarget)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn validate_ancestor_permissions(path: &Path) -> Result<(), Error> {
    for ancestor in path.ancestors() {
        let metadata = fs::metadata(ancestor)?;
        let mode = metadata.permissions().mode();
        let shared_sticky_dir =
            metadata.is_dir() && mode & STICKY_BIT != 0 && mode & UNSAFE_WRITE_BITS != 0;
        if mode & UNSAFE_WRITE_BITS != 0 && !shared_sticky_dir {
            return Err(Error::UnsafePermissions);
        }
    }
    Ok(())
}

fn is_broad_path(path: &Path) -> bool {
    path == Path::new("/")
        || path.parent() == Some(Path::new("/"))
        || path
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count()
            < 2
}

fn paths_intersect(first: &Path, second: &Path) -> bool {
    first.starts_with(second) || second.starts_with(first)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl std::ops::Deref for Fixture {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.path) {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "cleanup fixture"
                );
            }
        }
    }

    fn temp_dir(tag: &str) -> Fixture {
        loop {
            let counter = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = isolated_test_temp_root().join(format!(
                "pohunek-hermes-target-{tag}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                        .expect("set private mode");
                    return Fixture { path };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create isolated test directory: {error}"),
            }
        }
    }

    fn context(root: &Path) -> TargetContext {
        let hermes = root.join("hermes");
        let home = root.join("home");
        let workspace = root.join("workspace");
        for path in [&hermes, &home, &workspace] {
            fs::create_dir(path).expect("create fixture directory");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("set private mode");
        }
        TargetContext::new(hermes, home, vec![workspace]).expect("absolute context")
    }

    #[test]
    fn resolves_default_named_and_custom_targets() {
        let root = temp_dir("happy");
        let context = context(&root);
        let profiles = root.join("hermes/profiles");
        let named_home = profiles.join("pohunek-compat");
        for path in [&profiles, &named_home] {
            fs::create_dir(path).expect("create named profile directory");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("set private mode");
        }

        let default = context
            .resolve(TargetSelection::Profile(ProfileName::default()))
            .expect("default target");
        assert_eq!(default.hermes_home(), root.join("hermes").as_path());
        assert_eq!(
            default.plugin_root(),
            root.join("hermes/plugins/operators/pohunek").as_path()
        );
        assert!(
            matches!(default.invocation(), HermesInvocation::Profile(name) if name.as_str() == "default")
        );

        let named = context
            .resolve(TargetSelection::Profile(
                ProfileName::new("pohunek-compat").expect("profile"),
            ))
            .expect("named target");
        assert_eq!(
            named.hermes_home(),
            root.join("hermes/profiles/pohunek-compat")
        );

        let custom = context
            .resolve(TargetSelection::CustomHome(root.join("custom-home")))
            .expect("custom target");
        assert_eq!(custom.hermes_home(), root.join("custom-home"));
        assert_eq!(custom.invocation(), &HermesInvocation::CustomHome);
    }

    #[test]
    fn rejects_invalid_and_reserved_profiles() {
        assert_eq!(ProfileName::new("."), Err(Error::ReservedProfile));
        for profile in ["..", "", "has.dot", "has/slash", "-leading", "a b"] {
            assert!(ProfileName::new(profile).is_err(), "{profile}");
        }
        for profile in RESERVED_PROFILE_NAMES {
            assert_eq!(ProfileName::new(profile), Err(Error::ReservedProfile));
        }
        assert_eq!(
            ProfileName::new(" Default ").expect("normalized").as_str(),
            "default"
        );
        assert_eq!(
            ProfileName::new("Coder").expect("normalized").as_str(),
            "coder"
        );
        ProfileName::new("lowercase_123-name-_").expect("valid profile");
        ProfileName::new("trailing-").expect("valid trailing hyphen");
        ProfileName::new("a".repeat(MAX_PROFILE_BYTES + 1)).expect_err("oversize profile");
    }

    #[test]
    fn rejects_missing_named_profile_and_unsafe_default_root() {
        let root = temp_dir("named-root");
        let context = context(&root);
        assert_eq!(
            context.resolve(TargetSelection::Profile(
                ProfileName::new("coder").expect("profile"),
            )),
            Err(Error::UnsafeTarget)
        );

        let profiles = root.join("hermes/profiles");
        fs::create_dir(&profiles).expect("create profiles directory");
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o700))
            .expect("set private mode");
        fs::write(profiles.join("coder"), b"not a directory").expect("write invalid profile");
        assert_eq!(
            context.resolve(TargetSelection::Profile(
                ProfileName::new("coder").expect("profile"),
            )),
            Err(Error::UnsafePermissions)
        );

        fs::set_permissions(root.join("hermes"), fs::Permissions::from_mode(0o777))
            .expect("set unsafe mode");
        assert_eq!(
            context.resolve(TargetSelection::Profile(ProfileName::default())),
            Err(Error::UnsafePermissions)
        );
    }

    #[test]
    fn rejects_relative_root_home_workspace_and_broad_targets() {
        let root = temp_dir("forbidden");
        let context = context(&root);
        assert_eq!(
            context.resolve(TargetSelection::CustomHome(PathBuf::from("relative"))),
            Err(Error::RelativePath)
        );
        for target in [
            PathBuf::from("/"),
            root.join("home"),
            root.join("workspace"),
            root.join("workspace/nested"),
            root.to_path_buf(),
        ] {
            assert_eq!(
                context.resolve(TargetSelection::CustomHome(target)),
                Err(Error::UnsafeTarget)
            );
        }
    }

    #[test]
    fn rejects_named_profile_symlink_escape() {
        let root = temp_dir("symlink");
        let context = context(&root);
        let profiles = root.join("hermes/profiles");
        symlink(root.join("outside"), &profiles).expect("create symlink");
        fs::create_dir(root.join("outside")).expect("create symlink target");

        assert_eq!(
            context.resolve(TargetSelection::Profile(
                ProfileName::new("coder").expect("profile"),
            )),
            Err(Error::UnsafeTarget)
        );
    }

    #[test]
    fn rejects_custom_target_with_any_symlink_component() {
        let root = temp_dir("custom-symlink");
        let context = context(&root);
        fs::create_dir(root.join("outside")).expect("create symlink target");
        symlink(root.join("outside"), root.join("custom-link")).expect("create symlink");
        assert_eq!(
            context.resolve(TargetSelection::CustomHome(root.join("custom-link/home"))),
            Err(Error::UnsafeTarget)
        );
    }

    #[test]
    fn rejects_custom_target_inside_a_git_workspace_not_listed_by_the_caller() {
        let root = temp_dir("custom-git-workspace");
        let context = context(&root);
        let repository = root.join("other-repository");
        fs::create_dir(&repository).expect("create repository");
        fs::set_permissions(&repository, fs::Permissions::from_mode(0o700))
            .expect("set private mode");
        fs::write(repository.join(".git"), b"gitdir: ignored\n").expect("write git marker");

        assert_eq!(
            context.resolve(TargetSelection::CustomHome(repository.join("hermes-home"))),
            Err(Error::UnsafeTarget)
        );
    }

    #[test]
    fn rejects_symlinks_in_each_existing_plugin_component() {
        for (index, component) in PLUGIN_COMPONENTS.iter().enumerate() {
            let root = temp_dir(component);
            let context = context(&root);
            let outside = root.join("outside");
            fs::create_dir(&outside).expect("create symlink target");
            let mut parent = root.join("hermes");
            for previous in &PLUGIN_COMPONENTS[..index] {
                parent.push(previous);
                fs::create_dir(&parent).expect("create plugin component");
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))
                    .expect("set private mode");
            }
            symlink(&outside, parent.join(component)).expect("create plugin symlink");
            assert_eq!(
                context.resolve(TargetSelection::Profile(ProfileName::default())),
                Err(Error::UnsafeTarget),
                "{component}"
            );
        }
    }

    #[test]
    fn canonicalizes_protected_home_before_comparing_targets() {
        let root = temp_dir("protected-symlink");
        let hermes = root.join("hermes");
        let home = root.join("home");
        let workspace = root.join("workspace");
        for path in [&hermes, &home, &workspace] {
            fs::create_dir(path).expect("create fixture directory");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("set private mode");
        }
        let home_link = root.join("home-link");
        symlink(&home, &home_link).expect("create home link");
        let context = TargetContext::new(hermes, home_link, vec![workspace]).expect("context");

        assert_eq!(
            context.resolve(TargetSelection::CustomHome(home)),
            Err(Error::UnsafeTarget)
        );
    }

    #[test]
    fn rejects_unsafe_mode_and_wrong_owner() {
        let root = temp_dir("permissions");
        let mut context = context(&root);
        let custom = root.join("custom");
        fs::create_dir(&custom).expect("create custom home");
        fs::set_permissions(&custom, fs::Permissions::from_mode(0o777)).expect("set unsafe mode");
        assert_eq!(
            context.resolve(TargetSelection::CustomHome(custom.clone())),
            Err(Error::UnsafePermissions)
        );

        fs::set_permissions(&custom, fs::Permissions::from_mode(0o700))
            .expect("restore private mode");
        context.uid = context.uid.saturating_add(1);
        assert_eq!(
            context.resolve(TargetSelection::CustomHome(custom)),
            Err(Error::UnsafePermissions)
        );
    }

    #[test]
    fn error_display_and_recovery_do_not_leak_unsafe_paths() {
        let error = Error::UnsafeTarget;
        let rendered = error.to_string();
        assert!(!rendered.contains("/tmp"));
        assert!(!error.recovery_hint().contains("/tmp"));
    }
}
