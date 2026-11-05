//! The package lifecycle scenarios.
//!
//! They drive the real session registry through `SessionRegistry::packages()`
//! and reach session internals (runtime handles, procwatch, resume), so they
//! live beside the registry rather than in `crate::packages`.

#[cfg(test)]
mod bind_tests;
#[cfg(test)]
mod catalog_tests;
#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod newer_version_tests;
#[cfg(test)]
mod official_alias_tests;
#[cfg(test)]
mod selection_tests;
#[cfg(test)]
mod test_fixture;
#[cfg(test)]
mod tests;
