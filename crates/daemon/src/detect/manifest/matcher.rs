use std::collections::HashMap;

use regex::Regex;

use super::{ManifestRegion, MatchContext};

#[derive(Debug)]
pub(super) struct MatchEvaluation<'a> {
    context: &'a MatchContext,
    lowercase_regions: HashMap<ManifestRegion, String>,
}

impl<'a> MatchEvaluation<'a> {
    pub(super) fn new(context: &'a MatchContext) -> Self {
        Self {
            context,
            lowercase_regions: HashMap::new(),
        }
    }

    pub(super) fn has_region_text(&self, region: &ManifestRegion) -> bool {
        self.context.region_text(region).is_some()
    }

    fn region_text(&self, region: &ManifestRegion) -> Option<&str> {
        self.context.region_text(region)
    }

    fn lowercase_region_text(&mut self, region: &ManifestRegion) -> Option<&str> {
        if !self.lowercase_regions.contains_key(region) {
            let text = self.context.region_text(region)?;
            self.lowercase_regions
                .insert(region.clone(), text.to_lowercase());
        }

        self.lowercase_regions.get(region).map(String::as_str)
    }
}

#[derive(Debug, Clone)]
pub(super) enum Gate {
    Contains(String),
    Regex(Regex),
    LineRegex(Regex),
    All(Vec<Gate>),
    Any(Vec<Gate>),
    Not(Box<Gate>),
}

impl Gate {
    pub(super) fn matches(
        &self,
        region: &ManifestRegion,
        evaluation: &mut MatchEvaluation<'_>,
    ) -> bool {
        match self {
            Self::Contains(needle) => evaluation
                .lowercase_region_text(region)
                .is_some_and(|text| text.contains(needle)),
            Self::Regex(regex) => evaluation
                .region_text(region)
                .is_some_and(|text| regex.is_match(text)),
            Self::LineRegex(regex) => evaluation
                .region_text(region)
                .is_some_and(|text| text.lines().any(|line| regex.is_match(line))),
            Self::All(gates) => gates.iter().all(|gate| gate.matches(region, evaluation)),
            Self::Any(gates) => gates.iter().any(|gate| gate.matches(region, evaluation)),
            Self::Not(gate) => !gate.matches(region, evaluation),
        }
    }

    pub(super) fn has_matcher(&self) -> bool {
        match self {
            Self::Contains(_) | Self::Regex(_) | Self::LineRegex(_) => true,
            Self::All(gates) | Self::Any(gates) => gates.iter().any(Gate::has_matcher),
            Self::Not(gate) => gate.has_matcher(),
        }
    }
}
