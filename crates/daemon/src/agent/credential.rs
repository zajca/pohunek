//! Detects credential literals in profile inputs.
//!
//! A host agent profile is plaintext, so a secret that reaches it stays on disk
//! in the clear. The checks here refuse the shapes secrets commonly take: a
//! vendor token prefix, a JWT, a PEM block, an `Authorization` scheme value,
//! and a URL that embeds a password. They are heuristics that fail closed on
//! the listed shapes; a secret of another shape still belongs in `secret_env`,
//! which carries names only.
//!
//! Diagnostics are fixed text. Nothing here returns, formats or logs the value
//! it inspected.

// Rust guideline compliant 2026-11-11

/// Name segments (compared case-insensitively, split on `_` and `-`) that mark
/// an environment variable or a flag as carrying a secret.
const CREDENTIAL_NAME_SEGMENTS: [&str; 8] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
    "COOKIE",
];

/// A vendor token prefix and the fewest token characters that must follow it.
///
/// The minimum tail keeps an ordinary word that starts like a prefix (`sk-learn`)
/// from matching.
struct TokenPrefix {
    prefix: &'static str,
    min_tail: usize,
}

/// Token prefixes that identify a vendor credential.
const TOKEN_PREFIXES: [TokenPrefix; 9] = [
    TokenPrefix {
        prefix: "sk-",
        min_tail: 16,
    },
    TokenPrefix {
        prefix: "ghp_",
        min_tail: 16,
    },
    TokenPrefix {
        prefix: "gho_",
        min_tail: 16,
    },
    TokenPrefix {
        prefix: "github_pat_",
        min_tail: 16,
    },
    TokenPrefix {
        prefix: "glpat-",
        min_tail: 16,
    },
    TokenPrefix {
        prefix: "xoxa-",
        min_tail: 8,
    },
    TokenPrefix {
        prefix: "xoxb-",
        min_tail: 8,
    },
    TokenPrefix {
        prefix: "xoxp-",
        min_tail: 8,
    },
    TokenPrefix {
        prefix: "xoxr-",
        min_tail: 8,
    },
];

/// Prefix of a Google API key, followed by [`GOOGLE_KEY_TAIL`] characters.
const GOOGLE_KEY_PREFIX: &str = "AIza";

/// Characters that follow [`GOOGLE_KEY_PREFIX`] in a Google API key.
const GOOGLE_KEY_TAIL: usize = 35;

/// Prefixes of an AWS access key id.
const AWS_KEY_PREFIXES: [&str; 2] = ["AKIA", "ASIA"];

/// Upper-case alphanumeric characters that follow an AWS key prefix.
const AWS_KEY_TAIL: usize = 16;

/// First characters of a base64url-encoded JWT header (`{"`).
const JWT_HEADER_PREFIX: &str = "eyJ";

/// Number of dot-separated segments of a JWT.
const JWT_SEGMENTS: usize = 3;

/// Opening line of a PEM block.
const PEM_BEGIN: &str = "-----BEGIN";

/// `Authorization` schemes whose value is a secret.
const AUTH_SCHEMES: [&str; 2] = ["bearer ", "basic "];

/// Header prefix that may precede an `Authorization` scheme.
const AUTH_HEADER_PREFIX: &str = "authorization:";

/// Separator between a URL scheme and its authority.
const URL_SCHEME_SEPARATOR: &str = "://";

/// Why a value or a name was refused.
///
/// The text is fixed and safe to show: it names a rule, never the input.
pub(crate) type Problem = &'static str;

/// Whether `name`, an environment variable or a flag name, marks a secret.
///
/// The name is split on `_` and `-`; the name is refused when any segment equals
/// one of [`CREDENTIAL_NAME_SEGMENTS`] ignoring case. The check does not apply
/// to `secret_env` slot names, which are meant to look like `ANTHROPIC_API_KEY`.
pub(crate) fn credential_name_problem(name: &str) -> Option<Problem> {
    name.split(['_', '-'])
        .any(|segment| {
            CREDENTIAL_NAME_SEGMENTS
                .iter()
                .any(|marker| segment.eq_ignore_ascii_case(marker))
        })
        .then_some("names a credential")
}

/// Whether `value` has the shape of a credential literal.
///
/// The whole value and each part of it between `/` and `=` are inspected, so a
/// token inside a path or a `--flag=<token>` form is found.
pub(crate) fn credential_literal_problem(value: &str) -> Option<Problem> {
    whole_value_problem(value).or_else(|| {
        value
            .split(['/', '='])
            .filter(|part| !part.is_empty() && *part != value)
            .find_map(|part| {
                has_auth_scheme(part)
                    .then_some("is an authorization header value")
                    .or_else(|| token_problem(part))
            })
    })
}

/// Checks that apply to the value as a whole.
fn whole_value_problem(value: &str) -> Option<Problem> {
    if value.contains(PEM_BEGIN) {
        return Some("is a PEM block");
    }
    if has_auth_scheme(value) {
        return Some("is an authorization header value");
    }
    if url_embeds_password(value) {
        return Some("is a URL with an embedded password");
    }
    token_problem(value)
}

/// Checks that apply to one token-shaped string.
fn token_problem(token: &str) -> Option<Problem> {
    if is_jwt(token) {
        return Some("is a JWT");
    }
    if has_vendor_token_prefix(token) {
        return Some("has the shape of a vendor token");
    }
    None
}

fn has_auth_scheme(value: &str) -> bool {
    let lowered = value.trim_start().to_ascii_lowercase();
    let lowered = lowered
        .strip_prefix(AUTH_HEADER_PREFIX)
        .map_or(lowered.as_str(), str::trim_start);
    AUTH_SCHEMES.iter().any(|scheme| {
        lowered
            .strip_prefix(scheme)
            .is_some_and(|rest| !rest.trim().is_empty())
    })
}

fn url_embeds_password(value: &str) -> bool {
    let Some((_scheme, rest)) = value.split_once(URL_SCHEME_SEPARATOR) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority
        .rsplit_once('@')
        .and_then(|(userinfo, _host)| userinfo.split_once(':'))
        .is_some_and(|(_user, password)| !password.is_empty())
}

fn is_base64url(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn is_jwt(token: &str) -> bool {
    let segments: Vec<&str> = token.split('.').collect();
    segments.len() == JWT_SEGMENTS
        && segments[0].starts_with(JWT_HEADER_PREFIX)
        && segments.iter().all(|segment| is_base64url(segment))
}

fn is_token_tail(tail: &str) -> bool {
    tail.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn has_vendor_token_prefix(token: &str) -> bool {
    let prefixed = TOKEN_PREFIXES.iter().any(|entry| {
        token
            .strip_prefix(entry.prefix)
            .is_some_and(|tail| tail.len() >= entry.min_tail && is_token_tail(tail))
    });
    let google = token
        .strip_prefix(GOOGLE_KEY_PREFIX)
        .is_some_and(|tail| tail.len() >= GOOGLE_KEY_TAIL && is_token_tail(tail));
    let aws = AWS_KEY_PREFIXES.iter().any(|prefix| {
        token.strip_prefix(prefix).is_some_and(|tail| {
            tail.len() == AWS_KEY_TAIL
                && tail
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
    });
    prefixed || google || aws
}

/// The index of the first launch argument that carries a credential literal.
///
/// An argument is refused when [`credential_literal_problem`] fires on it, or
/// when it is `--<name>=<value>` with a credential-named flag and a non-empty
/// value, or a credential-named `--<name>` followed by an argument that is not
/// itself a flag.
pub(crate) fn credential_arg_problem(args: &[String]) -> Option<usize> {
    for (index, arg) in args.iter().enumerate() {
        if credential_literal_problem(arg).is_some() {
            return Some(index);
        }
        let Some(flag) = arg.strip_prefix("--") else {
            continue;
        };
        let carries_value = match flag.split_once('=') {
            Some((name, value)) => credential_name_problem(name).is_some() && !value.is_empty(),
            None => {
                credential_name_problem(flag).is_some()
                    && args
                        .get(index + 1)
                        .is_some_and(|next| !next.starts_with("--") && !next.is_empty())
            }
        };
        if carries_value {
            return Some(index);
        }
    }
    None
}
