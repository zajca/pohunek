//! Replays a trace through a model and evaluates the assertions of its sidecar.

// Rust guideline compliant 2026-06-26

use crate::expect::{Assertion, Expect, Verdict};
use crate::model::{GridDump, GridModel, ModelError};
use crate::trace::{Record, Trace};

/// The result of one assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The assertion's id.
    pub id: String,
    /// Pass, or fail with the reason.
    pub verdict: Verdict,
}

/// A replay: every assertion's outcome and the final screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// One outcome per assertion, in sidecar order.
    pub outcomes: Vec<Outcome>,
    /// The screen after the last record.
    pub dump: GridDump,
}

/// A replay could not run to the end.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    /// The model failed to dump.
    #[error(transparent)]
    Model(#[from] ModelError),
    /// An assertion names a moment the trace does not have.
    #[error("assertion {id}: {reason}")]
    Moment {
        /// The assertion.
        id: String,
        /// Why the moment does not exist.
        reason: String,
    },
}

/// Number of records applied when `assertion` is evaluated.
fn moment(assertion: &Assertion, trace: &Trace) -> Result<usize, ReplayError> {
    let bad = |reason: String| ReplayError::Moment {
        id: assertion.id.clone(),
        reason,
    };
    if let Some(count) = assertion.after_record {
        return if count <= trace.records.len() {
            Ok(count)
        } else {
            Err(bad(format!(
                "after_record {count} but the trace has {} records",
                trace.records.len()
            )))
        };
    }
    for (nth, offset) in [(assertion.after_resize, 1), (assertion.before_resize, 0)] {
        if let Some(nth) = nth {
            return trace
                .records
                .iter()
                .enumerate()
                .filter(|(_, record)| matches!(record, Record::Resize { .. }))
                .nth(nth - 1)
                .map(|(index, _)| index + offset)
                .ok_or_else(|| bad(format!("resize {nth} does not exist in the trace")));
        }
    }
    Ok(trace.records.len())
}

/// Feeds `model` every record of `trace`.
///
/// `model` must have been created at the trace's initial size.
pub fn apply(model: &mut dyn GridModel, record: &Record) {
    match record {
        Record::Output(bytes) => model.feed(bytes),
        Record::Resize { rows, cols } => model.resize(*rows, *cols),
    }
}

/// Replays `trace` through `model` and evaluates `expect` (when given) at each
/// assertion's moment.
///
/// A failed assertion is a recorded [`Verdict::Fail`], not an error.
///
/// # Errors
///
/// Returns a [`ReplayError`] when the model cannot dump or an assertion names a
/// moment the trace does not have.
pub fn replay(
    model: &mut dyn GridModel,
    trace: &Trace,
    expect: Option<&Expect>,
) -> Result<Replay, ReplayError> {
    let assertions = expect.map_or(&[][..], |expect| expect.assertion.as_slice());
    let moments = assertions
        .iter()
        .map(|assertion| moment(assertion, trace))
        .collect::<Result<Vec<_>, _>>()?;
    let mut verdicts: Vec<Option<Verdict>> = vec![None; assertions.len()];
    for applied in 0..=trace.records.len() {
        if moments.contains(&applied) {
            let dump = model.dump()?;
            for (index, assertion) in assertions.iter().enumerate() {
                if moments[index] == applied {
                    verdicts[index] = Some(assertion.check.evaluate(&dump));
                }
            }
        }
        if let Some(record) = trace.records.get(applied) {
            apply(model, record);
        }
    }
    let outcomes = assertions
        .iter()
        .zip(verdicts)
        .map(|(assertion, verdict)| Outcome {
            id: assertion.id.clone(),
            verdict: verdict.unwrap_or_else(|| Verdict::Fail("never evaluated".to_owned())),
        })
        .collect();
    Ok(Replay {
        outcomes,
        dump: model.dump()?,
    })
}
