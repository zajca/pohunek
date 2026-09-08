//! Canonical decimal strings preserve `PostgreSQL` revisions in JavaScript.

use serde::{Deserialize, Deserializer, Serializer};

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "Serde's with adapter passes the field by reference"
)]
pub(crate) fn serialize<S>(value: &i64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if *value < 0 {
        return Err(serde::ser::Error::custom("revision must be nonnegative"));
    }
    serializer.collect_str(value)
}

pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    let revision = value
        .parse::<i64>()
        .map_err(|_error| serde::de::Error::custom("invalid revision"))?;
    if revision < 0 || revision.to_string() != value {
        return Err(serde::de::Error::custom(
            "revision must be canonical decimal",
        ));
    }
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use crate::Record;
    use uuid::Uuid;

    #[test]
    fn revisions_round_trip_without_javascript_precision_loss() {
        for revision in [0, 1, 9_007_199_254_740_993, i64::MAX] {
            let record = Record {
                id: Uuid::nil(),
                revision,
            };
            let wire = serde_json::to_value(record).expect("serialize revision");
            assert_eq!(wire["revision"], revision.to_string());
            assert_eq!(
                serde_json::from_value::<Record>(wire).expect("deserialize revision"),
                record
            );
        }
    }

    #[test]
    fn revisions_reject_noncanonical_and_out_of_range_values() {
        for revision in ["-1", "01", "+1", " 1", "1.0", "9223372036854775808", ""] {
            let wire = serde_json::json!({"id": Uuid::nil(), "revision": revision});
            serde_json::from_value::<Record>(wire).expect_err("invalid revision");
        }
        serde_json::from_value::<Record>(serde_json::json!({"id": Uuid::nil(), "revision": 1}))
            .expect_err("JSON numbers are not revisions");
        serde_json::to_value(Record {
            id: Uuid::nil(),
            revision: -1,
        })
        .expect_err("negative database revisions must not escape");
    }
}
