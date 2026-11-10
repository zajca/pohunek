//! Process matchers shared by every component that recognises a program by its
//! kernel task name or command line.
//!
//! The daemon compiles a manifest's `[process]` section into this type to find
//! agents below a shell, and the session worker compiles the same patterns to
//! decide whether the foreground program of a PTY may be answered, so both
//! apply one matching rule.

// Rust guideline compliant 2026-10-12

use regex::Regex;

use super::ProcessFact;

/// Process-level matchers compiled from a manifest `[process]` section.
#[derive(Debug, Clone)]
pub struct ProcessMatchers {
    comm: Vec<Regex>,
    cmdline: Vec<Regex>,
}

impl ProcessMatchers {
    /// Wraps already compiled matchers.
    #[must_use]
    pub fn new(comm: Vec<Regex>, cmdline: Vec<Regex>) -> Self {
        Self { comm, cmdline }
    }

    /// Compiles matchers from regular-expression sources.
    ///
    /// # Errors
    ///
    /// Returns the first pattern the regex engine rejects.
    pub fn from_patterns<C, L>(comm: C, cmdline: L) -> Result<Self, regex::Error>
    where
        C: IntoIterator,
        C::Item: AsRef<str>,
        L: IntoIterator,
        L::Item: AsRef<str>,
    {
        Ok(Self {
            comm: compile(comm)?,
            cmdline: compile(cmdline)?,
        })
    }

    /// Returns whether process facts match these matchers.
    ///
    /// Matching is an OR: any `comm` regex matching the task command name, or any
    /// `cmdline` regex matching argv joined with spaces, is enough. An empty
    /// matcher set never matches.
    #[must_use]
    pub fn matches(&self, fact: &ProcessFact) -> bool {
        self.comm.iter().any(|regex| regex.is_match(&fact.comm))
            || (!self.cmdline.is_empty()
                && self
                    .cmdline
                    .iter()
                    .any(|regex| regex.is_match(&fact.cmdline.join(" "))))
    }

    /// Whether no matcher is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.comm.is_empty() && self.cmdline.is_empty()
    }

    /// The `comm` regular-expression sources.
    pub fn comm_patterns(&self) -> impl Iterator<Item = &str> {
        self.comm.iter().map(Regex::as_str)
    }

    /// The `cmdline` regular-expression sources.
    pub fn cmdline_patterns(&self) -> impl Iterator<Item = &str> {
        self.cmdline.iter().map(Regex::as_str)
    }
}

fn compile<I>(sources: I) -> Result<Vec<Regex>, regex::Error>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    sources
        .into_iter()
        .map(|source| Regex::new(source.as_ref()))
        .collect()
}
