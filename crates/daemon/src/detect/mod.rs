//! Synchronous activity detector pipeline.
//!
//! Per `docs/plan-phase-1.md` "State Engine": this module coordinates OSC
//! parsing, VT screen extraction, manifest matching, and a debounced state
//! machine. Session code can own a detector without coupling the pipeline to
//! async runtime concerns.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use protocol::{AgentActivity, DetectionRegionPreview, ProtocolError, RuntimeRef, StateSource};

use crate::agent::host::{InputReadiness, RuntimeDefinition, RuntimeHost};
use crate::procwatch::ProcessFact;

mod machine;
mod manifest;
mod osc;

pub use machine::{ActivityEvidence, ActivityTransition, DetectionConfig, StateMachine};
pub(crate) use manifest::MAX_MANIFEST_SOURCE_BYTES;
pub use manifest::{
    Manifest, ManifestError, ManifestMatch, ManifestRegion, MatchContext, MatcherKind,
    ProcessMatchers,
};
pub use osc::{OscEvidence, OscParser};
// The VT screen model lives in `pohunek-terminal` so the daemon and CLI share
// one implementation instead of drifting copies.
pub use pohunek_terminal::{ScreenObservation, ScreenRegion, ScreenTracker};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Generic shell activity manifest shipped with the daemon.
///
/// Embedded at compile time so the production detector always has a manifest to
/// drive the screen/manifest layer. It is a trusted, unit-tested constant (not
/// user input), so parsing it with `.expect` at startup is acceptable.
const GENERIC_SHELL_MANIFEST: &str = include_str!("manifests/shell.toml");

/// Compiled manifest sources checked by daemon readiness diagnostics, each
/// with the runtime id of the built-in runtime it belongs to.
pub(crate) fn embedded_manifest_sources() -> [(&'static str, &'static str); 1] {
    [(protocol::RuntimeId::SHELL, GENERIC_SHELL_MANIFEST)]
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectorConfig {
    pub detection: DetectionConfig,
    /// The screen manifest, shared read-only by every detector of a runtime.
    ///
    /// Sharing keeps the compiled rules and their lazy-DFA caches warm across
    /// detectors: a cloned `regex::Regex` starts with an empty cache pool and
    /// rebuilds its DFA states on its first screens. `Regex` is `Send + Sync`
    /// and its cache pool hands each concurrent search its own cache, so
    /// sessions on different threads can match through one manifest at once.
    pub manifest: Option<Arc<Manifest>>,
    /// Which evidence makes the detector report editable input as ready.
    pub input_readiness: InputReadiness,
}

impl DetectorConfig {
    /// Production detector config using the embedded generic shell manifest.
    #[must_use]
    pub fn generic_shell() -> Self {
        Self {
            detection: DetectionConfig::default(),
            manifest: Some(Arc::clone(shared_generic_shell_manifest())),
            input_readiness: InputReadiness::default(),
        }
    }

    /// Production detector config for the runtime `agent` names: that
    /// runtime's manifest, or the generic shell manifest when no installed
    /// runtime backs the kind.
    #[must_use]
    pub fn for_agent(host: &RuntimeHost, agent: &RuntimeRef) -> Self {
        host.resolve_ref(agent).map_or_else(
            |_unresolved| Self::generic_shell(),
            |definition| Self::for_definition(&definition),
        )
    }

    /// Detector config for a session launched with `pin`: the override
    /// manifest when there is one, else the manifest of the definition the pin
    /// resolves to, else the generic shell manifest. An override manifest keeps
    /// the readiness of the definition the pin resolves to, as far as the
    /// override can report it ([`InputReadiness::for_manifest`]).
    #[must_use]
    pub fn for_pinned(
        host: &RuntimeHost,
        base: &RuntimeRef,
        pin: &crate::agent::host::LaunchPin,
        override_manifest: Option<Manifest>,
    ) -> Self {
        match override_manifest {
            Some(manifest) => Self {
                detection: DetectionConfig::default(),
                input_readiness: readiness_of(host.definition_for_pin(base, pin))
                    .for_manifest(&manifest),
                manifest: Some(Arc::new(manifest)),
            },
            None => host.definition_for_pin(base, pin).map_or_else(
                |_unresolved| Self::generic_shell(),
                |definition| Self::for_definition(&definition),
            ),
        }
    }

    /// The configuration of a session whose profile override and resolved
    /// definition are already known: the override wins, else the definition's
    /// manifest, else the generic shell manifest. An override manifest keeps
    /// the readiness of the resolved definition.
    #[must_use]
    pub fn for_resolution(
        override_manifest: Option<Manifest>,
        definition: Option<&RuntimeDefinition>,
    ) -> Self {
        match (override_manifest, definition) {
            (Some(manifest), definition) => Self {
                detection: DetectionConfig::default(),
                manifest: Some(Arc::new(manifest)),
                input_readiness: definition
                    .map_or_else(InputReadiness::default, RuntimeDefinition::input_readiness),
            },
            (None, Some(definition)) => Self::for_definition(definition),
            (None, None) => Self::generic_shell(),
        }
    }

    /// Production detector config driven by `definition`'s manifest and
    /// input readiness.
    #[must_use]
    pub fn for_definition(definition: &RuntimeDefinition) -> Self {
        Self {
            detection: DetectionConfig::default(),
            manifest: Some(Arc::clone(definition.manifest())),
            input_readiness: definition.input_readiness(),
        }
    }

    /// Detector config for a host agent profile (Part C): use the profile's
    /// override manifest when it declares one, else inherit the base kind's
    /// manifest. The override is already parsed via the capped,
    /// non-panicking [`Manifest::parse_str`] (a malformed one disabled the profile
    /// before this point), so detection never `.expect`-panics on host input.
    #[must_use]
    pub fn for_profile(
        host: &RuntimeHost,
        base: &RuntimeRef,
        override_manifest: Option<Manifest>,
    ) -> Self {
        match override_manifest {
            Some(manifest) => Self {
                detection: DetectionConfig::default(),
                input_readiness: readiness_of(host.resolve_ref(base)).for_manifest(&manifest),
                manifest: Some(Arc::new(manifest)),
            },
            None => Self::for_agent(host, base),
        }
    }
}

/// The input readiness of a resolved definition, or the default when the
/// reference does not resolve.
fn readiness_of(
    definition: Result<std::sync::Arc<RuntimeDefinition>, ProtocolError>,
) -> InputReadiness {
    definition.map_or_else(
        |_unresolved| InputReadiness::default(),
        |definition| definition.input_readiness(),
    )
}

/// The parsed embedded generic shell manifest.
#[must_use]
pub fn generic_shell_manifest() -> &'static Manifest {
    shared_generic_shell_manifest()
}

/// Parses the embedded generic shell manifest once; every shell detector shares it.
///
/// The manifest is a shipped, trusted, unit-tested constant, so a parse failure
/// is a programming error rather than a recoverable condition.
pub(crate) fn shared_generic_shell_manifest() -> &'static Arc<Manifest> {
    static MANIFEST: OnceLock<Arc<Manifest>> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        Arc::new(
            Manifest::parse_str(GENERIC_SHELL_MANIFEST)
                .expect("generic shell manifest must parse")
                .without_working_latch(),
        )
    })
}

/// Identifies the runtime a process belongs to from the process matchers its
/// installed definitions declare.
///
/// Definitions are tried in runtime-id order, so a process that two runtimes
/// both claim resolves deterministically.
#[must_use]
pub fn identify_agent(host: &RuntimeHost, fact: &ProcessFact) -> Option<RuntimeRef> {
    identify_definition(host, fact)
        .map(|definition| RuntimeRef::from(definition.runtime_id().clone()))
}

/// [`identify_agent`] returning the definition whose matchers matched, so the
/// caller applies that definition's rules to the process.
#[must_use]
pub fn identify_definition(
    host: &RuntimeHost,
    fact: &ProcessFact,
) -> Option<std::sync::Arc<RuntimeDefinition>> {
    host.registry()
        .definitions()
        .find(|definition| {
            definition
                .manifest()
                .process_matchers()
                .is_some_and(|matchers| matchers.matches(fact))
        })
        .map(std::sync::Arc::clone)
}

#[derive(Debug)]
pub struct Detector {
    osc: OscParser,
    screen: ScreenTracker,
    state: StateMachine,
    manifest: Option<Arc<Manifest>>,
    input_readiness: InputReadiness,
    initial_input_pending: bool,
    latest_title: Option<String>,
    latest_progress: Option<String>,
    latest_cwd_hint: Option<String>,
    process_activity: ProcessActivityScanner,
    needs_visible_recheck: bool,
    /// Whether terminal context or the active manifest changed since the last
    /// tick evaluated it. A tick over unchanged context yields the same
    /// evidence, so it skips the manifest evaluation.
    context_dirty: bool,
    /// Monotonic sequence of evidence arrivals that the strict `manifest_idle`
    /// latch judges freshness with: every received OSC title (even one that
    /// repeats its value) and every screen text change or character redraw
    /// gets a fresh point. The debounced classification keeps its own
    /// value-changed `ContextFreshness` flags, so activity semantics are
    /// unchanged.
    strict_seq: u64,
    title_seq: u64,
    screen_seq: u64,
    /// Sequence point of the most recent visible-blocker sighting under
    /// `manifest_idle` readiness (0 = none seen). Once a blocker is seen,
    /// only idle evidence from a source that arrived *after* the latest
    /// sighting can release the input: a cached idle title from before the
    /// blocker — or a title received while the blocker still held — never
    /// does.
    idle_barrier: u64,
    /// Manifest evaluations the working latch ran at intermediate points.
    #[cfg(test)]
    latch_evaluations: usize,
    /// Chunks in which the working latch recorded a sighting.
    #[cfg(test)]
    latch_sightings: usize,
}

impl Detector {
    #[must_use]
    pub fn new(rows: u16, cols: u16, started_at: Instant, config: DetectorConfig) -> Self {
        Self {
            osc: OscParser::new(),
            screen: ScreenTracker::new(rows, cols),
            state: StateMachine::new(started_at, config.detection),
            manifest: config.manifest,
            input_readiness: config.input_readiness,
            initial_input_pending: true,
            latest_title: None,
            latest_progress: None,
            latest_cwd_hint: None,
            process_activity: ProcessActivityScanner::default(),
            needs_visible_recheck: false,
            context_dirty: true,
            strict_seq: 0,
            title_seq: 0,
            screen_seq: 0,
            idle_barrier: 0,
            #[cfg(test)]
            latch_evaluations: 0,
            #[cfg(test)]
            latch_sightings: 0,
        }
    }

    /// Ends the initial-input wait. Ordered startup observation stops with it
    /// unless the manifest latches working sightings, which keep it armed.
    ///
    /// Reconfiguration never re-arms a completed wait.
    pub fn finish_initial_input(&mut self) {
        if !self.initial_input_pending {
            return;
        }
        self.initial_input_pending = false;
        // An agent manifest keeps the ordered observer alive for the working
        // latch: its parser must see every byte to stay in step with the
        // stream.
        if !self.latch_eligible() {
            self.screen.finish_observation();
        }
        self.context_dirty = true;
    }

    pub fn feed(&mut self, now: Instant, bytes: &[u8]) -> Vec<ActivityTransition> {
        // A reconfigure or resize may expose a blocker before the next
        // output clears it. Observe that context before consuming new bytes.
        self.sight_full_context();
        self.context_dirty = true;

        let has_process_activity = self.process_activity.observe(bytes);
        let recent_before = self.screen.recent_text();
        let osc_events = self.osc.advance_with_offsets(bytes);

        // The screen is fed segment by segment, split at each completed OSC
        // event: every event is stamped against the screen as it looked where
        // that event's bytes ended, never against the chunk's final screen —
        // a title followed by a clear in one chunk arrived while the blocker
        // still held.
        //
        // Under an agent manifest, a `Working` winner at one of those points is
        // remembered: a turn whose working title or frame is replaced by an idle
        // one inside the same chunk is still working evidence.
        let mut latch = ChunkLatch::new(self.latch_gate_open(), recent_before.clone());
        let mut consumed = 0;
        for (item, end) in osc_events {
            self.feed_screen_segment(&bytes[consumed..end], &mut latch);
            if self.apply_osc_event(item, &mut latch.osc_changes) {
                self.sight_full_context();
                self.latch_at_osc_point(&mut latch);
            }
            consumed = end;
        }
        self.feed_screen_segment(&bytes[consumed..], &mut latch);
        #[cfg(test)]
        self.record_latch_counters(&latch);
        let osc_changes = latch.osc_changes;
        let sighted_working = latch.sighted;

        // Overall screen change keeps the debounced classification's
        // value-changed freshness semantics, unchanged for activity.
        let screen_changed = self.screen.recent_text() != recent_before;
        let freshness = ContextFreshness {
            screen: screen_changed,
            osc_title: osc_changes.title,
            osc_progress: osc_changes.progress,
        };
        let hit = self.manifest_evidence(freshness);
        let has_structured_evidence = hit.is_some();

        let mut transitions = Vec::new();
        // Only a chunk that ends idle latches: transcript text drawn ahead of a
        // dialog, or of a screen no rule names, must never read as working.
        if let Some(sighted) = sighted_working {
            if hit
                .as_ref()
                .is_some_and(|hit| hit.evidence.activity == AgentActivity::Idle)
            {
                push_transition(&mut transitions, self.state.observe_evidence(now, sighted));
            }
        }
        if let Some(hit) = hit {
            push_transition(
                &mut transitions,
                self.state.observe_evidence(now, hit.evidence),
            );
        }

        if has_process_activity && !has_structured_evidence {
            push_transition(&mut transitions, self.state.observe_bytes(now));
        }

        transitions
    }

    #[cfg(test)]
    fn record_latch_counters(&mut self, latch: &ChunkLatch) {
        self.latch_evaluations += latch.evaluations;
        self.latch_sightings += usize::from(latch.sighted.is_some());
    }

    /// Whether the manifest takes part in the working latch.
    fn latch_eligible(&self) -> bool {
        self.manifest
            .as_ref()
            .is_some_and(|manifest| manifest.latches_working())
    }

    /// Whether this chunk evaluates intermediate working sightings.
    ///
    /// Evaluation stops while `Working` is already published, which keeps busy
    /// output on the plain feed path.
    fn latch_gate_open(&self) -> bool {
        self.latch_eligible() && self.state.published_activity() != Some(AgentActivity::Working)
    }

    /// Evaluates the context after a completed OSC event.
    fn latch_at_osc_point(&self, latch: &mut ChunkLatch) {
        if let Some(manifest) = self.manifest.as_deref() {
            latch.evaluate(
                manifest,
                &self.screen,
                self.latest_title.as_deref(),
                self.latest_progress.as_deref(),
                false,
            );
        }
    }

    /// Feeds a segment through the ordered screen observer and evaluates the
    /// working latch at every observation point. Returns whether characters
    /// were drawn.
    fn feed_screen_segment_latched(&mut self, bytes: &[u8], latch: &mut ChunkLatch) -> bool {
        let Detector {
            screen,
            manifest,
            latest_title,
            latest_progress,
            ..
        } = self;
        let Some(manifest) = manifest.as_deref() else {
            return screen.feed(bytes);
        };
        let mut drew = false;
        screen.feed_observed(bytes, &mut |screen: &ScreenTracker, observed| {
            drew |= observed.drew_chars;
            latch.evaluate(
                manifest,
                screen,
                latest_title.as_deref(),
                latest_progress.as_deref(),
                observed.drew_chars,
            );
        });
        drew
    }

    /// Feeds a segment and stamps text changes or character redraws.
    ///
    /// A clear followed by an identical repaint is fresh screen evidence even
    /// when the segment's final text equals its initial text. Control-only
    /// output does not refresh the cached screen's arrival sequence.
    ///
    /// Under strict `manifest_idle` readiness the segment is fed observed
    /// instead: pending text changes are sighted before cursor movement, erasure
    /// or potential character wrapping can hide them. Formatting does not
    /// split printable runs into repeated grid observations. A complete
    /// blocker is recorded before its clear, and the final grid is observed
    /// even when the final byte is itself a control action.
    fn feed_screen_segment(&mut self, bytes: &[u8], latch: &mut ChunkLatch) {
        if !self.initial_input_pending {
            if latch.open {
                self.feed_screen_segment_latched(bytes, latch);
            } else {
                self.screen.feed(bytes);
            }
            return;
        }
        if self.input_readiness == InputReadiness::ManifestIdle {
            self.feed_screen_segment_observed(bytes, latch);
            return;
        }
        let recent_before = self.screen.recent_text();
        let drew_chars = if latch.open {
            self.feed_screen_segment_latched(bytes, latch)
        } else {
            self.screen.feed(bytes)
        };
        if self.screen.recent_text() != recent_before || drew_chars {
            self.strict_seq += 1;
            self.screen_seq = self.strict_seq;
        }
        // Screen evidence received while a title blocker still holds must
        // not become fresh merely because the following OSC clears it.
        self.sight_full_context();
    }

    /// Feeds one pending-startup segment through [`ScreenTracker::feed_observed`].
    ///
    /// The observer borrows the detector's sequence counters and OSC context
    /// field by field, so the screen tracker can be fed while the observer
    /// keeps mutating the counters.
    fn feed_screen_segment_observed(&mut self, bytes: &[u8], latch: &mut ChunkLatch) {
        let Detector {
            screen,
            strict_seq,
            screen_seq,
            idle_barrier,
            manifest,
            latest_title,
            latest_progress,
            ..
        } = self;
        let mut observer = ReadinessObserver {
            manifest: manifest.as_deref(),
            latest_title: latest_title.as_deref(),
            latest_progress: latest_progress.as_deref(),
            title_arrival: self.title_seq,
            strict_seq: &mut *strict_seq,
            screen_seq: &mut *screen_seq,
            idle_barrier: &mut *idle_barrier,
            baseline: screen.recent_text(),
        };
        screen.feed_observed(bytes, &mut |screen: &ScreenTracker, observed| {
            observer.observe(screen, observed);
            if let Some(manifest) = manifest.as_deref() {
                latch.evaluate(
                    manifest,
                    screen,
                    latest_title.as_deref(),
                    latest_progress.as_deref(),
                    observed.drew_chars,
                );
            }
        });
    }

    /// Applies one completed OSC event in stream order and stamps its arrival
    /// sequence point. A repeated identical value still counts as a fresh
    /// arrival; the value-changed flags continue to drive the debounced
    /// classification. Returns whether the event is latch-relevant evidence.
    fn apply_osc_event(&mut self, item: OscEvidence, changes: &mut OscChanges) -> bool {
        match item {
            OscEvidence::Title(title) => {
                changes.title |= self.latest_title.as_deref() != Some(title.as_str());
                self.latest_title = Some(title);
                self.strict_seq += 1;
                self.title_seq = self.strict_seq;
                true
            }
            OscEvidence::Progress(progress) => {
                changes.progress |= self.latest_progress.as_deref() != Some(progress.as_str());
                self.latest_progress = Some(progress);
                true
            }
            OscEvidence::Cwd(path) => {
                self.latest_cwd_hint = Some(path);
                false
            }
        }
    }

    pub fn tick(&mut self, now: Instant) -> Vec<ActivityTransition> {
        let mut transitions = Vec::new();

        // Working evidence is left to the byte/process path; tick only reconfirms
        // debounced idle/blocked states and refreshes stable visible states. A
        // runtime reconfigure asks for one full visible-context recheck so cached
        // screen/OSC context can publish under the newly active manifest.
        // A pending candidate needs repeated observations to confirm, so it
        // keeps evaluating even over unchanged context.
        if self.context_dirty || self.needs_visible_recheck || self.state.has_pending() {
            if let Some(hit) = self.manifest_evidence(ContextFreshness::all()) {
                self.sight_visible_blocker(&hit);
                if hit.evidence.activity != AgentActivity::Working || self.needs_visible_recheck {
                    push_transition(
                        &mut transitions,
                        self.state.observe_evidence(now, hit.evidence),
                    );
                }
            }
            self.context_dirty = false;
        }
        self.needs_visible_recheck = false;
        push_transition(&mut transitions, self.state.tick(now));
        transitions
    }

    /// Whether terminal output identifies an editable agent input reader.
    #[must_use]
    pub fn input_ready(&self) -> bool {
        // Manifest-based readiness uses the winning evidence from the full
        // current context: a visible blocker (such as a startup update popup)
        // outranks a stale idle title, and no evidence at all — the gap before
        // the agent's first title — is never readiness. A `manifest_idle`
        // session additionally requires the winning idle evidence to be
        // newer than the most recent visible-blocker sighting, so a screen
        // clear alone, or the idle title cached from before the blocker, can
        // never release the latched prompt.
        let idle_hit = || {
            self.manifest_evidence(ContextFreshness::all())
                .filter(|hit| hit.evidence.activity == AgentActivity::Idle)
        };
        let screen_idle =
            || idle_hit().is_some_and(|hit| hit.evidence.source == StateSource::Screen);
        match self.input_readiness {
            InputReadiness::BracketedPaste => self.screen.bracketed_paste() || screen_idle(),
            InputReadiness::ScreenIdle => screen_idle(),
            InputReadiness::ManifestIdle => idle_hit().is_some_and(|hit| {
                matches!(
                    hit.evidence.source,
                    StateSource::Screen | StateSource::OscTitle
                ) && hit.arrival > self.idle_barrier
            }),
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.context_dirty = true;
        self.screen.resize(rows, cols);
        self.sight_full_context();
    }

    /// Switches detector tuning and manifest without clearing terminal context.
    pub fn reconfigure(&mut self, now: Instant, config: DetectorConfig) {
        self.context_dirty = true;
        self.state = StateMachine::new(now, config.detection);
        self.manifest = config.manifest;
        self.input_readiness = config.input_readiness;
        if self.latch_eligible() {
            self.screen.resume_observation();
        } else if !self.initial_input_pending {
            self.screen.finish_observation();
        }
        self.process_activity.reset();
        self.state.clear_pending();
        self.needs_visible_recheck = true;
        self.sight_full_context();
    }

    /// Recovers detector state after dropped PTY output.
    ///
    /// A skipped broadcast chunk may have severed an in-flight escape sequence,
    /// so we discard the OSC parser state, the process-activity scanner state,
    /// the cached OSC title/progress, bracketed-paste mode, and the `vt100`
    /// screen grid rather than trust any of them. Fresh state repaints on the
    /// next agent refresh.
    pub fn resync_after_lag(&mut self) {
        self.osc.reset();
        self.process_activity.reset();
        self.state.clear_pending();
        self.context_dirty = true;
        self.latest_title = None;
        self.latest_progress = None;
        self.latest_cwd_hint = None;
        self.screen.reset();
        self.needs_visible_recheck = false;
        // The context these sequence points name is gone; only fresh arrivals
        // from here on count. The strict latch itself stays armed so its
        // blocker invariant survives the resync.
        self.title_seq = 0;
        self.screen_seq = 0;
    }

    #[must_use]
    pub fn latest_title(&self) -> Option<&str> {
        self.latest_title.as_deref()
    }

    #[must_use]
    pub fn latest_progress(&self) -> Option<&str> {
        self.latest_progress.as_deref()
    }

    /// Takes the latest OSC 7 cwd hint, if one has arrived.
    pub fn take_cwd_hint(&mut self) -> Option<String> {
        self.latest_cwd_hint.take()
    }

    /// Records only the full-context winner for strict input readiness.
    fn sight_full_context(&mut self) {
        if !self.initial_input_pending || self.input_readiness != InputReadiness::ManifestIdle {
            return;
        }
        if let Some(hit) = self.manifest_evidence(ContextFreshness::all()) {
            self.sight_visible_blocker(&hit);
        }
    }

    /// Records a visible blocker sighting: from now on, idle evidence must be
    /// newer than this sighting to release a `manifest_idle` input. A later
    /// sighting (a redrawn popup, a blocker that outlives other evidence)
    /// replaces the barrier, so a fresh title or screen update received while
    /// the blocker still holds cannot arm future readiness.
    fn sight_visible_blocker(&mut self, hit: &ManifestEvidenceHit) {
        if self.initial_input_pending
            && self.input_readiness == InputReadiness::ManifestIdle
            && hit.evidence.activity == AgentActivity::Blocked
            && hit.visible_blocker
        {
            self.idle_barrier = self.strict_seq;
        }
    }

    /// Renders every region required by the active manifest.
    #[must_use]
    pub fn region_previews(&self) -> Vec<DetectionRegionPreview> {
        let Some(manifest) = &self.manifest else {
            return Vec::new();
        };
        let context = self.match_context(manifest, ContextFreshness::all());

        manifest
            .required_regions()
            .into_iter()
            .map(|region| DetectionRegionPreview {
                kind: region.kind(),
                region: region.to_string(),
                text: context.region_text(&region).unwrap_or_default().to_owned(),
            })
            .collect()
    }

    fn manifest_evidence(&self, freshness: ContextFreshness) -> Option<ManifestEvidenceHit> {
        self.manifest.as_ref().and_then(|manifest| {
            manifest_hit(
                manifest,
                &self.screen,
                self.latest_title.as_deref(),
                self.latest_progress.as_deref(),
                self.title_seq,
                self.screen_seq,
                freshness,
            )
        })
    }

    fn match_context(&self, manifest: &Manifest, freshness: ContextFreshness) -> MatchContext {
        match_context_for(
            manifest,
            &self.screen,
            self.latest_title.as_deref(),
            self.latest_progress.as_deref(),
            freshness,
        )
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct OscChanges {
    title: bool,
    progress: bool,
}

/// The working latch of one `feed` call: a `Working` winner seen at an
/// intermediate point of the chunk, before a later point hid it.
struct ChunkLatch {
    /// Whether further points are still evaluated: the gate was open at the
    /// chunk start and nothing was sighted yet.
    open: bool,
    /// The screen text at the chunk start, for screen freshness.
    recent_before: String,
    /// Title and progress changes among the bytes consumed so far.
    osc_changes: OscChanges,
    /// Whether characters were drawn at an earlier point of this chunk.
    drew: bool,
    evaluations: usize,
    sighted: Option<ActivityEvidence>,
}

impl ChunkLatch {
    fn new(open: bool, recent_before: String) -> Self {
        Self {
            open,
            recent_before,
            osc_changes: OscChanges::default(),
            drew: false,
            evaluations: 0,
            sighted: None,
        }
    }

    /// Evaluates the manifest over the context at one point with the freshness
    /// of the bytes consumed so far, so a cached title never latches on a
    /// repaint and a visible blocker still outranks a stale spinner. A
    /// `Working` winner is kept and closes the latch.
    fn evaluate(
        &mut self,
        manifest: &Manifest,
        screen: &ScreenTracker,
        latest_title: Option<&str>,
        latest_progress: Option<&str>,
        drew_chars: bool,
    ) {
        if !self.open {
            return;
        }
        self.evaluations += 1;
        self.drew |= drew_chars;
        let freshness = ContextFreshness {
            screen: self.drew || screen.recent_text() != self.recent_before,
            osc_title: self.osc_changes.title,
            osc_progress: self.osc_changes.progress,
        };
        let hit = manifest_hit(
            manifest,
            screen,
            latest_title,
            latest_progress,
            0,
            0,
            freshness,
        );
        if let Some(hit) = hit.filter(|hit| hit.evidence.activity == AgentActivity::Working) {
            self.sighted = Some(hit.evidence);
            self.open = false;
        }
    }
}

/// Builds the manifest match context over explicit context pieces.
///
/// A pure, shared counterpart of [`Detector::match_context`]: the observed
/// feed's observer borrows the detector field by field (the screen is being
/// fed through the same borrow set), so both the whole-detector path and the
/// observer must build one context the same way.
fn match_context_for(
    manifest: &Manifest,
    screen: &ScreenTracker,
    latest_title: Option<&str>,
    latest_progress: Option<&str>,
    freshness: ContextFreshness,
) -> MatchContext {
    let mut context = MatchContext::default();

    for region in manifest.required_regions() {
        if !freshness.includes(&region) {
            continue;
        }

        context = match region {
            ManifestRegion::OscTitle => {
                if let Some(title) = latest_title {
                    context.with_region_text(ManifestRegion::OscTitle, title.to_owned())
                } else {
                    context
                }
            }
            ManifestRegion::OscProgress => {
                if let Some(progress) = latest_progress {
                    context.with_region_text(ManifestRegion::OscProgress, progress.to_owned())
                } else {
                    context
                }
            }
            ManifestRegion::WholeRecent => {
                context.with_region_text(ManifestRegion::WholeRecent, screen.recent_text())
            }
            ManifestRegion::BottomLines(count) => context.with_region_text(
                ManifestRegion::BottomLines(count),
                region_text(&screen.bottom_lines(count)),
            ),
            ManifestRegion::BottomNonEmptyLines(count) => context.with_region_text(
                ManifestRegion::BottomNonEmptyLines(count),
                region_text(&screen.bottom_non_empty_lines(count)),
            ),
            ManifestRegion::TopNonEmptyLines(count) => context.with_region_text(
                ManifestRegion::TopNonEmptyLines(count),
                region_text(&screen.top_non_empty_lines(count)),
            ),
            ManifestRegion::LastNonEmptyAbovePromptBox => context.with_region_text(
                ManifestRegion::LastNonEmptyAbovePromptBox,
                screen.last_non_empty_above_prompt_box(),
            ),
            ManifestRegion::AfterLastPromptMarker => context.with_region_text(
                ManifestRegion::AfterLastPromptMarker,
                screen.after_last_prompt_marker(),
            ),
            ManifestRegion::PromptBoxBody => {
                context.with_region_text(ManifestRegion::PromptBoxBody, screen.prompt_box_body())
            }
            ManifestRegion::AfterLastHorizontalRule => context.with_region_text(
                ManifestRegion::AfterLastHorizontalRule,
                screen.after_last_horizontal_rule(),
            ),
        };
    }

    context
}

/// The winning manifest evidence over explicit context pieces: the pure,
/// shared counterpart of [`Detector::manifest_evidence`], for the same
/// borrow-split reason as [`match_context_for`].
fn manifest_hit(
    manifest: &Manifest,
    screen: &ScreenTracker,
    latest_title: Option<&str>,
    latest_progress: Option<&str>,
    title_arrival: u64,
    screen_arrival: u64,
    freshness: ContextFreshness,
) -> Option<ManifestEvidenceHit> {
    let context = match_context_for(manifest, screen, latest_title, latest_progress, freshness);
    let matched = manifest.match_context(&context)?;
    let source = manifest_source(&matched.region);
    if matched.activity == AgentActivity::Blocked
        && matched.visible_blocker
        && !is_visible_manifest_source(source)
    {
        return None;
    }

    // Arrival sequence for eligible idle sources; other sources cannot
    // release strict initial input.
    let arrival = match source {
        StateSource::OscTitle => title_arrival,
        StateSource::Screen => screen_arrival,
        StateSource::OscProgress
        | StateSource::Process
        | StateSource::Report
        | StateSource::Unknown => 0,
    };

    Some(ManifestEvidenceHit {
        evidence: ActivityEvidence {
            activity: matched.activity,
            source,
        },
        visible_blocker: matched.visible_blocker,
        arrival,
    })
}

/// Observer behind one observed screen segment under strict `manifest_idle`
/// readiness.
///
/// It stamps screen freshness into the detector's own sequence counters and
/// re-arms the idle barrier when the full context at an observation names a
/// visible blocker — the same barrier the whole-detector path keeps — using
/// only disjoint borrows so the screen can be fed while it writes.
struct ReadinessObserver<'sight> {
    manifest: Option<&'sight Manifest>,
    latest_title: Option<&'sight str>,
    latest_progress: Option<&'sight str>,
    title_arrival: u64,
    strict_seq: &'sight mut u64,
    screen_seq: &'sight mut u64,
    idle_barrier: &'sight mut u64,
    baseline: String,
}

impl ReadinessObserver<'_> {
    /// Observes the grid at one control-action boundary (or the segment
    /// tail): freshness comes from the text the boundary sees or a redraw
    /// since the previous observation, and the barrier re-arms on a visible
    /// blocker.
    fn observe(&mut self, screen: &ScreenTracker, observed: ScreenObservation) {
        let text = screen.recent_text();
        let changed = text != self.baseline || observed.drew_chars;
        self.baseline = text;
        // OSC changes are sighted by the outer pipeline. An unchanged grid
        // cannot introduce a blocker between those ordered OSC arrivals.
        if !changed {
            return;
        }
        *self.strict_seq += 1;
        *self.screen_seq = *self.strict_seq;

        let hit = self.manifest.and_then(|manifest| {
            manifest_hit(
                manifest,
                screen,
                self.latest_title,
                self.latest_progress,
                self.title_arrival,
                *self.screen_seq,
                ContextFreshness::all(),
            )
        });
        if hit.is_some_and(|hit| {
            hit.evidence.activity == AgentActivity::Blocked && hit.visible_blocker
        }) {
            *self.idle_barrier = *self.strict_seq;
        }
    }
}

/// A manifest match with the strict-latch inputs `input_ready` and the feed
/// path need: the winning evidence, whether it names a visible blocker, and
/// the arrival sequence of the winning source.
#[derive(Debug, Clone, Copy)]
struct ManifestEvidenceHit {
    evidence: ActivityEvidence,
    visible_blocker: bool,
    /// Sequence point of the winning source's most recent real arrival: the
    /// title's receipt for `osc_title`, the last screen text change for the
    /// screen regions.
    arrival: u64,
}

#[derive(Debug, Clone, Copy)]
struct ContextFreshness {
    screen: bool,
    osc_title: bool,
    osc_progress: bool,
}

impl ContextFreshness {
    fn all() -> Self {
        Self {
            screen: true,
            osc_title: true,
            osc_progress: true,
        }
    }

    fn includes(self, region: &ManifestRegion) -> bool {
        match region {
            ManifestRegion::OscTitle => self.osc_title,
            ManifestRegion::OscProgress => self.osc_progress,
            ManifestRegion::WholeRecent
            | ManifestRegion::BottomLines(_)
            | ManifestRegion::BottomNonEmptyLines(_)
            | ManifestRegion::TopNonEmptyLines(_)
            | ManifestRegion::LastNonEmptyAbovePromptBox
            | ManifestRegion::AfterLastPromptMarker
            | ManifestRegion::PromptBoxBody
            | ManifestRegion::AfterLastHorizontalRule => self.screen,
        }
    }
}

#[derive(Debug, Default)]
struct ProcessActivityScanner {
    state: ProcessActivityState,
}

impl ProcessActivityScanner {
    fn observe(&mut self, bytes: &[u8]) -> bool {
        let mut has_activity = false;

        for &byte in bytes {
            match self.state {
                ProcessActivityState::Ground => match byte {
                    ESC => self.state = ProcessActivityState::Escape,
                    _ => has_activity = true,
                },
                ProcessActivityState::Escape => match byte {
                    b']' => self.state = ProcessActivityState::OscString,
                    ESC => self.state = ProcessActivityState::Escape,
                    _ => {
                        self.state = ProcessActivityState::Ground;
                        has_activity = true;
                    }
                },
                ProcessActivityState::OscString => match byte {
                    BEL => self.state = ProcessActivityState::Ground,
                    ESC => self.state = ProcessActivityState::OscStringEscape,
                    _ => {}
                },
                ProcessActivityState::OscStringEscape => match byte {
                    b'\\' => self.state = ProcessActivityState::Ground,
                    b']' => self.state = ProcessActivityState::OscString,
                    ESC => self.state = ProcessActivityState::OscStringEscape,
                    _ => self.state = ProcessActivityState::Ground,
                },
            }
        }

        has_activity
    }

    fn reset(&mut self) {
        self.state = ProcessActivityState::Ground;
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ProcessActivityState {
    #[default]
    Ground,
    Escape,
    OscString,
    OscStringEscape,
}

fn manifest_source(region: &ManifestRegion) -> StateSource {
    match region {
        ManifestRegion::OscTitle => StateSource::OscTitle,
        ManifestRegion::OscProgress => StateSource::OscProgress,
        ManifestRegion::WholeRecent
        | ManifestRegion::BottomLines(_)
        | ManifestRegion::BottomNonEmptyLines(_)
        | ManifestRegion::TopNonEmptyLines(_)
        | ManifestRegion::LastNonEmptyAbovePromptBox
        | ManifestRegion::AfterLastPromptMarker
        | ManifestRegion::PromptBoxBody
        | ManifestRegion::AfterLastHorizontalRule => StateSource::Screen,
    }
}

fn is_visible_manifest_source(source: StateSource) -> bool {
    matches!(
        source,
        StateSource::OscTitle | StateSource::OscProgress | StateSource::Screen
    )
}

fn region_text(region: &ScreenRegion) -> String {
    region.lines.join("\n")
}

fn push_transition(
    transitions: &mut Vec<ActivityTransition>,
    transition: Option<ActivityTransition>,
) {
    if let Some(transition) = transition {
        transitions.push(transition);
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    use protocol::{AgentActivity, RuntimeRef, StateSource};

    use super::{
        ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest, ManifestRegion,
        MatchContext,
    };
    use std::sync::Arc;

    use protocol::{PackageId, PackageIdentity, PackageVersion, RuntimeId};

    use crate::agent::host::fixture::{official_host, official_manifest};
    use crate::agent::host::{
        DefinitionError, DefinitionOrigin, DefinitionParts, LaunchProgram, RuntimeDefinition,
        RuntimeHost, RuntimeRegistry, RuntimeSource, SourceTrust,
    };
    use crate::agent::InputRules;
    use crate::procwatch::{Pid, ProcessFact};

    /// Detector config of an official package runtime, selected through the
    /// registry of [`official_host`].
    fn package_config(kind: &RuntimeRef) -> DetectorConfig {
        let host = official_host();
        let definition = host.resolve_ref(kind).expect("the runtime is served");
        assert!(
            matches!(
                definition.binding().provenance,
                protocol::BindingProvenance::Package { .. }
            ),
            "{kind:?} is served by its official package"
        );
        DetectorConfig::for_agent(&host, kind)
    }

    fn instant() -> Instant {
        Instant::now()
    }

    /// Detector config used by most tests.
    ///
    /// Loads the same embedded generic shell manifest the production detector
    /// uses, so OSC title/progress and visible-screen keyword detection run
    /// through the manifest layer just like in the live daemon.
    fn config() -> DetectorConfig {
        DetectorConfig {
            input_readiness: crate::agent::host::InputReadiness::default(),
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 1,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(Arc::clone(super::shared_generic_shell_manifest())),
        }
    }

    fn debounce_config() -> DetectorConfig {
        DetectorConfig {
            input_readiness: crate::agent::host::InputReadiness::default(),
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 3,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(Arc::clone(super::shared_generic_shell_manifest())),
        }
    }

    fn transition(activity: AgentActivity, source: StateSource) -> ActivityTransition {
        ActivityTransition { activity, source }
    }

    fn manifest(source: &str) -> Manifest {
        Manifest::parse_str(source).expect("manifest should parse")
    }

    fn shared_manifest(source: &str) -> Arc<Manifest> {
        Arc::new(manifest(source))
    }

    fn fact(process_id: Pid, comm: &str, cmdline: &[&str]) -> ProcessFact {
        ProcessFact {
            pid: process_id,
            pgid: process_id,
            ppid: 1,
            start_identity: crate::procwatch::StartIdentity::new(u64::from(process_id)),
            comm: comm.to_owned(),
            cmdline: cmdline
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
        }
    }

    #[test]
    fn identify_agent_matches_the_process_sections_of_served_runtimes() {
        assert_eq!(
            super::identify_agent(&official_host(), &fact(100, "codex", &["/usr/bin/codex"])),
            Some(RuntimeRef::codex())
        );
        assert_eq!(
            super::identify_agent(
                &official_host(),
                &fact(101, "node", &["node", "/opt/claude-code/bin/claude.js"],)
            ),
            Some(RuntimeRef::claude())
        );
        assert_eq!(
            super::identify_agent(&official_host(), &fact(102, "sleep", &["sleep", "30"])),
            None
        );

        for (pid, comm, cmdline) in [
            (103, "hermes", vec!["/usr/local/bin/hermes", "chat"]),
            (104, "python3", vec!["/usr/local/bin/hermes", "chat"]),
            (
                105,
                "python3",
                vec!["/usr/bin/python3", "-m", "hermes_cli.main", "chat"],
            ),
            (
                106,
                "python3.13",
                vec![
                    "/usr/bin/python3.13",
                    "/opt/hermes/hermes_cli/main.py",
                    "chat",
                ],
            ),
        ] {
            assert_eq!(
                super::identify_agent(&official_host(), &fact(pid, comm, &cmdline)),
                Some(RuntimeRef::hermes())
            );
        }

        for (pid, comm, cmdline) in [
            (107, "hermes-helper", vec!["/usr/bin/hermes-helper", "chat"]),
            (108, "python3", vec!["python3", "/work/cli.py", "chat"]),
            (
                109,
                "python3",
                vec!["python3", "-m", "not_hermes_cli.main", "chat"],
            ),
            (110, "cat", vec!["cat", "/work/hermes"]),
            (
                111,
                "echo",
                vec!["echo", "/usr/bin/python3", "-m", "hermes_cli.main"],
            ),
            (
                112,
                "node",
                vec!["node", "/opt/hermes/hermes_cli/main.py", "chat"],
            ),
            (
                113,
                "python3",
                vec!["python3", "/opt/hermes/hermes_cli/helper.py", "chat"],
            ),
        ] {
            assert_eq!(
                super::identify_agent(&official_host(), &fact(pid, comm, &cmdline)),
                None,
                "unrelated process must not be identified as Hermes"
            );
        }
    }

    #[test]
    fn osc_title_working_split_across_chunks_emits_working_title_transition() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector.feed(started_at, b"\x1b]2;wor").is_empty());
        assert!(detector
            .feed(started_at + Duration::from_millis(10), b"ki")
            .is_empty());

        assert_eq!(
            detector.feed(started_at + Duration::from_millis(20), b"ng\x07"),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("working"));
    }

    #[test]
    fn osc_title_split_between_escape_and_bracket_does_not_emit_process() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector.feed(started_at, b"\x1b").is_empty());

        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"]2;working\x07"),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("working"));
    }

    #[test]
    fn mixed_output_and_partial_osc_emits_process_then_final_title_source() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"output\x1b]2;blo"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"cked\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("blocked"));
    }

    #[test]
    fn osc_st_split_between_escape_and_backslash_allows_later_process_fallback() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector.feed(started_at, b"\x1b]2;ready\x1b").is_empty());
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"\\"),
            vec![transition(AgentActivity::Idle, StateSource::OscTitle)]
        );

        assert_eq!(
            detector.feed(started_at + Duration::from_millis(20), b"\x1b[?25l"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn osc_7_cwd_hint_is_taken_once_without_activity_transition() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector
            .feed(started_at, b"\x1b]7;file:///work/pohunek-cwd\x07")
            .is_empty());
        assert_eq!(
            detector.take_cwd_hint(),
            Some("/work/pohunek-cwd".to_owned())
        );
        assert_eq!(detector.take_cwd_hint(), None);
    }

    #[test]
    fn resync_after_lag_clears_pending_cwd_hint() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector
            .feed(started_at, b"\x1b]7;file:///work/pohunek-cwd\x07")
            .is_empty());
        detector.resync_after_lag();

        assert_eq!(detector.take_cwd_hint(), None);
    }

    #[test]
    fn osc_progress_action_required_emits_blocked_progress_transition() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"\x1b]9;action required\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscProgress)]
        );
        assert_eq!(detector.latest_progress(), Some("action required"));
    }

    #[test]
    fn non_empty_control_bytes_without_visible_text_emit_working_process_transition() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"\x1b[?25l"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn manifest_osc_title_rule_emits_osc_title_source() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "title-blocked"
            state = "blocked"
            priority = 1
            region = "osc_title"
            contains = "needs-review"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"\x1b]2;needs-review\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscTitle)]
        );
    }

    #[test]
    fn manifest_bottom_lines_11_region_can_match() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "bottom-11"
            state = "working"
            priority = 1
            region = "bottom_lines(11)"
            contains = "line-11-target"
            "#,
        ));
        let mut detector = Detector::new(12, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                b"line-01\r\nline-02\r\nline-03\r\nline-04\r\nline-05\r\nline-06\r\nline-07\r\nline-08\r\nline-09\r\nline-10\r\nline-11-target"
            ),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn after_last_horizontal_rule_region_matches_text_below_the_rule() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "below-rule-blocked"
            state = "blocked"
            priority = 1
            region = "after_last_horizontal_rule"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[Hheader\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\napproval required".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn top_non_empty_lines_region_matches_joined_head_content() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "top-trust-blocked"
            state = "blocked"
            priority = 1
            region = "top_non_empty_lines(2)"
            contains = "trust this repository"
            "#,
        ));
        let mut detector = Detector::new(5, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H\r\nDo you trust this repository?\r\n\r\nlater output".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn top_non_empty_lines_region_matches_cjk_wide_glyph_content() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "top-trust-cjk-blocked"
            state = "blocked"
            priority = 1
            region = "top_non_empty_lines(1)"
            contains = "信任仓库"
            "#,
        ));
        let mut detector = Detector::new(4, 20, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H\r\n信任仓库界\r\n\r\nlater output".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn last_non_empty_above_prompt_box_region_matches_status_line() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "above-prompt-blocked"
            state = "blocked"
            priority = 1
            region = "last_non_empty_above_prompt_box"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(6, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[Holder status\r\napproval required\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{203a} type here\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn last_non_empty_above_prompt_box_region_matches_cjk_wide_glyph_content() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "above-prompt-cjk-blocked"
            state = "blocked"
            priority = 1
            region = "last_non_empty_above_prompt_box"
            contains = "审批需要"
            "#,
        ));
        let mut detector = Detector::new(5, 20, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H审批需要确认界\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{203a} type here\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn region_previews_render_new_regions_with_canonical_names() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "top"
            state = "blocked"
            priority = 2
            region = "top_non_empty_lines(2)"
            contains = "trust"

            [[rules]]
            id = "prompt-adjacent"
            state = "idle"
            priority = 1
            region = "last_non_empty_above_prompt_box"
            contains = "complete"
            "#,
        ));
        let mut detector = Detector::new(7, 40, started_at, detector_config);
        detector.feed(
            started_at,
            "trust this directory\r\ncompleted for 2s\r\n─────\r\n› type here\r\n─────".as_bytes(),
        );

        let previews = detector.region_previews();
        assert_eq!(previews.len(), 2);
        assert_eq!(previews[0].region, "top_non_empty_lines(2)");
        assert_eq!(previews[0].text, "trust this directory\ncompleted for 2s");
        assert_eq!(previews[1].region, "last_non_empty_above_prompt_box");
        assert_eq!(previews[1].text, "completed for 2s");
    }

    #[test]
    fn after_last_prompt_marker_region_matches_text_below_the_marker() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "below-marker-blocked"
            state = "blocked"
            priority = 1
            region = "after_last_prompt_marker"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H\u{203a} run tests\r\napproval required".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn missing_osc_title_region_is_absent_from_manifest_context() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "missing-title"
            state = "blocked"
            priority = 1
            region = "osc_title"
            not = { contains = "working" }
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert!(detector.tick(started_at).is_empty());
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"plain output"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn stale_manifest_context_does_not_suppress_process_fallback() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "visible-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"approval required"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"\x1b[?25l"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn tick_reconfirms_static_manifest_evidence_for_default_debounce() {
        let started_at = instant();
        let mut detector_config = debounce_config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "visible-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert!(detector.feed(started_at, b"approval required").is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(200)),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn tick_prioritizes_manifest_evidence_over_generic_osc_to_confirm_debounce() {
        let started_at = instant();
        let mut detector_config = debounce_config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "screen-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert!(detector
            .feed(started_at, b"\x1b]2;idle\x07approval required")
            .is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(200)),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn tick_reconfirms_static_osc_evidence_for_default_debounce() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, debounce_config());

        assert!(detector.feed(started_at, b"\x1b]2;idle\x07").is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(200)),
            vec![transition(AgentActivity::Idle, StateSource::OscTitle)]
        );
    }

    #[test]
    fn tick_does_not_reemit_cached_working_after_process_activity() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"\x1b]2;working\x07"),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        // Plain output with no working keyword: the screen manifest layer stays
        // dormant, so the only signal is process activity, which records the
        // published source as Process without re-emitting.
        assert!(detector
            .feed(started_at + Duration::from_millis(100), b"plain log output")
            .is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(900))
            .is_empty());
    }

    #[test]
    fn resync_after_lag_clears_cached_title_and_progress_values() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        let transitions = detector.feed(started_at, b"\x1b]2;working\x07\x1b]9;progress\x07");
        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("working"));
        assert_eq!(detector.latest_progress(), Some("progress"));

        detector.resync_after_lag();

        assert_eq!(detector.latest_title(), None);
        assert_eq!(detector.latest_progress(), None);
    }

    #[test]
    fn resync_after_lag_clears_visible_screen_so_stale_text_no_longer_matches() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "visible-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        // The blocker text matched against the visible screen before the lag.
        assert_eq!(
            detector.feed(started_at, b"approval required"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );

        detector.resync_after_lag();

        // After resync the screen is blank, so a tick finds no visible blocker to
        // reconfirm and emits nothing.
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
    }

    #[test]
    fn resync_after_lag_clears_pending_osc_debounce_candidate() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, debounce_config());

        assert!(detector.feed(started_at, b"\x1b]2;idle\x07").is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());

        detector.resync_after_lag();

        assert!(detector
            .feed(started_at + Duration::from_millis(200), b"\x1b]2;idle\x07")
            .is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(300))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(400)),
            vec![transition(AgentActivity::Idle, StateSource::OscTitle)]
        );
    }

    #[test]
    fn tick_delegates_stable_visible_refresh() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "visible-working"
            state = "working"
            priority = 1
            region = "whole_recent"
            contains = "compiling workspace"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"Compiling workspace"),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(800)),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn a_tick_over_unchanged_context_skips_the_manifest_evaluation() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "visible-idle"
            state = "idle"
            priority = 1
            region = "whole_recent"
            contains = "ready for input"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);
        detector.feed(started_at, b"ready for input");
        assert!(detector.context_dirty, "output marks the context dirty");

        detector.tick(started_at + Duration::from_millis(100));
        detector.tick(started_at + Duration::from_millis(200));
        detector.tick(started_at + Duration::from_millis(300));
        assert!(!detector.state.has_pending());
        assert!(
            !detector.context_dirty,
            "the evaluating tick consumed the change"
        );

        assert!(
            detector
                .tick(started_at + Duration::from_millis(400))
                .is_empty(),
            "a stable idle state within the refresh interval emits nothing"
        );
        detector.resize(3, 100);
        assert!(detector.context_dirty, "a resize marks the context dirty");
    }

    #[test]
    fn repeated_startup_completion_does_not_invalidate_the_evaluated_context() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());
        detector.finish_initial_input();
        detector.tick(started_at + Duration::from_millis(100));
        assert!(!detector.context_dirty);

        // The detector loop calls this before every iteration once startup ended.
        detector.finish_initial_input();
        assert!(
            !detector.context_dirty,
            "a completed startup does not force the next tick to re-evaluate"
        );
    }

    #[test]
    fn resize_changes_screen_dimensions_used_by_manifest_context() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "two-line-context"
            state = "blocked"
            priority = 1
            region = "bottom_non_empty_lines(2)"
            contains = ["ready marker", "approval required"]
            "#,
        ));
        let mut detector = Detector::new(1, 80, started_at, detector_config);

        assert!(!detector
            .feed(started_at, b"approval required")
            .contains(&transition(AgentActivity::Blocked, StateSource::Screen)));

        detector.resize(2, 80);

        assert_eq!(
            detector.feed(
                started_at + Duration::from_millis(10),
                b"\x1b[2J\x1b[Hready marker\r\napproval required"
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn hermes_manifest_maps_bounded_classic_and_tui_evidence() {
        let manifest = official_manifest("hermes");
        let cases = [
            (
                ManifestRegion::BottomNonEmptyLines(16),
                "⚠ Dangerous Command\n1. Allow once\n4. Deny",
                AgentActivity::Blocked,
            ),
            (
                ManifestRegion::BottomNonEmptyLines(16),
                "⚠ approval required · run command\n1. Allow once\n4. Deny",
                AgentActivity::Blocked,
            ),
            (
                ManifestRegion::BottomNonEmptyLines(8),
                "output\n⚕ ❯",
                AgentActivity::Working,
            ),
            (
                ManifestRegion::BottomNonEmptyLines(8),
                "Ctrl+C to interrupt…",
                AgentActivity::Working,
            ),
            (
                ManifestRegion::BottomNonEmptyLines(4),
                "❯",
                AgentActivity::Idle,
            ),
            (
                ManifestRegion::BottomNonEmptyLines(4),
                "─ ready │ model",
                AgentActivity::Idle,
            ),
            (
                ManifestRegion::WholeRecent,
                "Failed to initialize agent: provider unavailable",
                AgentActivity::Blocked,
            ),
        ];

        for (region, text, activity) in cases {
            let matched = manifest
                .match_context(&MatchContext::default().with_region_text(region, text))
                .expect("Hermes fixture should match");
            assert_eq!(matched.activity, activity);
        }
    }

    #[test]
    fn hermes_manifest_rejects_unrelated_screen_text() {
        let matched =
            official_manifest("hermes").match_context(&MatchContext::default().with_region_text(
                ManifestRegion::WholeRecent,
                "documentation: type Allow once or Deny when asked",
            ));
        assert!(matched.is_none());
    }

    /// Source of one custom runtime whose manifest is `manifest_source`.
    #[derive(Debug)]
    struct CustomSource {
        manifest_source: &'static str,
    }

    impl RuntimeSource for CustomSource {
        fn trust(&self) -> SourceTrust {
            SourceTrust::Builtin
        }

        fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
            Ok(vec![RuntimeDefinition::new(DefinitionParts {
                runtime_id: RuntimeId::parse("acme").expect("runtime id"),
                origin: DefinitionOrigin::Builtin {
                    package: Some(PackageIdentity {
                        id: PackageId::parse("acme.runtime").expect("package id"),
                        version: PackageVersion::parse("1.0.0").expect("package version"),
                    }),
                },
                display_name: "Acme".to_owned(),
                program: LaunchProgram::Fixed("acme".to_owned()),
                default_args: Vec::new(),
                input_rules: InputRules::unrestricted(false, Duration::ZERO),
                input_readiness: crate::agent::host::InputReadiness::BracketedPaste,
                submit_delay_configurable: false,
                manifest: Arc::new(manifest(self.manifest_source)),
                native: None,
                prompt_arg: false,
                version_probe_parser: None,
                version_probe_policy: None,
                integration: None,
            })
            .expect("valid definition")])
        }
    }

    const ACME_MANIFEST: &str = r#"
        [process]
        comm = ["^acme-agent$"]
        cmdline = ['(^|/)acme-agent($| )']

        [[rules]]
        id = "acme-blocked"
        state = "blocked"
        priority = 1
        region = "whole_recent"
        contains = "acme needs you"
        "#;

    fn acme_host() -> RuntimeHost {
        let source = CustomSource {
            manifest_source: ACME_MANIFEST,
        };
        RuntimeHost::new(RuntimeRegistry::from_sources(&[&source]).expect("registry"))
    }

    #[test]
    fn detector_config_follows_the_definitions_manifest() {
        let host = acme_host();
        let mut config = DetectorConfig::for_agent(&host, &RuntimeRef::from_wire("acme"));
        config.detection = self::config().detection;
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config);

        assert_eq!(
            detector.feed(started_at, b"acme needs you\r\n"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn detector_config_for_a_kind_without_a_definition_is_the_generic_shell_manifest() {
        let host = acme_host();

        let config = DetectorConfig::for_agent(&host, &RuntimeRef::from_wire("missing"));

        assert_eq!(
            format!("{:?}", config.manifest),
            format!("{:?}", Some(super::generic_shell_manifest().clone()))
        );
    }

    #[test]
    fn identify_agent_uses_the_process_matchers_of_installed_definitions() {
        let host = acme_host();

        assert_eq!(
            super::identify_agent(&host, &fact(200, "acme-agent", &["/usr/bin/acme-agent"])),
            Some(RuntimeRef::from_wire("acme"))
        );
        assert_eq!(
            super::identify_agent(&host, &fact(201, "codex", &["/usr/bin/codex"])),
            None,
            "a runtime absent from the registry is never identified"
        );
    }

    #[test]
    fn detector_config_for_profile_uses_override_manifest_when_present() {
        let override_manifest = manifest(
            r#"
            [[rules]]
            id = "custom-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "custom blocker"
            "#,
        );

        let mut config = super::DetectorConfig::for_profile(
            &official_host(),
            &protocol::RuntimeRef::codex(),
            Some(override_manifest),
        );
        config.detection = self::config().detection;
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config);

        assert_eq!(
            detector.feed(started_at, b"custom blocker\r\n"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn detector_config_for_profile_inherits_base_manifest_without_override() {
        // With no override, a profile inherits its base kind's manifest. The
        // detector config holds an owned manifest (not the &'static), so assert by
        // behavior: feeding a Claude blocking pattern yields the same blocked
        // transition the base Claude config produces.
        let mut profile_config = super::DetectorConfig::for_profile(
            &official_host(),
            &protocol::RuntimeRef::claude(),
            None,
        );
        profile_config.detection = self::config().detection;
        let mut base_config = package_config(&RuntimeRef::claude());
        base_config.detection = self::config().detection;
        assert!(
            profile_config.manifest.is_some(),
            "a profile without an override must inherit a base manifest"
        );

        let blocked = "enter to select\nesc to cancel\n↑/↓ to navigate";
        let started_at = instant();
        let mut from_profile = Detector::new(6, 80, started_at, profile_config);
        let mut from_base = Detector::new(6, 80, started_at, base_config);
        assert_eq!(
            from_profile.feed(started_at, blocked.as_bytes()),
            from_base.feed(started_at, blocked.as_bytes()),
            "inherited manifest must detect exactly like the base Claude config"
        );
    }

    #[test]
    fn detector_reconfigure_switches_manifest_without_losing_context() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());
        assert_eq!(
            detector.feed(
                started_at,
                b"\x1b]2;cached nested title\x07\r\nnested-screen-token"
            ),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );

        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(
            r#"
            [[rules]]
            id = "nested-screen"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "nested-screen-token"
            "#,
        ));
        detector.reconfigure(started_at + Duration::from_millis(10), detector_config);

        assert_eq!(detector.latest_title(), Some("cached nested title"));
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(20)),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn detector_reconfigure_rechecks_cached_title_with_new_manifest() {
        let started_at = instant();
        let mut detector = Detector::new(8, 80, started_at, DetectorConfig::generic_shell());
        assert!(detector
            .feed(started_at, "\x1b]2;\u{2800} thinking\x07".as_bytes())
            .is_empty());

        detector.reconfigure(
            started_at + Duration::from_millis(10),
            package_config(&RuntimeRef::codex()),
        );

        assert_eq!(
            detector.tick(started_at + Duration::from_secs(4)),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
    }

    #[test]
    fn claude_manifest_maps_ink_selection_form_to_blocked() {
        let started_at = instant();
        let mut detector_config = package_config(&RuntimeRef::claude());
        detector_config.detection = config().detection;
        let mut detector = Detector::new(8, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[HReview\r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\nenter to select\r\nesc to cancel\r\n\u{2191}/\u{2193} to navigate".as_bytes(),
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn claude_manifest_maps_braille_title_spinner_to_working() {
        let started_at = instant();
        let mut detector_config = package_config(&RuntimeRef::claude());
        detector_config.detection = config().detection;
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, "\x1b]2;\u{280b} thinking\x07".as_bytes()),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
    }

    #[test]
    fn claude_manifest_maps_live_status_spinner_to_working() {
        let started_at = instant();
        let mut detector_config = package_config(&RuntimeRef::claude());
        detector_config.detection = config().detection;
        let mut detector = Detector::new(8, 120, started_at, detector_config);

        // Captured from a live Claude Code turn: the status spinner renders above
        // the prompt box while the prompt box (`❯`) stays visible, so the screen
        // must resolve to working, not prompt-box idle.
        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H\u{273b} Noodling\u{2026} (12s \u{b7} \u{2193} 1.2k tokens)\r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{276f} \r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n  auto mode on".as_bytes(),
            ),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn claude_manifest_finished_status_line_resolves_prompt_box_idle() {
        let started_at = instant();
        let mut detector_config = package_config(&RuntimeRef::claude());
        detector_config.detection = config().detection;
        let mut detector = Detector::new(8, 120, started_at, detector_config);

        // A completed turn renders "✻ Cogitated for 26s" (no ellipsis, no hint
        // parenthesis) above the prompt box; that must not match the spinner rule.
        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H\u{273b} Cogitated for 26s\r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{276f} \r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n  auto mode on".as_bytes(),
            ),
            vec![transition(AgentActivity::Idle, StateSource::Screen)]
        );
    }

    #[test]
    fn claude_manifest_maps_prompt_adjacent_completion_status_to_idle() {
        let claude = official_host()
            .resolve_ref(&RuntimeRef::claude())
            .expect("the Claude package is served");
        let matched = claude
            .manifest()
            .match_context(&MatchContext::default().with_region_text(
                ManifestRegion::LastNonEmptyAbovePromptBox,
                "✻ Cogitated for 26s",
            ))
            .expect("Claude completion fixture should match");

        assert_eq!(matched.activity, AgentActivity::Idle);
        assert_eq!(matched.region, ManifestRegion::LastNonEmptyAbovePromptBox);
    }

    #[test]
    fn claude_manifest_permission_dialog_outranks_stale_spinner_line() {
        let started_at = instant();
        let mut detector_config = package_config(&RuntimeRef::claude());
        detector_config.detection = config().detection;
        let mut detector = Detector::new(10, 120, started_at, detector_config);

        // A visible approval dialog must win even when a stale spinner line is
        // still on screen: the spinner rule excludes dialog markers, so the
        // bash permission prompt resolves blocked.
        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H\u{2733} Running\u{2026} (5s \u{b7} hooks)\r\nBash command\r\nsleep 45\r\nDo you want to proceed?\r\n\u{276f} 1. Yes\r\n  2. No\r\nesc to cancel".as_bytes(),
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    /// Steady-state detector of an official package runtime with the test
    /// debounce: its initial input has resolved and nothing is published.
    fn steady_detector(kind: &RuntimeRef, rows: u16, cols: u16, started_at: Instant) -> Detector {
        let mut detector_config = package_config(kind);
        detector_config.detection = config().detection;
        let mut detector = Detector::new(rows, cols, started_at, detector_config);
        detector.finish_initial_input();
        detector
    }

    fn claude_detector(rows: u16, cols: u16, started_at: Instant) -> Detector {
        steady_detector(&RuntimeRef::claude(), rows, cols, started_at)
    }

    /// A Claude screen with a live status spinner above the prompt box.
    const CLAUDE_WORKING_FRAME: &str = "\x1b[2J\x1b[H\u{273b} Noodling\u{2026} (12s \u{b7} \u{2193} 1.2k tokens)\r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{276f} \r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n  auto mode on";

    /// A Claude screen of a finished turn at the prompt box.
    const CLAUDE_IDLE_FRAME: &str = "\x1b[2J\x1b[H\u{273b} Cogitated for 26s\r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{276f} \r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n  auto mode on";

    fn working_osc_title() -> &'static str {
        "\x1b]2;\u{280b} thinking\x07"
    }

    fn idle_osc_title() -> &'static str {
        "\x1b]2;\u{2733} Claude Code\x07"
    }

    #[test]
    fn a_working_title_and_an_idle_title_in_one_chunk_still_yield_working_evidence() {
        let started_at = instant();
        let mut detector = claude_detector(3, 80, started_at);

        let chunk = format!("{}{}", working_osc_title(), idle_osc_title());
        let transitions = detector.feed(started_at, chunk.as_bytes());

        assert_eq!(
            transitions.first(),
            Some(&transition(AgentActivity::Working, StateSource::OscTitle)),
            "the working title sighted inside the chunk publishes working first: {transitions:?}"
        );
        assert_eq!(detector.latest_title(), Some("\u{2733} Claude Code"));
    }

    #[test]
    fn a_latched_working_title_is_followed_by_the_chunk_end_idle_after_debounce() {
        let started_at = instant();
        let mut detector = claude_detector(3, 80, started_at);

        let chunk = format!("{}{}", working_osc_title(), idle_osc_title());
        detector.feed(started_at, chunk.as_bytes());
        let mut transitions = Vec::new();
        for step in 1..=10 {
            transitions.extend(detector.tick(started_at + Duration::from_millis(100 * step)));
        }

        assert!(
            transitions
                .iter()
                .any(|transition| transition.activity == AgentActivity::Idle),
            "the chunk-end idle confirms through the ordinary debounce: {transitions:?}"
        );
    }

    #[test]
    fn the_working_latch_never_duplicates_a_published_working_state() {
        let started_at = instant();
        let mut detector = claude_detector(3, 80, started_at);
        assert_eq!(
            detector.feed(started_at, working_osc_title().as_bytes()),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );

        let chunk = format!("{}{}", working_osc_title(), idle_osc_title());
        let transitions = detector.feed(started_at + Duration::from_millis(10), chunk.as_bytes());

        assert!(
            transitions
                .iter()
                .all(|transition| transition.activity != AgentActivity::Working),
            "no second working edge while working is published: {transitions:?}"
        );
    }

    #[test]
    fn a_chunk_that_ends_on_a_working_title_publishes_one_working_edge() {
        let started_at = instant();
        let mut detector = claude_detector(3, 80, started_at);

        let chunk = format!(
            "{}{}{}",
            working_osc_title(),
            idle_osc_title(),
            working_osc_title()
        );
        let transitions = detector.feed(started_at, chunk.as_bytes());

        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
    }

    #[test]
    fn a_working_title_followed_by_a_permission_dialog_reports_blocked_alone() {
        let started_at = instant();
        let mut detector = claude_detector(10, 120, started_at);

        let chunk = format!(
            "{}\x1b[2J\x1b[HBash command\r\nsleep 45\r\nDo you want to proceed?\r\n\u{276f} 1. Yes\r\n  2. No\r\nesc to cancel{}",
            working_osc_title(),
            idle_osc_title()
        );
        let transitions = detector.feed(started_at, chunk.as_bytes());

        assert!(
            transitions
                .iter()
                .all(|transition| transition.activity != AgentActivity::Working),
            "a chunk ending on a visible dialog never latches working: {transitions:?}"
        );
        assert!(transitions
            .iter()
            .any(|transition| transition.activity == AgentActivity::Blocked));
    }

    #[test]
    fn a_lag_resync_fabricates_no_working_evidence() {
        let started_at = instant();
        let mut detector = claude_detector(3, 80, started_at);
        // A working title severed by the dropped chunks: its head arrived, its
        // tail and terminator did not.
        detector.feed(started_at, "\x1b]2;\u{280b} thi".as_bytes());
        detector.resync_after_lag();
        let sightings = detector.latch_sightings;

        let transitions = detector.feed(
            started_at + Duration::from_millis(10),
            format!("nking\x07{}", idle_osc_title()).as_bytes(),
        );

        assert_eq!(detector.latch_sightings, sightings, "{transitions:?}");
        assert!(transitions
            .iter()
            .all(|transition| transition.activity != AgentActivity::Working));
    }

    #[test]
    fn the_generic_shell_manifest_never_latches_a_transient_working_title() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        let transitions = detector.feed(started_at, b"\x1b]2;running\x07\x1b]2;bash\x07");

        assert!(
            transitions
                .iter()
                .all(|transition| transition.activity != AgentActivity::Working),
            "shell output stays on the byte path: {transitions:?}"
        );
        assert_eq!(
            detector.latch_evaluations, 0,
            "the shell latch gate is shut"
        );
    }

    #[test]
    fn a_working_frame_and_an_idle_frame_in_one_chunk_still_yield_working_evidence() {
        let started_at = instant();
        let mut detector = claude_detector(8, 120, started_at);

        let chunk = format!("{CLAUDE_WORKING_FRAME}{CLAUDE_IDLE_FRAME}");
        let transitions = detector.feed(started_at, chunk.as_bytes());

        assert_eq!(
            transitions.first(),
            Some(&transition(AgentActivity::Working, StateSource::Screen)),
            "the spinner frame sighted inside the chunk publishes working: {transitions:?}"
        );
    }

    #[test]
    fn a_working_frame_then_a_permission_dialog_reports_blocked_alone() {
        let started_at = instant();
        let mut detector = claude_detector(10, 120, started_at);

        let dialog = "\x1b[2J\x1b[HBash command\r\nsleep 45\r\nDo you want to proceed?\r\n\u{276f} 1. Yes\r\n  2. No\r\nesc to cancel";
        let chunk = format!("{CLAUDE_WORKING_FRAME}{dialog}");
        let transitions = detector.feed(started_at, chunk.as_bytes());

        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    /// Claude detector whose manifest never latches, for comparison.
    fn claude_detector_without_latch(started_at: Instant) -> Detector {
        let mut detector_config = package_config(&RuntimeRef::claude());
        detector_config.detection = config().detection;
        detector_config.manifest = Some(Arc::new(
            manifest(include_str!(
                "../../../../runtime-packages/claude/detect.toml"
            ))
            .without_working_latch(),
        ));
        let mut detector = Detector::new(10, 120, started_at, detector_config);
        detector.finish_initial_input();
        detector
    }

    #[test]
    fn a_forged_working_row_above_the_idle_editor_adds_no_latch_evidence_in_claude() {
        let started_at = instant();
        let forged = CLAUDE_IDLE_FRAME.replace(
            "\u{273b} Cogitated for 26s",
            "* Verbing\u{2026} (3s \u{b7} thinking)",
        );
        let mut latching = claude_detector(10, 120, started_at);
        let mut plain = claude_detector_without_latch(started_at);
        for detector in [&mut latching, &mut plain] {
            detector.feed(started_at, CLAUDE_IDLE_FRAME.as_bytes());
        }

        // The spinner rule reads the whole screen, so the forged row already
        // reads working at the chunk end; the latch adds nothing on top.
        assert_eq!(
            latching.feed(started_at + Duration::from_millis(10), forged.as_bytes()),
            plain.feed(started_at + Duration::from_millis(10), forged.as_bytes())
        );
    }

    #[test]
    fn a_forged_working_row_above_the_idle_editor_never_latches_in_pi() {
        let started_at = instant();
        let mut detector = pi_detector(started_at);
        detector.feed(started_at, pi_idle_frame().as_bytes());
        let sightings = detector.latch_sightings;

        let forged = pi_idle_frame().replace(
            "working done",
            &format!("\u{2500}\u{2500} \u{2807} Working {}", rule(40)),
        );
        let transitions = detector.feed(started_at + Duration::from_millis(10), forged.as_bytes());

        assert_eq!(detector.latch_sightings, sightings, "{transitions:?}");
    }

    #[test]
    fn a_working_frame_erased_by_an_interrupt_still_latches_then_goes_idle() {
        let started_at = instant();
        let mut detector = claude_detector(8, 120, started_at);

        let chunk = format!(
            "{CLAUDE_WORKING_FRAME}\x1b[2J\x1b[H  \u{23bf} Interrupted\r\n{CLAUDE_IDLE_FRAME}"
        );
        let transitions = detector.feed(started_at, chunk.as_bytes());

        assert_eq!(
            transitions,
            vec![
                transition(AgentActivity::Working, StateSource::Screen),
                transition(AgentActivity::Idle, StateSource::Screen),
            ]
        );
    }

    #[test]
    fn a_frame_chunk_at_published_working_runs_no_latch_evaluation() {
        let started_at = instant();
        let mut detector = claude_detector(8, 120, started_at);
        detector.feed(started_at, CLAUDE_WORKING_FRAME.as_bytes());
        let evaluations = detector.latch_evaluations;

        let chunk = format!("{CLAUDE_IDLE_FRAME}{CLAUDE_WORKING_FRAME}{CLAUDE_IDLE_FRAME}");
        detector.feed(started_at + Duration::from_millis(10), chunk.as_bytes());

        assert_eq!(
            detector.latch_evaluations, evaluations,
            "a published working state closes the latch gate"
        );
    }

    #[test]
    fn a_full_idle_repaint_never_latches_working() {
        for columns in [30, 120, 200] {
            let started_at = instant();
            let mut detector = claude_detector(8, columns, started_at);
            detector.feed(started_at, CLAUDE_IDLE_FRAME.as_bytes());

            let mut repaint = String::new();
            for line in [
                "\u{273b} Cogitated for 26s",
                "\u{2500}\u{2500}\u{2500}\u{2500}",
                "\u{276f} ",
                "\u{2500}\u{2500}\u{2500}\u{2500}",
                "  auto mode on",
            ] {
                let _ = write!(repaint, "\x1b[2K{line}\r\n");
            }
            let transitions =
                detector.feed(started_at + Duration::from_millis(10), repaint.as_bytes());

            assert!(
                transitions
                    .iter()
                    .all(|transition| transition.activity != AgentActivity::Working),
                "{columns} columns: {transitions:?}"
            );
            assert!(
                detector.latch_evaluations > 0,
                "{columns} columns: gate open"
            );
            assert_eq!(detector.latch_sightings, 0, "{columns} columns");
        }
    }

    #[test]
    fn a_blocker_outranking_a_spinner_at_an_intermediate_point_never_latches() {
        let started_at = instant();
        let mut detector = claude_detector(8, 120, started_at);
        detector.feed(started_at, CLAUDE_IDLE_FRAME.as_bytes());
        let sightings = detector.latch_sightings;
        let evaluations = detector.latch_evaluations;

        // The folder trust dialog (blocked, priority 980) is drawn before the
        // spinner line (working, 960), so no point shows the spinner without
        // the dialog; the chunk then ends on a different idle frame.
        let chunk = format!(
            "\x1b[2J\x1b[3;1HYes, I trust this folder\r\nNo, exit\r\nEnter to confirm\x1b[2;1H\u{2500}\u{2500}\u{2500}\u{2500}\x1b[1;1H\u{273b} Noodling\u{2026} (3s \u{b7} thinking){}",
            CLAUDE_IDLE_FRAME.replace("26s", "40s")
        );
        let transitions = detector.feed(started_at + Duration::from_millis(10), chunk.as_bytes());

        assert!(
            detector.latch_evaluations > evaluations,
            "the gate was open"
        );
        assert_eq!(detector.latch_sightings, sightings, "{transitions:?}");
        assert!(transitions
            .iter()
            .all(|transition| transition.activity != AgentActivity::Working));
    }

    #[test]
    fn a_stale_spinner_under_an_erased_dialog_latches_inside_a_running_turn() {
        let started_at = instant();
        let mut detector = claude_detector(10, 120, started_at);
        // A dialog drawn under a spinner line that is still on screen: the
        // spinner rule excludes dialog markers, so the screen reads blocked.
        let dialog = "\x1b[2J\x1b[H\u{2733} Running\u{2026} (5s \u{b7} hooks)\r\nBash command\r\nsleep 45\r\nDo you want to proceed?\r\n\u{276f} 1. Yes\r\n  2. No\r\nesc to cancel";
        detector.feed(started_at, dialog.as_bytes());
        assert_eq!(
            detector.state.published_activity(),
            Some(AgentActivity::Blocked)
        );

        // The dialog is answered: it is erased and the idle prompt redrawn
        // before the spinner line is rewritten. The point between the two
        // still shows the stale spinner without a dialog.
        let chunk = "\x1b[2;1H\x1b[J\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{276f} \r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\n  auto mode on\x1b[1;1H\x1b[2K\u{273b} Cogitated for 9s";
        let transitions = detector.feed(started_at + Duration::from_millis(10), chunk.as_bytes());

        assert_eq!(
            transitions.first(),
            Some(&transition(AgentActivity::Working, StateSource::Screen)),
            "{transitions:?}"
        );
    }

    #[test]
    fn a_cached_working_title_is_not_fresh_at_a_screen_repaint() {
        let started_at = instant();
        let mut detector = claude_detector(8, 120, started_at);
        detector.feed(started_at, working_osc_title().as_bytes());
        // A reconfigure restarts the published state and keeps the cached title.
        let mut claude = package_config(&RuntimeRef::claude());
        claude.detection = config().detection;
        detector.reconfigure(started_at, claude);
        assert_eq!(detector.latest_title(), Some("\u{280b} thinking"));
        let sightings = detector.latch_sightings;
        let evaluations = detector.latch_evaluations;

        let transitions = detector.feed(
            started_at + Duration::from_millis(10),
            CLAUDE_IDLE_FRAME.as_bytes(),
        );

        assert!(
            detector.latch_evaluations > evaluations,
            "the gate was open"
        );
        assert_eq!(detector.latch_sightings, sightings, "{transitions:?}");
    }

    #[test]
    fn a_shell_detector_switching_in_an_agent_manifest_resumes_the_latch() {
        let started_at = instant();
        let mut detector = Detector::new(8, 120, started_at, config());
        detector.finish_initial_input();

        let mut claude = package_config(&RuntimeRef::claude());
        claude.detection = config().detection;
        detector.reconfigure(started_at, claude);
        let chunk = format!("{CLAUDE_WORKING_FRAME}{CLAUDE_IDLE_FRAME}");
        let transitions = detector.feed(started_at, chunk.as_bytes());

        assert_eq!(
            transitions.first(),
            Some(&transition(AgentActivity::Working, StateSource::Screen)),
            "{transitions:?}"
        );
    }

    #[test]
    fn a_shell_session_never_latches_a_line_printed_and_erased_in_one_chunk() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());
        detector.finish_initial_input();

        let transitions = detector.feed(
            started_at,
            b"\x1b[2J\x1b[Hrunning tests\x1b[2K\r\x1b[2Kdone",
        );

        assert!(
            transitions
                .iter()
                .all(|transition| transition.source == StateSource::Process),
            "only the byte path may report: {transitions:?}"
        );
        assert_eq!(detector.latch_evaluations, 0);
    }

    fn rule(width: usize) -> String {
        "\u{2500}".repeat(width)
    }

    /// Steady-state detector over the Pi package manifest.
    fn pi_detector(started_at: Instant) -> Detector {
        let mut detector_config = config();
        detector_config.manifest = Some(shared_manifest(include_str!(
            "../../../../runtime-packages/pi/detect.toml"
        )));
        let mut detector = Detector::new(24, 120, started_at, detector_config);
        detector.finish_initial_input();
        detector
    }

    /// A Pi frame whose working line carries the braille spinner.
    fn pi_working_frame() -> String {
        format!(
            "\x1b[2J\x1b[Hsay hi\r\n\r\n{} \u{2807} Working {}\r\n{}\r\n~/work\r\n1.1%/128k (auto)      stub-model",
            "\u{2500}\u{2500}",
            rule(60),
            rule(100)
        )
    }

    /// A Pi frame of the idle editor between two rules.
    fn pi_idle_frame() -> String {
        format!(
            "\x1b[2J\x1b[Hsay hi\r\n\r\nworking done\r\n\r\n{}\r\n\r\n{}\r\n~/work\r\n0.0%/128k (auto)      stub-model",
            rule(100),
            rule(100)
        )
    }

    #[test]
    fn a_codex_working_title_and_an_idle_title_in_one_chunk_still_yield_working_evidence() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::codex(), 8, 120, started_at);

        let transitions = detector.feed(
            started_at,
            "\x1b]2;\u{280b} work\x07\x1b]2;work\x07".as_bytes(),
        );

        assert_eq!(
            transitions.first(),
            Some(&transition(AgentActivity::Working, StateSource::OscTitle)),
            "{transitions:?}"
        );
    }

    #[test]
    fn a_codex_title_generation_spinner_on_an_idle_thread_never_latches_working() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::codex(), 8, 120, started_at);
        let sightings = detector.latch_sightings;

        // An idle unnamed thread spins its title-generation placeholder: the
        // braille frame is followed by the separator, not by a thread name.
        let transitions = detector.feed(
            started_at,
            "\x1b]2;\u{2834} | cxw\x07\x1b]2;cxw\x07".as_bytes(),
        );

        assert_eq!(detector.latch_sightings, sightings, "{transitions:?}");
        assert!(transitions
            .iter()
            .all(|transition| transition.activity != AgentActivity::Working));
    }

    #[test]
    fn a_codex_working_title_with_a_thread_name_still_latches_working() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::codex(), 8, 120, started_at);

        for title in [
            "\u{280b} cxw",
            "\u{2834} \u{2834} | cxw",
            "\u{280b} my-thread | cxw",
        ] {
            let transitions = detector.feed(
                started_at,
                format!("\x1b]2;{title}\x07\x1b]2;cxw\x07").as_bytes(),
            );
            assert_eq!(
                transitions.first(),
                Some(&transition(AgentActivity::Working, StateSource::OscTitle)),
                "{title}: {transitions:?}"
            );
            detector.reconfigure(started_at, {
                let mut config = package_config(&RuntimeRef::codex());
                config.detection = self::config().detection;
                config
            });
        }
    }

    #[test]
    fn a_pi_working_frame_and_an_idle_frame_in_one_chunk_still_yield_working_evidence() {
        let started_at = instant();
        let mut detector = pi_detector(started_at);
        detector.feed(started_at, pi_idle_frame().as_bytes());
        let published = detector.state.published_activity();
        assert_ne!(published, Some(AgentActivity::Working));

        let chunk = format!(
            "{}{}",
            pi_working_frame(),
            pi_idle_frame().replace("working done", "second turn done")
        );
        let transitions = detector.feed(started_at + Duration::from_millis(10), chunk.as_bytes());

        assert_eq!(
            transitions.first(),
            Some(&transition(AgentActivity::Working, StateSource::Screen)),
            "{transitions:?}"
        );
    }

    #[test]
    fn a_pi_idle_redraw_never_latches_working() {
        let started_at = instant();
        let mut detector = pi_detector(started_at);
        detector.feed(started_at, pi_idle_frame().as_bytes());
        let sightings = detector.latch_sightings;
        let evaluations = detector.latch_evaluations;

        // Bottom-up repaint: the lower editor rule and footer first, then the
        // text above, with partial erasure in between.
        let redraw = format!(
            "\x1b[22;1H\x1b[J{}\r\n~/work\r\n0.0%/128k (auto)      stub-model\x1b[18;1H\x1b[J{}\r\n\r\n{}",
            rule(100),
            "working done",
            rule(100)
        );
        let transitions = detector.feed(started_at + Duration::from_millis(10), redraw.as_bytes());

        assert!(
            detector.latch_evaluations > evaluations,
            "the gate was open"
        );
        assert_eq!(detector.latch_sightings, sightings, "{transitions:?}");
    }

    #[test]
    fn a_hermes_working_prompt_erased_to_an_idle_prompt_in_one_chunk_still_yields_working_evidence()
    {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::hermes(), 12, 80, started_at);
        detector.feed(started_at, b"\x1b[2J\x1b[Hanswer\r\n\xe2\x9d\xaf ");

        // The working prompt line is completed by an erase of its trailing
        // text, then replaced by the idle prompt.
        let chunk = "\x1b[2J\x1b[Hanswer\r\n\u{2695} \u{276f} trailing\r\x1b[4C\x1b[K\x1b[2J\x1b[Hanswer two\r\n\u{276f} ";
        let transitions = detector.feed(started_at + Duration::from_millis(10), chunk.as_bytes());

        assert_eq!(
            transitions.first(),
            Some(&transition(AgentActivity::Working, StateSource::Screen)),
            "{transitions:?}"
        );
    }

    #[test]
    fn a_hermes_bare_working_prompt_above_its_rule_chrome_reads_working() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::hermes(), 12, 100, started_at);
        let rule = "\u{2500}".repeat(60);

        let transitions = detector.feed(
            started_at,
            format!("\x1b[2J\x1b[Hanswer\r\n\u{2695} \u{276f}\r\n{rule}\r\n\u{2500}").as_bytes(),
        );

        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    /// The captured classic working frame of Hermes 0.20.0
    /// (`compat/hermes/goldens/working.txt`): the staff prompt carries the
    /// placeholder Hermes paints into an empty composer while a turn runs, and
    /// the rule chrome wraps one cell onto a second row.
    fn hermes_working_frame() -> String {
        let rule = "\u{2500}".repeat(99);
        format!(
            "\x1b[2J\x1b[H answer\r\n\r\n \u{2695} pohunek-compat-v1 \u{2502} 0/64K \u{2502} [\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}] 0%\r\n\r\n{rule}\r\n\u{2500}\r\n\r\n\u{2695} \u{276f} msg=interrupt \u{b7} /queue \u{b7} /bg \u{b7} /steer \u{b7} Ctrl+C cancel\r\n\r\n{rule}\r\n\u{2500}"
        )
    }

    #[test]
    fn a_hermes_working_prompt_with_its_interrupt_placeholder_reads_working() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::hermes(), 14, 100, started_at);

        let transitions = detector.feed(started_at, hermes_working_frame().as_bytes());

        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn a_hermes_compact_working_prompt_without_the_arrow_reads_working() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::hermes(), 10, 60, started_at);

        let transitions = detector.feed(
            started_at,
            "\x1b[2J\x1b[Hanswer\r\n\u{2695} msg=interrupt \u{b7} /queue \u{b7} /bg \u{b7} /steer \u{b7} Ctrl+C cancel"
                .as_bytes(),
        );

        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn a_wrapped_hermes_interrupt_placeholder_reads_working_at_every_narrow_width() {
        for cols in [28_u16, 40, 54, 55, 60] {
            let started_at = instant();
            let mut detector = steady_detector(&RuntimeRef::hermes(), 10, cols, started_at);

            let transitions = detector.feed(
                started_at,
                "\x1b[2J\x1b[Hanswer\r\n\u{2695} msg=interrupt \u{b7} /queue \u{b7} /bg \u{b7} /steer \u{b7} Ctrl+C cancel"
                    .as_bytes(),
            );

            assert_eq!(
                transitions,
                vec![transition(AgentActivity::Working, StateSource::Screen)],
                "{cols} columns"
            );
        }
    }

    #[test]
    fn a_wrapped_hermes_interrupt_placeholder_in_the_transcript_never_reads_working() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::hermes(), 14, 40, started_at);
        let rule = "\u{2500}".repeat(30);

        // The quote wraps over two rows and is repainted above an idle prompt
        // that was drawn first, then more output follows the quote.
        let transitions = detector.feed(
            started_at,
            format!(
                "\x1b[2J\x1b[10;1H{rule}\r\n\u{276f} \r\n{rule}\x1b[1;1H\u{2695} msg=interrupt \u{b7} /queue \u{b7} /bg \u{b7} /steer \u{b7} Ctrl+C cancel\r\nquoted above"
            )
            .as_bytes(),
        );

        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Idle, StateSource::Screen)]
        );
    }

    #[test]
    fn a_hermes_interrupt_placeholder_in_the_transcript_above_the_idle_prompt_never_reads_working()
    {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::hermes(), 14, 100, started_at);
        let rule = "\u{2500}".repeat(60);

        // The idle prompt is drawn first and the assistant's quote of the
        // working line is repainted above it, so the quote is never the last
        // non-empty line.
        let transitions = detector.feed(
            started_at,
            format!(
                "\x1b[2J\x1b[10;1H{rule}\r\n\u{276f} \r\n{rule}\x1b[1;1H\u{2695} \u{276f} msg=interrupt \u{b7} /queue \u{b7} /bg \u{b7} /steer \u{b7} Ctrl+C cancel\r\nquoted above"
            )
            .as_bytes(),
        );

        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Idle, StateSource::Screen)]
        );
    }

    #[test]
    fn a_hermes_idle_redraw_over_an_old_working_prompt_line_never_latches_working() {
        let started_at = instant();
        let mut detector = steady_detector(&RuntimeRef::hermes(), 14, 80, started_at);
        // An old transcript line holds a bare staff prompt, far above the
        // idle prompt.
        let mut screen = String::from("\x1b[2J\x1b[H\u{2695} \u{276f}\r\n");
        for line in 1..=8 {
            let _ = write!(screen, "out {line}\r\n");
        }
        screen.push_str("\u{276f} ");
        detector.feed(started_at, screen.as_bytes());
        assert_eq!(
            detector.state.published_activity(),
            Some(AgentActivity::Idle),
            "the finished frame reads idle"
        );
        let sightings = detector.latch_sightings;
        let evaluations = detector.latch_evaluations;

        // The redraw erases from the third row down, which leaves the old
        // staff prompt inside the bottom window, then reprints the rest.
        let mut redraw = String::from("\x1b[3;1H\x1b[J");
        for line in 2..=8 {
            let _ = write!(redraw, "out {line}\r\n");
        }
        redraw.push_str("\u{276f} ");
        let transitions = detector.feed(started_at + Duration::from_millis(10), redraw.as_bytes());

        assert!(
            detector.latch_evaluations > evaluations,
            "the gate was open"
        );
        assert_eq!(detector.latch_sightings, sightings, "{transitions:?}");
    }

    #[test]
    fn generic_shell_manifest_blocked_outranks_working_within_one_screen() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        // "working" and "action required" both appear; blocked rules carry the
        // higher priority, so the resolved activity is Blocked.
        assert_eq!(
            detector.feed(started_at, b"\x1b[2J\x1b[Hworking: action required"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }
}
