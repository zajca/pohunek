//! Capture of the structured events an in-process worker logs.
//!
//! A scenario that has to see what the worker decided, rather than what a
//! client was told within its own time budget, reads the worker's event here.

// Rust guideline compliant 2026-11-11

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::layer::SubscriberExt;

/// One event the worker logged, with every field rendered as text.
#[derive(Debug, Clone)]
pub(crate) struct LoggedEvent {
    name: String,
    fields: BTreeMap<String, String>,
}

/// Captures every event the worker logs on the test thread.
///
/// Scenarios that use it run the worker in process on a current-thread
/// runtime, so the worker logs on the thread that installed this capture.
#[derive(Clone, Default)]
pub(crate) struct EventLog(Arc<Mutex<Vec<LoggedEvent>>>);

impl EventLog {
    /// Installs this capture as the thread's default subscriber, after a
    /// process-wide anchor dispatcher, as the crafted-claim capture does.
    pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
        static ANCHOR: OnceLock<tracing::Dispatch> = OnceLock::new();
        ANCHOR.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
        tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
    }

    /// The fields of every logged event named `name`, in logging order.
    pub(crate) fn named(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .expect("event log")
            .iter()
            .filter(|event| event.name == name)
            .map(|event| event.fields.clone())
            .collect()
    }

    /// Every logged event name, field name and field value, one per line:
    /// the text of every record the worker logged.
    pub(crate) fn rendered(&self) -> String {
        self.0
            .lock()
            .expect("event log")
            .iter()
            .flat_map(|event| {
                std::iter::once(event.name.clone()).chain(
                    event
                        .fields
                        .iter()
                        .flat_map(|(name, value)| [name.clone(), value.clone()]),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLog {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = FieldText::default();
        event.record(&mut fields);
        self.0.lock().expect("event log").push(LoggedEvent {
            name: event.metadata().name().to_owned(),
            fields: fields.0,
        });
    }
}

/// Renders every field of an event as text, including its message.
#[derive(Default)]
struct FieldText(BTreeMap<String, String>);

impl tracing::field::Visit for FieldText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}
