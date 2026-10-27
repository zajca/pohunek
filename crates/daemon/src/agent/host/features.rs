//! Runtime features: the append-only registry of named core capabilities a
//! runtime descriptor can require.
//!
//! A descriptor lists the features it needs in `requires`. The core refuses a
//! descriptor that names a feature it does not know, and refuses a descriptor
//! key or compiled id that belongs to a feature the descriptor does not
//! require. A feature is added by the change that implements it, as one row
//! of [`RUNTIME_FEATURES`] plus its rows of [`FEATURE_GATES`], and the row is
//! never removed or renamed: a published descriptor may require it.

// Rust guideline compliant 2026-10-26

use package::version_probe::SEMVER_REVISION_LINE_PARSER_ID;
use toml::{Table, Value};

use super::definition::{DefinitionError, MAX_LABEL_BYTES};
use super::transcripts::MUSE_SESSION_PARSER_ID;

/// Maximum number of entries in a descriptor's `requires`.
pub(super) const MAX_REQUIRES: usize = 16;

/// Separator between the feature name and its version number.
const VERSION_MARKER: &str = "-v";

/// One runtime feature this core implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RuntimeFeature {
    /// Feature id, shaped `[a-z0-9-]+-v<N>`.
    pub(super) id: &'static str,
    /// Core version that first implemented the feature.
    pub(super) since: &'static str,
}

/// What in a descriptor belongs to a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GateRule {
    /// A descriptor key at this dotted path is present.
    Key(&'static str),
    /// The descriptor key at this dotted path names this compiled parser or
    /// handler id.
    Value(&'static str, &'static str),
}

impl GateRule {
    fn path(self) -> &'static str {
        match self {
            Self::Key(path) | Self::Value(path, _) => path,
        }
    }

    fn matches(self, descriptor: &Table) -> bool {
        let Some(found) = lookup(descriptor, self.path()) else {
            return false;
        };
        match self {
            Self::Key(_) => true,
            Self::Value(_, value) => found.as_str() == Some(value),
        }
    }
}

/// A descriptor rule that is accepted only when `feature` is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FeatureGate {
    /// Feature id the rule belongs to.
    pub(super) feature: &'static str,
    /// What in the descriptor the feature owns.
    pub(super) rule: GateRule,
}

/// The feature rows and gate rows one descriptor parse checks against.
#[derive(Debug, Clone, Copy)]
pub(super) struct FeatureTables<'a> {
    /// Features the core implements.
    pub(super) features: &'a [RuntimeFeature],
    /// Descriptor rules owned by those features.
    pub(super) gates: &'a [FeatureGate],
}

/// Features this core implements, in the order they were added.
///
/// `fixtures/features.txt` snapshots this table; a removal or rename fails the
/// guard test.
pub(super) const RUNTIME_FEATURES: &[RuntimeFeature] = &[
    RuntimeFeature {
        id: "config-home-subdir-v1",
        since: "0.38.0",
    },
    RuntimeFeature {
        id: "version-probe-revision-v1",
        since: "0.38.0",
    },
    RuntimeFeature {
        id: "launch-env-v1",
        since: "0.38.0",
    },
    RuntimeFeature {
        id: "transcripts-data-root-v1",
        since: "0.38.0",
    },
];

/// Descriptor rules owned by [`RUNTIME_FEATURES`].
pub(super) const FEATURE_GATES: &[FeatureGate] = &[
    FeatureGate {
        feature: "config-home-subdir-v1",
        rule: GateRule::Key("config_home.subdir"),
    },
    FeatureGate {
        feature: "version-probe-revision-v1",
        rule: GateRule::Value(
            "runtime.version_probe.parser",
            SEMVER_REVISION_LINE_PARSER_ID,
        ),
    },
    FeatureGate {
        feature: "launch-env-v1",
        rule: GateRule::Key("launch_env"),
    },
    FeatureGate {
        feature: "transcripts-data-root-v1",
        rule: GateRule::Key("transcripts.root_env"),
    },
    FeatureGate {
        feature: "transcripts-data-root-v1",
        rule: GateRule::Key("transcripts.root_home"),
    },
    FeatureGate {
        feature: "transcripts-data-root-v1",
        rule: GateRule::Key("transcripts.max_depth"),
    },
    FeatureGate {
        feature: "transcripts-data-root-v1",
        rule: GateRule::Value("transcripts.parser", MUSE_SESSION_PARSER_ID),
    },
    FeatureGate {
        feature: "transcripts-data-root-v1",
        rule: GateRule::Key("native_reference.existence.dir_name"),
    },
];

impl FeatureTables<'static> {
    /// The tables a production descriptor parse uses.
    pub(super) const PRODUCTION: Self = Self {
        features: RUNTIME_FEATURES,
        gates: FEATURE_GATES,
    };
}

/// Whether `id` is shaped `[a-z0-9-]+-v<N>` and fits [`MAX_LABEL_BYTES`].
#[must_use]
pub(super) fn is_feature_id(id: &str) -> bool {
    if id.len() > MAX_LABEL_BYTES {
        return false;
    }
    let Some((name, number)) = id.rsplit_once(VERSION_MARKER) else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// Checks the descriptor's `requires` list before the typed parse.
///
/// A `requires` of another TOML type is left for the typed parse to report.
/// Running first lets an unknown feature win over the `Malformed` error of a
/// key only that feature's core can parse.
///
/// # Errors
///
/// [`DefinitionError::Field`] for too many entries, a duplicate or an entry
/// that is not a feature id, and [`DefinitionError::FeatureUnsupported`] for
/// the first well-formed entry the core does not implement.
pub(super) fn check_requires(
    descriptor: &Table,
    tables: &FeatureTables<'_>,
) -> Result<(), DefinitionError> {
    let Some(requires) = declared_requires(descriptor) else {
        return Ok(());
    };
    let field = |reason| DefinitionError::Field {
        field: "requires",
        reason,
    };
    if requires.len() > MAX_REQUIRES {
        return Err(field("has too many entries"));
    }
    for (index, entry) in requires.iter().enumerate() {
        if !is_feature_id(entry) {
            return Err(field("entry is not a feature id"));
        }
        if requires[..index].contains(entry) {
            return Err(field("entry is listed twice"));
        }
    }
    match requires
        .iter()
        .find(|entry| !tables.features.iter().any(|feature| feature.id == **entry))
    {
        Some(unknown) => Err(DefinitionError::FeatureUnsupported {
            feature: (*unknown).to_owned(),
            core: crate::DAEMON_VERSION,
        }),
        None => Ok(()),
    }
}

/// Checks that every feature-owned rule the descriptor uses is covered by its
/// `requires`.
///
/// # Errors
///
/// [`DefinitionError::FeatureNotRequired`] naming the key path of the first
/// rule whose feature the descriptor does not require.
pub(super) fn check_gates(
    descriptor: &Table,
    tables: &FeatureTables<'_>,
) -> Result<(), DefinitionError> {
    let requires = declared_requires(descriptor).unwrap_or_default();
    match tables
        .gates
        .iter()
        .find(|gate| gate.rule.matches(descriptor) && !requires.contains(&gate.feature))
    {
        Some(gate) => Err(DefinitionError::FeatureNotRequired {
            key: gate.rule.path(),
            feature: gate.feature,
        }),
        None => Ok(()),
    }
}

/// The `requires` entries when the key is an array of strings.
fn declared_requires(descriptor: &Table) -> Option<Vec<&str>> {
    descriptor
        .get("requires")?
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect()
}

fn lookup<'a>(descriptor: &'a Table, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut current = descriptor.get(parts.next()?)?;
    for part in parts {
        current = current.as_table()?.get(part)?;
    }
    Some(current)
}

/// Why `table` is not an append-only continuation of `snapshot`, or `None`.
///
/// `snapshot` holds one `<id> <since>` row per line in table order. Every
/// snapshot row must still be present unchanged at the same position, and the
/// table must not hold a row the snapshot lacks, so each addition is a
/// deliberate edit of the snapshot.
#[cfg(test)]
fn snapshot_violation(table: &[RuntimeFeature], snapshot: &str) -> Option<String> {
    let rows: Vec<String> = table
        .iter()
        .map(|feature| format!("{} {}", feature.id, feature.since))
        .collect();
    let recorded: Vec<&str> = snapshot.lines().collect();
    for (index, line) in recorded.iter().enumerate() {
        if rows.get(index).map(String::as_str) != Some(*line) {
            return Some(format!(
                "snapshot row {index} `{line}` is removed or changed"
            ));
        }
    }
    rows.get(recorded.len())
        .map(|row| format!("table row `{row}` is not in the snapshot"))
}

/// Why `features` and `gates` are not a well-formed table, or `None`.
///
/// Ids must be feature ids and unique, `since` must be a semver version that
/// never decreases down the table, and every gate must name a listed feature.
/// `since` has no upper bound: a feature is recorded with the release that
/// ships it, which is later than the version of the tree that adds it.
#[cfg(test)]
fn table_violation(features: &[RuntimeFeature], gates: &[FeatureGate]) -> Option<String> {
    let mut previous: Option<semver::Version> = None;
    for (index, feature) in features.iter().enumerate() {
        if !is_feature_id(feature.id) {
            return Some(format!("row {index} `{}` is not a feature id", feature.id));
        }
        if features[..index]
            .iter()
            .any(|earlier| earlier.id == feature.id)
        {
            return Some(format!("row {index} `{}` is listed twice", feature.id));
        }
        let Ok(since) = semver::Version::parse(feature.since) else {
            return Some(format!("row {index} `{}` has no semver since", feature.id));
        };
        if previous.as_ref().is_some_and(|earlier| *earlier > since) {
            return Some(format!("row {index} `{}` has an earlier since", feature.id));
        }
        previous = Some(since);
    }
    gates
        .iter()
        .find(|gate| !features.iter().any(|feature| feature.id == gate.feature))
        .map(|gate| format!("gate for unlisted feature `{}`", gate.feature))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_table_is_well_formed() {
        assert_eq!(table_violation(RUNTIME_FEATURES, FEATURE_GATES), None);
    }

    #[test]
    fn table_guard_catches_malformed_duplicate_and_unordered_rows() {
        let row = |id, since| RuntimeFeature { id, since };
        let gate = |feature| FeatureGate {
            feature,
            rule: GateRule::Key("config_home"),
        };
        let ok = [row("a-v1", "0.38.0"), row("b-v1", "0.38.0")];
        assert_eq!(table_violation(&ok, &[gate("b-v1")]), None);
        assert!(table_violation(&[row("A-v1", "0.38.0")], &[]).is_some());
        assert!(table_violation(&[row("a", "0.38.0")], &[]).is_some());
        assert!(table_violation(&[row("a-v1", "0.38.0"), row("a-v1", "0.39.0")], &[]).is_some());
        assert!(table_violation(&[row("a-v1", "next")], &[]).is_some());
        assert!(table_violation(&[row("a-v1", "0.39.0"), row("b-v1", "0.38.0")], &[]).is_some());
        assert!(table_violation(&ok, &[gate("c-v1")]).is_some());
        // A feature of the unreleased next version is valid in this tree.
        assert_eq!(table_violation(&[row("a-v1", "99.0.0")], &[]), None);
    }

    #[test]
    fn production_table_equals_the_snapshot() {
        let snapshot = include_str!("fixtures/features.txt");
        assert_eq!(
            snapshot_violation(RUNTIME_FEATURES, snapshot),
            None,
            "edit fixtures/features.txt only to append a row"
        );
    }

    #[test]
    fn snapshot_guard_catches_removal_rename_and_unrecorded_rows() {
        let table = [
            RuntimeFeature {
                id: "a-v1",
                since: "0.38.0",
            },
            RuntimeFeature {
                id: "b-v1",
                since: "0.39.0",
            },
        ];
        assert_eq!(
            snapshot_violation(&table, "a-v1 0.38.0\nb-v1 0.39.0\n"),
            None
        );
        assert!(snapshot_violation(&table[..1], "a-v1 0.38.0\nb-v1 0.39.0\n").is_some());
        assert!(snapshot_violation(&table[1..], "a-v1 0.38.0\nb-v1 0.39.0\n").is_some());
        assert!(snapshot_violation(&table, "a-v1 0.38.0\nc-v1 0.39.0\n").is_some());
        assert!(snapshot_violation(&table, "a-v1 0.38.1\nb-v1 0.39.0\n").is_some());
        assert!(snapshot_violation(&table, "a-v1 0.38.0\n").is_some());
    }
}
