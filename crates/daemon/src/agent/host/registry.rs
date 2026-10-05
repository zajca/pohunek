//! Runtime sources and the registry that resolves runtime identities.

use std::collections::BTreeMap;
use std::sync::Arc;

use protocol::{BindingProvenance, LaunchBinding, ProtocolError, RuntimeId};

use super::definition::{DefinitionError, RuntimeDefinition};

/// Runtime ids only a [`SourceTrust::Builtin`] source may register, except that
/// a [`SourceTrust::Official`] source may take over the three official agents.
///
/// `shell` and the three official agents are reserved so no local or
/// third-party definition can shadow them; only a catalog-authorized package
/// serves an official agent in place of its built-in.
pub const RESERVED_RUNTIME_IDS: [&str; 4] = ["shell", "codex", "claude", "hermes"];

/// How far the registry trusts the definitions a source returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTrust {
    /// Compiled into the daemon; may register reserved ids and must return
    /// built-in provenance.
    Builtin,
    /// Anything else; must not register reserved ids and must return package
    /// provenance.
    External,
    /// A package authorized by the signed official catalog. It returns
    /// package provenance, may register any unreserved id and takes the
    /// official agent ids (never `shell`) over from the built-in runtime,
    /// whatever the order of the sources.
    Official,
}

/// A supplier of runtime definitions.
pub trait RuntimeSource: std::fmt::Debug {
    /// Trust level the registry applies to this source.
    fn trust(&self) -> SourceTrust;

    /// Loads every definition the source provides.
    ///
    /// # Errors
    ///
    /// Returns the first invalid definition.
    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError>;
}

/// Why a registry could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A source returned an invalid definition.
    #[error(transparent)]
    Definition(#[from] DefinitionError),
    /// A source without the authority to claim a reserved id claimed it.
    #[error("runtime id `{runtime_id}` is reserved and the source may not claim it")]
    Reserved {
        /// The claimed id.
        runtime_id: RuntimeId,
    },
    /// Two definitions claim the same id.
    #[error("runtime id `{runtime_id}` is defined more than once")]
    Duplicate {
        /// The contested id.
        runtime_id: RuntimeId,
    },
    /// A definition's provenance does not match its source's trust.
    #[error("runtime `{runtime_id}` has provenance its source is not allowed to provide")]
    ProvenanceMismatch {
        /// The offending id.
        runtime_id: RuntimeId,
    },
}

/// One row of the registry inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryEntry {
    /// Runtime identity.
    pub runtime_id: RuntimeId,
    /// Display name.
    pub display_name: String,
    /// Launch binding a session of this runtime records.
    pub binding: LaunchBinding,
}

/// Resolves runtime identities to definitions.
#[derive(Debug, Clone)]
pub struct RuntimeRegistry {
    definitions: BTreeMap<RuntimeId, Arc<RuntimeDefinition>>,
    /// Built-in definitions an official package serves in place of, by id.
    shadowed: BTreeMap<RuntimeId, Arc<RuntimeDefinition>>,
}

impl RuntimeRegistry {
    /// Builds a registry from `sources`, enforcing reserved ids, provenance
    /// and uniqueness.
    ///
    /// An [`SourceTrust::Official`] definition of an official agent id
    /// replaces the built-in definition of that id, whichever source comes
    /// first; the replaced built-in is kept for [`Self::shadowed_builtin`].
    ///
    /// # Errors
    ///
    /// Returns a [`RegistryError`] for an invalid definition, a reserved id
    /// claimed by a source that may not claim it, a provenance its source may
    /// not provide, or a duplicate id.
    pub fn from_sources(sources: &[&dyn RuntimeSource]) -> Result<Self, RegistryError> {
        let mut definitions = BTreeMap::new();
        let mut official = Vec::new();
        for source in sources {
            let trust = source.trust();
            for definition in source.load()? {
                let runtime_id = definition.runtime_id().clone();
                let builtin = matches!(
                    definition.binding().provenance,
                    BindingProvenance::Builtin { .. }
                );
                if builtin != (trust == SourceTrust::Builtin) {
                    return Err(RegistryError::ProvenanceMismatch { runtime_id });
                }
                match trust {
                    SourceTrust::Official => {
                        if runtime_id.as_str() == RuntimeId::SHELL {
                            return Err(RegistryError::Reserved { runtime_id });
                        }
                        official.push(definition);
                        continue;
                    }
                    SourceTrust::External
                        if RESERVED_RUNTIME_IDS.contains(&runtime_id.as_str()) =>
                    {
                        return Err(RegistryError::Reserved { runtime_id });
                    }
                    SourceTrust::Builtin | SourceTrust::External => {}
                }
                if definitions
                    .insert(runtime_id.clone(), Arc::new(definition))
                    .is_some()
                {
                    return Err(RegistryError::Duplicate { runtime_id });
                }
            }
        }
        let mut shadowed = BTreeMap::new();
        for definition in official {
            let runtime_id = definition.runtime_id().clone();
            let reserved = RESERVED_RUNTIME_IDS.contains(&runtime_id.as_str());
            if let Some(existing) = definitions.get(&runtime_id) {
                let replaceable = reserved
                    && matches!(
                        existing.binding().provenance,
                        BindingProvenance::Builtin { .. }
                    );
                if !replaceable {
                    return Err(RegistryError::Duplicate { runtime_id });
                }
            }
            if let Some(builtin) = definitions.insert(runtime_id.clone(), Arc::new(definition)) {
                shadowed.insert(runtime_id, builtin);
            }
        }
        Ok(Self {
            definitions,
            shadowed,
        })
    }

    /// The built-in definition of `runtime_id` when an official package
    /// serves the id instead.
    #[must_use]
    pub fn shadowed_builtin(&self, runtime_id: &RuntimeId) -> Option<&Arc<RuntimeDefinition>> {
        self.shadowed.get(runtime_id)
    }

    /// Resolves an installed runtime.
    ///
    /// # Errors
    ///
    /// Returns the stable `runtime_not_installed` error when no definition is
    /// registered for `runtime_id`.
    pub fn resolve(
        &self,
        runtime_id: &RuntimeId,
    ) -> Result<&Arc<RuntimeDefinition>, ProtocolError> {
        self.definitions
            .get(runtime_id)
            .ok_or_else(|| ProtocolError::runtime_not_installed(runtime_id))
    }

    /// Iterates every registered definition, ordered by runtime id.
    pub fn definitions(&self) -> impl Iterator<Item = &Arc<RuntimeDefinition>> {
        self.definitions.values()
    }

    /// Lists every registered runtime, ordered by id.
    #[must_use]
    pub fn inventory(&self) -> Vec<InventoryEntry> {
        self.definitions
            .values()
            .map(|definition| InventoryEntry {
                runtime_id: definition.runtime_id().clone(),
                display_name: definition.display_name().to_owned(),
                binding: definition.binding().clone(),
            })
            .collect()
    }
}
