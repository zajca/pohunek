use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to parse manifest TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("manifest source is {size} bytes, max {max}")]
    SourceTooLarge { size: usize, max: usize },
    #[error("manifest contains {count} rules, max {max}")]
    TooManyRules { count: usize, max: usize },
    #[error("manifest contains {count} gates, max {max}")]
    TooManyGates { count: usize, max: usize },
    #[error("manifest contains {count} matchers, max {max}")]
    TooManyMatchers { count: usize, max: usize },
    #[error("manifest gate depth {depth} exceeds max {max}")]
    MaxDepthExceeded { depth: usize, max: usize },
    #[error("rule {rule_id:?} has invalid state {state:?}")]
    InvalidState { rule_id: String, state: String },
    #[error("rule {rule_id:?} has invalid region {region:?}")]
    InvalidRegion { rule_id: String, region: String },
    #[error("rule {rule_id:?} has no gate matchers")]
    MissingGate { rule_id: String },
    #[error("rule {rule_id:?} has invalid regex {pattern:?}: {source}")]
    InvalidRegex {
        rule_id: String,
        pattern: String,
        source: regex::Error,
    },
    #[error("rule {rule_id:?} has {kind:?} matcher of {len} bytes, max {max}")]
    MatcherTooLong {
        rule_id: String,
        kind: MatcherKind,
        len: usize,
        max: usize,
    },
    #[error("rule {rule_id:?} has empty {kind:?} matcher")]
    EmptyMatcher { rule_id: String, kind: MatcherKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherKind {
    Contains,
    Regex,
    LineRegex,
}
