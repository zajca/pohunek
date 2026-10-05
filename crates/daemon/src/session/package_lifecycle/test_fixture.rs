//! Fixtures of the package lifecycle tests: real archives, a real plugin root
//! and a session registry over a host that serves it.

// Rust guideline compliant 2026-10-05

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use protocol::{PackageInstallParams, PackageTrust, RuntimeId};

use super::HostTrustAnchor;
use crate::agent::host::fixture::{pi_shaped_document, PI_SHAPED_NO_CHECK};
use crate::agent::host::{BuiltinSource, PackageSource, PackageStore, RuntimeHost};
use crate::session::tests::{hermetic_shell, temp_dir};
use crate::session::{SessionRegistry, SessionRegistryConfig};

/// Detection manifest every fixture package ships.
const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

/// Package id of the default fixture package.
pub(super) const PACKAGE: &str = "acme.runtime.pi";

/// Runtime id of the default fixture package.
pub(super) const RUNTIME: &str = "pi";

/// Program of a fixture package that is never launched.
pub(super) const INERT_PROGRAM: &str = "/bin/sh";

/// A real package archive.
#[derive(Clone)]
pub(super) struct Package {
    pub(super) bytes: Vec<u8>,
    pub(super) digest: PackageDigest,
}

impl Package {
    /// Builds the archive of a package `package` at `version` serving
    /// `runtime`, launching `program`.
    pub(super) fn build(package: &str, version: &str, runtime: &str, program: &str) -> Self {
        Self::build_with(package, version, runtime, program, None)
    }

    /// Like [`Self::build`], declaring `integration` as the handler id and the
    /// hook schema id of the descriptor's `[integration]` table.
    pub(super) fn build_with(
        package: &str,
        version: &str,
        runtime: &str,
        program: &str,
        integration: Option<(&str, &str)>,
    ) -> Self {
        let mut document = pi_shaped_document(Path::new(program), PI_SHAPED_NO_CHECK)
            .replace("id = \"acme.runtime.pi\"", &format!("id = \"{package}\""))
            .replace("version = \"1.0.0\"", &format!("version = \"{version}\""))
            .replace("id = \"pi\"", &format!("id = \"{runtime}\""))
            .replace(
                "detect_manifest = \"any\"",
                "detect_manifest = \"detect.toml\"",
            );
        if let Some((handler, schema)) = integration {
            let _ = write!(
                document,
                "\n[integration]\nhandler = \"{handler}\"\nhook_schema = \"{schema}\"\n"
            );
        }
        Self::from_entries(&files(document))
    }

    /// The default package at 1.0.0.
    pub(super) fn pi() -> Self {
        Self::build(PACKAGE, "1.0.0", RUNTIME, INERT_PROGRAM)
    }

    /// Builds the archive of arbitrary entries.
    pub(super) fn from_entries(entries: &[ArchiveEntry]) -> Self {
        let bytes = build_archive(entries, &Limits::DEFAULT).expect("the fixture archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the fixture archive reads")
            .digest()
            .clone();
        Self { bytes, digest }
    }

    /// An archive whose descriptor is `document`.
    pub(super) fn with_descriptor(document: &str) -> Self {
        Self::from_entries(&files(document.to_owned()))
    }

    /// An archive without a runtime descriptor.
    pub(super) fn without_descriptor() -> Self {
        Self::from_entries(&[ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: DETECT_MANIFEST.as_bytes().to_vec(),
            executable: false,
        }])
    }
}

/// The two files of a fixture package.
pub(super) fn files(document: String) -> Vec<ArchiveEntry> {
    vec![
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: document.into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: DETECT_MANIFEST.as_bytes().to_vec(),
            executable: false,
        },
    ]
}

/// Parameters of an explicit-digest install of `package` from `path`.
pub(super) fn explicit(
    path: &str,
    package: &Package,
    enable: bool,
    select: bool,
) -> PackageInstallParams {
    PackageInstallParams {
        archive_path: path.to_owned(),
        trust: PackageTrust::ExplicitDigest {
            digest: package.digest.clone(),
        },
        enable,
        select,
        dry_run: false,
    }
}

/// A plugin root, a host serving it and a session registry over the host.
pub(super) struct Fixture {
    pub(super) dir: PathBuf,
    pub(super) plugins: PathBuf,
    /// The agents directory the registry loads host profiles from.
    pub(super) agents: PathBuf,
    pub(super) host: RuntimeHost,
    pub(super) registry: SessionRegistry,
    store_path: Option<PathBuf>,
}

impl Fixture {
    /// A fixture without a trust anchor and without a session store.
    pub(super) fn new(tag: &str) -> Self {
        Self::build(tag, None, None)
    }

    /// A fixture with a catalog trust anchor.
    pub(super) fn with_anchor(tag: &str, anchor: HostTrustAnchor) -> Self {
        Self::build(tag, Some(anchor), None)
    }

    /// A fixture whose registry persists sessions to `store_path`.
    pub(super) fn with_store(tag: &str, store_path: PathBuf) -> Self {
        Self::build(tag, None, Some(store_path))
    }

    /// A second daemon over the plugin root, agents directory and session
    /// store of this fixture, as after a restart.
    pub(super) fn reopen(&self) -> Self {
        Self::at(self.dir.clone(), None, self.store_path.clone())
    }

    fn build(tag: &str, anchor: Option<HostTrustAnchor>, store_path: Option<PathBuf>) -> Self {
        Self::at(temp_dir(tag), anchor, store_path)
    }

    fn at(dir: PathBuf, anchor: Option<HostTrustAnchor>, store_path: Option<PathBuf>) -> Self {
        let plugins = dir.join("plugins");
        let agents = dir.join("agents");
        let state = dir.join("state");
        for private in [&agents, &state] {
            fs::create_dir_all(private).expect("create a private fixture directory");
            fs::set_permissions(private, fs::Permissions::from_mode(0o700))
                .expect("secure a fixture directory");
        }
        let host = RuntimeHost::with_packages(
            BuiltinSource::new("/bin/sh"),
            PackageSource::new(PackageStore::open(&plugins).expect("the store opens")),
        )
        .expect("the host builds");
        let registry = SessionRegistry::new_with_runtimes(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                stop_grace: std::time::Duration::from_millis(50),
                store_path: store_path.clone(),
                catalog_trust_anchor: anchor,
                agents_dir: Some(agents.clone()),
                host_state_dir: Some(state),
                ..SessionRegistryConfig::default()
            },
            host.clone(),
        );
        Self {
            dir,
            plugins,
            agents,
            host,
            registry,
            store_path,
        }
    }

    /// Writes `bytes` as a file in the fixture directory and returns its
    /// absolute path.
    pub(super) fn write(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.dir.join(name);
        fs::write(&path, bytes).expect("write the fixture file");
        path.to_str().expect("utf-8 path").to_owned()
    }

    /// Writes the host profile `name` with `text`, owner-private.
    pub(super) fn write_profile(&self, name: &str, text: &str) -> PathBuf {
        let path = self.agents.join(format!("{name}.toml"));
        fs::write(&path, text).expect("write the profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure the profile");
        path
    }

    /// Writes the archive of `package` and returns its path.
    pub(super) fn write_archive(&self, package: &Package) -> String {
        self.write(
            &format!("{}.tar.zst", package.digest.as_str().replace(':', "-")),
            &package.bytes,
        )
    }

    /// The registry record and the entries below `packages/`, to compare
    /// before and after a refused call.
    pub(super) fn snapshot(&self) -> (Option<Vec<u8>>, Vec<String>) {
        let record = fs::read(self.plugins.join("registry.json")).ok();
        let mut roots: Vec<String> = fs::read_dir(self.plugins.join("packages"))
            .expect("the packages directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        roots.sort();
        (record, roots)
    }

    /// The installed root directory of `digest`.
    pub(super) fn root(&self, digest: &PackageDigest) -> PathBuf {
        self.plugins.join("packages").join(
            digest
                .as_str()
                .strip_prefix("sha256:")
                .expect("digest prefix"),
        )
    }

    /// Appends a line to the installed descriptor of `digest`.
    pub(super) fn tamper(&self, digest: &PackageDigest) {
        let descriptor = self.root(digest).join("files").join("runtime.toml");
        let mut bytes = fs::read(&descriptor).expect("read descriptor");
        bytes.extend_from_slice(b"\n# tampered\n");
        fs::write(&descriptor, bytes).expect("write descriptor");
    }

    /// The digest and package a bare request for `runtime` resolves to now.
    pub(super) fn serving(&self, runtime: &str) -> Option<PackageDigest> {
        let id = RuntimeId::parse(runtime).expect("runtime id");
        let definition = self.host.resolve_id(&id).ok()?;
        match &definition.binding().provenance {
            protocol::BindingProvenance::Package { package_digest, .. } => {
                Some(package_digest.clone())
            }
            protocol::BindingProvenance::Builtin { .. } => None,
        }
    }
}
