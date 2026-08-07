//! `session_id` helpers for `tracing` spans.
//!
//! DESIGN.md observability: structured logs carry `session_id` on session-scoped work.
//! Use [`session_span`] / [`session_span_at`] at session boundaries so subscribers
//! (JSON or plain) emit the field on every nested event.

use tracing::{span, Level, Span};

/// Create an `INFO` span named `name` with a `session_id` field.
///
/// ```ignore
/// let _guard = remotelink_common::session_span("handle_intent", &session_id).entered();
/// tracing::info!("pending");
/// ```
pub fn session_span(name: &'static str, session_id: &str) -> Span {
    span!(Level::INFO, "session", session.name = name, session_id = %session_id)
}

/// Create a span at an explicit level with `session_id`.
pub fn session_span_at(level: Level, name: &'static str, session_id: &str) -> Span {
    match level {
        Level::ERROR => {
            span!(Level::ERROR, "session", session.name = name, session_id = %session_id)
        }
        Level::WARN => {
            span!(Level::WARN, "session", session.name = name, session_id = %session_id)
        }
        Level::INFO => {
            span!(Level::INFO, "session", session.name = name, session_id = %session_id)
        }
        Level::DEBUG => {
            span!(Level::DEBUG, "session", session.name = name, session_id = %session_id)
        }
        Level::TRACE => {
            span!(Level::TRACE, "session", session.name = name, session_id = %session_id)
        }
    }
}

/// Field name used for session correlation (`session_id`).
pub const SESSION_ID_FIELD: &str = "session_id";

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{EnvFilter, Registry};

    fn with_test_subscriber<F: FnOnce()>(f: F) {
        // Max level so TRACE spans are enabled during tests.
        let subscriber = Registry::default()
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .with(EnvFilter::new("trace"));
        tracing::subscriber::with_default(subscriber, f);
    }

    #[test]
    fn session_span_carries_session_id_field() {
        with_test_subscriber(|| {
            let span = session_span("test_op", "sess-abc-123");
            assert_eq!(span.metadata().map(|m| m.name()), Some("session"));
            let _g = span.entered();
            tracing::info!("inside session span");
        });
    }

    #[test]
    fn session_span_at_levels() {
        with_test_subscriber(|| {
            for level in [
                Level::ERROR,
                Level::WARN,
                Level::INFO,
                Level::DEBUG,
                Level::TRACE,
            ] {
                let s = session_span_at(level, "lvl", "s1");
                assert!(s.metadata().is_some(), "level {level:?}");
            }
        });
    }

    #[test]
    fn field_constant() {
        assert_eq!(SESSION_ID_FIELD, "session_id");
    }

    /// Ensure we can attach a subscriber without depending on global init in tests.
    #[test]
    fn with_subscriber_smoke() {
        with_test_subscriber(|| {
            let _g = session_span("ws_accept", "sess-42").entered();
            tracing::info!(result = "accept", "session accepted");
        });
    }
}
