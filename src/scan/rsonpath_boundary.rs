//! The crate's only gateway to the rsonpath engine's **runtime** API — there is
//! exactly one byte-scanning engine, and this module is named after it so the
//! quarantine's target is unmistakable.
//!
//! Every call that feeds raw — possibly adversarial — input to the engine lives
//! here, wrapped in a panic absorber, and nowhere else. rsonpath is an unsafe-heavy
//! dependency that has been observed to panic on malformed input (found by
//! `just fuzz`: a slice-index panic in its match-writing path), while this crate's
//! contract is termination *without* panics; this boundary turns such panics into
//! [`ScanError::Engine`].
//!
//! `catch_unwind` and panic-payload handling are a quarantine measure for the
//! external engine ONLY. Crate-internal code is panic-free by construction (the lint
//! profile denies panicking APIs), so nothing outside this module should reach for
//! these techniques — and nothing in here is visible outside it beyond the two
//! wrapper functions.
//!
//! The engine's *compile-time* API (`RsonpathEngine::compile_query`, used by the
//! splitter) is deliberately not routed through here: it consumes already-validated
//! queries rather than raw input, and reports failure as an ordinary `Result` that
//! the splitter treats as a DOM fallback.

use super::ScanError;
use rsonpath::engine::error::EngineError;
use rsonpath::engine::{Engine as _, RsonpathEngine};
use rsonpath::input::BorrowedBytes;
use rsonpath::result::{Match, MatchSpan, Sink};
use std::fmt::{self, Display};

/// Runs `engine` over `input`, collecting every match into `sink` and absorbing
/// engine panics.
pub(super) fn collect_matches<S: Sink<Match>>(
    engine: &RsonpathEngine,
    input: &BorrowedBytes<'_>,
    sink: &mut S,
) -> Result<(), ScanError> {
    absorb_engine_panic(|| engine.matches(input, sink))
}

/// Runs `engine` over `input`, collecting each match's approximate span into
/// `spans` (no byte copying) and absorbing engine panics.
pub(super) fn collect_approximate_spans(
    engine: &RsonpathEngine,
    input: &BorrowedBytes<'_>,
    spans: &mut Vec<MatchSpan>,
) -> Result<(), ScanError> {
    absorb_engine_panic(|| engine.approximate_spans(input, spans))
}

/// Runs one engine call, absorbing any panic from inside the engine into
/// [`ScanError::Engine`]. `AssertUnwindSafe` is sound here: on unwind, everything
/// the closure touched (sinks, spans) is either discarded or read only through the
/// caller's own always-valid fields. Under `panic = "abort"` there is nothing to
/// catch and the process aborts; the global panic hook still runs for caught panics,
/// so a message may be logged.
fn absorb_engine_panic<T>(
    operation: impl FnOnce() -> Result<T, EngineError>,
) -> Result<T, ScanError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(outcome) => outcome.map_err(engine_error),
        Err(payload) => Err(ScanError::Engine {
            source: Box::new(EnginePanic(panic_message(payload.as_ref()))),
        }),
    }
}

/// Wraps an engine failure as [`ScanError::Engine`], boxed so the typed source
/// survives without naming the engine's error type in this crate's public API.
fn engine_error(error: EngineError) -> ScanError {
    ScanError::Engine {
        source: Box::new(error),
    }
}

/// The engine panicked instead of returning an error — an upstream bug, surfaced as
/// an ordinary [`ScanError::Engine`] at this boundary.
#[derive(Debug)]
struct EnginePanic(String);

impl Display for EnginePanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the engine panicked: {}", self.0)
    }
}

impl std::error::Error for EnginePanic {}

/// Best-effort extraction of a panic payload's message.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic payload".to_owned())
        },
        |message| (*message).to_owned(),
    )
}
