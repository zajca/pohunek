//! Presentation-only profile labels: `display_name` and `audience`.
//!
//! A profile file may carry a human `display_name`, shown in pickers, tables
//! and session responses, and an `audience` naming the pickers the profile
//! appears in (`owner`, `agent` or `hidden`). Both keys are cosmetic: they are
//! stripped from the text before the revision digest runs, so adding, editing
//! or removing a label never changes a profile revision and never invalidates
//! a relay approval. A profile without either key resolves exactly as before,
//! with an absent `audience` meaning the owner audience.
//!
//! The module also builds the revision text ([`revision_text`]), which for a
//! profile that declares options digests the choices a launch selected.

use std::borrow::Cow;

use protocol::MAX_PROFILE_DISPLAY_NAME_BYTES;
use serde::Deserialize;
use toml_edit::{DocumentMut, ImDocument};

use super::options::SelectedChoices;

/// A parsed profile `display_name`: trimmed free text the daemon validated
/// on load. Custom [`Deserialize`] rejects a non-string, an empty or
/// all-whitespace value, an overlong value and a value holding a hidden or
/// separator character, with a fixed message that never echoes the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProfileDisplayName(String);

impl ProfileDisplayName {
    /// The validated label text.
    #[must_use]
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProfileDisplayName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Label;

        impl<'de> serde::de::Visitor<'de> for Label {
            type Value = ProfileDisplayName;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a display_name string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let trimmed = v.trim();
                if trimmed.is_empty() {
                    return Err(E::custom("display_name must not be empty"));
                }
                if trimmed.len() > MAX_PROFILE_DISPLAY_NAME_BYTES {
                    return Err(E::custom(format!(
                        "display_name exceeds {MAX_PROFILE_DISPLAY_NAME_BYTES} bytes"
                    )));
                }
                if trimmed
                    .chars()
                    .any(pohunek_worker_protocol::is_hidden_or_separator)
                {
                    return Err(E::custom(
                        "display_name must not contain control, separator or format characters",
                    ));
                }
                Ok(ProfileDisplayName(trimmed.to_owned()))
            }

            // A non-string TOML value reports the fixed message, never
            // serde's default `invalid type` text that quotes the value.
            fn visit_bool<E: serde::de::Error>(self, _v: bool) -> Result<Self::Value, E> {
                Err(E::custom("display_name must be a string"))
            }

            fn visit_i64<E: serde::de::Error>(self, _value: i64) -> Result<Self::Value, E> {
                Err(E::custom("display_name must be a string"))
            }

            fn visit_u64<E: serde::de::Error>(self, _value: u64) -> Result<Self::Value, E> {
                Err(E::custom("display_name must be a string"))
            }

            fn visit_f64<E: serde::de::Error>(self, _value: f64) -> Result<Self::Value, E> {
                Err(E::custom("display_name must be a string"))
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                Err(serde::de::Error::custom("display_name must be a string"))
            }

            fn visit_map<A>(self, _map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                Err(serde::de::Error::custom("display_name must be a string"))
            }
        }

        deserializer.deserialize_str(Label)
    }
}

/// A parsed profile `audience`: the picker the profile appears in. Custom
/// [`Deserialize`] rejects any value outside `owner` | `agent` | `hidden`
/// with a fixed message that never echoes the value. A local newtype (never
/// the protocol enum itself) so the private visitor stays in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProfileAudience(protocol::ProfileAudience);

impl ProfileAudience {
    /// The validated audience value.
    #[must_use]
    pub(super) fn get(self) -> protocol::ProfileAudience {
        self.0
    }
}

impl<'de> Deserialize<'de> for ProfileAudience {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Audience;

        impl<'de> serde::de::Visitor<'de> for Audience {
            type Value = ProfileAudience;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("`owner`, `agent` or `hidden`")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "owner" => Ok(ProfileAudience(protocol::ProfileAudience::Owner)),
                    "agent" => Ok(ProfileAudience(protocol::ProfileAudience::Agent)),
                    "hidden" => Ok(ProfileAudience(protocol::ProfileAudience::Hidden)),
                    _ => Err(E::custom("audience must be one of owner, agent, hidden")),
                }
            }

            // A non-string TOML value reports the fixed message, never
            // serde's default `invalid type` text that quotes the value.
            fn visit_bool<E: serde::de::Error>(self, _v: bool) -> Result<Self::Value, E> {
                Err(E::custom("audience must be one of owner, agent, hidden"))
            }

            fn visit_i64<E: serde::de::Error>(self, _value: i64) -> Result<Self::Value, E> {
                Err(E::custom("audience must be one of owner, agent, hidden"))
            }

            fn visit_u64<E: serde::de::Error>(self, _value: u64) -> Result<Self::Value, E> {
                Err(E::custom("audience must be one of owner, agent, hidden"))
            }

            fn visit_f64<E: serde::de::Error>(self, _value: f64) -> Result<Self::Value, E> {
                Err(E::custom("audience must be one of owner, agent, hidden"))
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                Err(serde::de::Error::custom(
                    "audience must be one of owner, agent, hidden",
                ))
            }

            fn visit_map<A>(self, _map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                Err(serde::de::Error::custom(
                    "audience must be one of owner, agent, hidden",
                ))
            }
        }

        deserializer.deserialize_str(Audience)
    }
}

/// The profile text as the revision digest sees it: `original` with the
/// `display_name` and `audience` label lines cut out.
///
/// Profiles without either key — the cheap pre-check that finds neither
/// substring — digest byte for byte, so every revision today's daemon froze
/// stays valid. Otherwise the cut removes exactly each label's own line:
/// the label key starts its line (it is a top-level key) and the line
/// ending (LF, CRLF or none) goes with the label, including a trailing
/// `#` comment that sits on the same line as the label. Standalone comment
/// and blank lines stay: they are real file content and keep their digest
/// weight, so only the label lines themselves are presentation-only. A
/// dotted key such as `foo.display_name` is not a label and never matches.
/// [`super::bind`] strips a pin through [`toml_edit`] (a lossy
/// `DocumentMut` round-trip that rewrites layout), while this cut is
/// byte-preserving by design: only the label lines themselves leave, so a
/// label-only edit never moves the digest but any other text change does.
/// This is the single place that builds the revision text.
///
/// The `editing_only_labels` scenarios pin which layout the cut claims:
/// editing a label value, or adding only a label line, keeps the revision,
/// while adding a comment or a blank line as its own line around the
/// labels is a real file change and the revision moves with it.
///
/// A profile whose root table holds `options` digests its effective launch
/// instead: the text re-rendered through [`toml_edit`] without the label keys
/// and the `options` table, followed by the `selected` choices (see
/// [`SelectedChoices`]). That rendering is lossy (it may rewrite layout), so
/// it applies only to profiles that declare options; a profile without an
/// `options` key, even one that mentions the word elsewhere, takes the
/// byte-preserving label cut and keeps every revision frozen before options
/// existed. `selected` is ignored for such a profile.
pub(crate) fn revision_text<'a>(
    original: &'a str,
    selected: &SelectedChoices,
) -> Result<Cow<'a, str>, String> {
    if original.contains(OPTIONS_KEY) {
        let mut document: DocumentMut = original
            .parse()
            .map_err(|error: toml_edit::TomlError| unparsable(original, &error, OPTIONS_KEY))?;
        if document.as_table().contains_key(OPTIONS_KEY) {
            for key in ["display_name", "audience", OPTIONS_KEY] {
                document.remove(key);
            }
            let mut text = document.to_string();
            text.push_str(&selected.revision_suffix()?);
            return Ok(Cow::Owned(text));
        }
    }
    without_labels(original)
}

/// Root key of the profile options table.
const OPTIONS_KEY: &str = "options";

/// `original` with the `display_name` and `audience` label lines cut out,
/// byte for byte otherwise; see [`revision_text`].
fn without_labels(original: &str) -> Result<Cow<'_, str>, String> {
    if !original.contains("display_name") && !original.contains("audience") {
        return Ok(Cow::Borrowed(original));
    }
    // Each pass parses the current text once and cuts from that same
    // parse: the parse finds the label's byte span (`ImDocument` keeps
    // spans; a `DocumentMut` round-trip would drop decor and normalise
    // newlines) and the cut removes only the label's own line. The two
    // passes cut different keys off the same text, so the second pass
    // re-parses its own cut result. An unparsable text fails with the key
    // name only, never a value.
    let mut text = Cow::Borrowed(original);
    for key in ["display_name", "audience"] {
        let parsed = text.clone().into_owned();
        let document =
            ImDocument::parse(parsed.as_str()).map_err(|error| unparsable(&parsed, &error, key))?;
        if let Some(span) = label_line_span(&document, &parsed, key) {
            let mut cut = String::with_capacity(parsed.len() - span.len());
            cut.push_str(&parsed[..span.start]);
            cut.push_str(&parsed[span.end..]);
            text = Cow::Owned(cut);
        }
    }
    Ok(text)
}

/// The revision-text error for `text` that does not parse while `key` is cut
/// from it: the line and the key name, never a value.
fn unparsable(text: &str, error: &toml_edit::TomlError, key: &str) -> String {
    let line = text
        .get(..error.span().map_or(0, |span| span.start))
        .map_or(1, |head| head.matches('\n').count() + 1);
    format!("line {line}: `{key}` keeps the revision text unparsable")
}

/// The byte range of the `key` label's own line in `text`, or `None` when
/// the text does not parse or the root table holds no such key. The range
/// starts at the line's first byte (the key itself starts the line, after
/// optional whitespace) and covers one line ending — including a trailing
/// `#` comment that sits on the same line — so removing it leaves
/// no blank line and no dangling comment behind.
fn label_line_span<S: AsRef<str>>(
    document: &ImDocument<S>,
    text: &str,
    key: &str,
) -> Option<std::ops::Range<usize>> {
    let (found, item) = document.as_table().get_key_value(key)?;
    if found.get() != key {
        return None;
    }
    let key_start = found.span()?.start;
    let value_end = item.span()?.end;
    let bytes = text.as_bytes();
    let mut start = key_start;
    while start > 0 && bytes[start - 1] != b'\n' {
        if bytes[start - 1] != b' ' && bytes[start - 1] != b'\t' {
            // A dotted key (`foo.display_name`) has no own line: it is not
            // a label.
            return None;
        }
        start -= 1;
    }
    let mut end = value_end;
    // A trailing `#` comment shares the label's line: it documents the
    // label, so it leaves with the label (the docs' own example writes the
    // labels with trailing comments).
    let comment_marker = text[end..].find('#').map(|offset| end + offset);
    let line_end = text[end..]
        .find('\n')
        .map_or(text.len(), |offset| end + offset);
    if comment_marker.is_some_and(|marker| marker < line_end) {
        end = line_end;
    } else {
        while end < line_end && (bytes[end] == b' ' || bytes[end] == b'\t') {
            end += 1;
        }
    }
    if bytes.get(end) == Some(&b'\r') && bytes.get(end + 1) == Some(&b'\n') {
        end += 2;
    } else if bytes.get(end) == Some(&b'\n') {
        end += 1;
    } else if end == bytes.len() {
        // No final newline: take the preceding one, so no blank line stays.
        if start > 0 && bytes[start - 1] == b'\n' {
            start -= 1;
            if start > 0 && bytes[start - 1] == b'\r' {
                start -= 1;
            }
        }
    }
    Some(start..end)
}

/// How readable the profile file `name`'s `display_name` key is.
///
/// `Gone` means no readable file and the name is not a served runtime.
/// Only `Gone` logs, at `warn` with the name only, never the file content;
/// every other read logs nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LabelRead {
    /// The file sets a label.
    Label(String),
    /// The file reads but sets no label.
    NoLabel,
    /// The file no longer parses, including an invalid label value; the
    /// session keeps showing by agent name.
    Broken,
    /// No readable file and not a served runtime.
    Gone,
}

/// The read of the profile file `name`'s `display_name` key.
///
/// The label-only read of `session list`/`inspect` enrichment. It follows
/// the existing `read_profile_text` file rules (owner-secure, bounded
/// UTF-8) with `toml::from_str::<RawProfile>`: no base resolution, no
/// package lookup.
pub(crate) fn display_name_of(registry: &super::ProfileRegistry, name: &str) -> LabelRead {
    let Some(dir) = registry.dir.as_deref() else {
        return LabelRead::NoLabel;
    };
    let path = dir.join(format!("{name}.toml"));
    let Ok(content) = super::read_profile_text(dir, name, &path) else {
        // `RuntimeId` spellings are exactly what the registry resolves; a
        // served name is a runtime with no profile file, never a deleted
        // profile. Anything else missing is `Gone`.
        let is_runtime = name
            .parse::<protocol::RuntimeId>()
            .is_ok_and(|id| registry.runtimes().resolve_id(&id).is_ok());
        return if is_runtime {
            LabelRead::NoLabel
        } else {
            LabelRead::Gone
        };
    };
    let Ok(raw) = toml::from_str::<super::RawProfile>(&content) else {
        return LabelRead::Broken;
    };
    raw.display_name.map_or(LabelRead::NoLabel, |label| {
        LabelRead::Label(label.as_str().to_owned())
    })
}
