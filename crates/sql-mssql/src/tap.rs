//! Server messages and row counts, read from tiberius's trace events.
//!
//! tiberius 0.12's result stream yields only column metadata and rows: INFO
//! tokens (PRINT, RAISERROR below severity 11, "Changed database context…")
//! and DONE tokens (rows affected) are decoded and dropped, but each is
//! logged as a `tracing` event first. A per-connection subscriber, installed
//! only while that connection's statement is polled (`WithSubscriber`), turns
//! those events back into data. Events fire synchronously while the stream is
//! polled, so whatever the tap holds when the stream yields an item came
//! before that item on the wire.
//!
//! The wording matched here is tiberius 0.12.3's (`Done with status …`,
//! `Database change from '…' to '…'`), hence the exact version pin. If the
//! events ever go missing (a static `tracing` max-level, a version bump) the
//! driver still works, only without notices and affected-row counts.

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Dispatch, Event, Level, Metadata, Subscriber};

#[derive(Clone, Debug, PartialEq)]
pub enum Tapped {
    /// An INFO message for the user.
    Message(String),
    /// The end of one statement; `rows` when the server counted them.
    Done { rows: Option<u64> },
    /// `USE` switched the session's database.
    Database(String),
}

#[derive(Default)]
pub struct Tap(Mutex<Vec<Tapped>>);

impl Tap {
    pub fn take(&self) -> Vec<Tapped> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The dispatcher to run a statement under.
    pub fn dispatch(self: &Arc<Self>) -> Dispatch {
        Dispatch::new(TapSubscriber(self.clone()))
    }
}

struct TapSubscriber(Arc<Tap>);

fn from_token_stream(m: &Metadata<'_>) -> bool {
    m.is_event() && m.target().starts_with("tiberius::tds::stream")
}

/// Env-change and login events share INFO with server messages; these are
/// tiberius's own texts, not the server's.
const OWN_INFO: [&str; 10] = [
    "Packet size change",
    "SQL collation change",
    "Begin transaction",
    "Commit transaction",
    "Rollback transaction",
    "Defect transaction",
    "Server requested routing",
    "Fallback mirror server",
    "Ignored env change",
    "FeatureExtAck with",
];

pub fn classify(level: Level, text: &str) -> Option<Tapped> {
    if level == Level::TRACE {
        let status = text.strip_prefix("Done with status ")?;
        if !status.contains("Count") || status.contains("Error") {
            return Some(Tapped::Done { rows: None });
        }
        // "… (1 row left)" / "… (42 rows left)" / no suffix for zero.
        let rows = status
            .rsplit_once(" (")
            .and_then(|(_, tail)| tail.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        return Some(Tapped::Done { rows: Some(rows) });
    }
    if level != Level::INFO {
        return None;
    }
    if let Some(change) = text.strip_prefix("Database change from '") {
        let new = change.rsplit_once(" to '")?.1.strip_suffix('\'')?;
        return Some(Tapped::Database(new.to_owned()));
    }
    if OWN_INFO.iter().any(|p| text.starts_with(p)) {
        return None;
    }
    Some(Tapped::Message(text.to_owned()))
}

/// Collects the `message` field, giving up after `cap` bytes: trace events
/// also carry whole rows, which are not worth formatting.
struct Message {
    text: String,
    cap: usize,
}

impl fmt::Write for Message {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.text.len() + s.len() > self.cap {
            return Err(fmt::Error);
        }
        self.text.push_str(s);
        Ok(())
    }
}

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self, "{value:?}");
        }
    }
}

impl Subscriber for TapSubscriber {
    fn register_callsite(&self, m: &'static Metadata<'static>) -> Interest {
        if from_token_stream(m) { Interest::sometimes() } else { Interest::never() }
    }

    fn enabled(&self, m: &Metadata<'_>) -> bool {
        from_token_stream(m) && matches!(*m.level(), Level::INFO | Level::TRACE)
    }

    fn event(&self, event: &Event<'_>) {
        let level = *event.metadata().level();
        let mut msg = Message { text: String::new(), cap: if level == Level::INFO { 64 * 1024 } else { 160 } };
        event.record(&mut msg);
        if let Some(t) = classify(level, &msg.text) {
            self.0.0.lock().unwrap_or_else(|e| e.into_inner()).push(t);
        }
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_tokens_carry_row_counts_only_when_counted() {
        let done = |s: &str| classify(Level::TRACE, s);
        assert_eq!(done("Done with status BitFlags<DoneStatus>(0b10000, Count) (42 rows left)"), Some(Tapped::Done { rows: Some(42) }));
        assert_eq!(done("Done with status BitFlags<DoneStatus>(0b10001, More | Count) (1 row left)"), Some(Tapped::Done { rows: Some(1) }));
        assert_eq!(done("Done with status BitFlags<DoneStatus>(0b10000, Count)"), Some(Tapped::Done { rows: Some(0) }));
        assert_eq!(done("Done with status BitFlags<DoneStatus>(0b1, More)"), Some(Tapped::Done { rows: None }));
        assert_eq!(done("Done with status BitFlags<DoneStatus>(0b10010, Error | Count)"), Some(Tapped::Done { rows: None }));
        assert_eq!(done("TokenRow { data: [] }"), None);
    }

    #[test]
    fn info_events_split_into_messages_and_database_switches() {
        assert_eq!(classify(Level::INFO, "hello from PRINT"), Some(Tapped::Message("hello from PRINT".into())));
        assert_eq!(classify(Level::INFO, "Database change from 'master' to 'app db'"), Some(Tapped::Database("app db".into())));
        assert_eq!(classify(Level::INFO, "Begin transaction"), None);
        assert_eq!(classify(Level::ERROR, "boom"), None);
    }

    /// The real wiring: events emitted under the dispatcher land in the tap,
    /// and only there.
    #[test]
    fn the_subscriber_collects_only_while_installed() {
        let tap = Arc::new(Tap::default());
        tracing::dispatcher::with_default(&tap.dispatch(), || {
            // Same target and shape as tiberius's own events.
            tracing::event!(target: "tiberius::tds::stream::token", Level::INFO, "{}", "printed");
            tracing::event!(target: "tiberius::tds::stream::token", Level::TRACE, "{}", "Done with status BitFlags<DoneStatus>(0b10000, Count) (3 rows left)");
            tracing::event!(target: "somebody::else", Level::INFO, "{}", "ignored");
        });
        tracing::event!(target: "tiberius::tds::stream::token", Level::INFO, "{}", "outside");
        assert_eq!(tap.take(), vec![Tapped::Message("printed".into()), Tapped::Done { rows: Some(3) }]);
    }
}
