//! U3/U8 — the log ring buffer behind the failure log window.
//!
//! openvpn3 offers no way to read back a completed session's log, so whatever
//! is captured live is all there will ever be (KTD4). The retention rules here
//! are the difference between a diagnosable failure and a blank pane.

use openvpn3_dbus::logbuf::{LogEntry, LogStore, SessionLog};

fn entry(message: &str) -> LogEntry {
    LogEntry {
        group: 2,
        level: 3,
        message: message.to_owned(),
    }
}

#[track_caller]
fn captured_messages(store: &LogStore) -> Vec<String> {
    match store.most_recent() {
        SessionLog::Captured { entries, .. } => {
            entries.iter().map(|e| e.message.clone()).collect()
        }
        other => panic!("expected captured log, got {other:?}"),
    }
}

#[test]
fn entries_are_returned_in_arrival_order() {
    let mut store = LogStore::new(64);
    store.begin_session("/net/openvpn/v3/sessions/1");
    store.push("/net/openvpn/v3/sessions/1", entry("first"));
    store.push("/net/openvpn/v3/sessions/1", entry("second"));
    store.push("/net/openvpn/v3/sessions/1", entry("third"));

    assert_eq!(captured_messages(&store), ["first", "second", "third"]);
}

#[test]
fn at_capacity_the_oldest_lines_are_dropped() {
    let mut store = LogStore::new(3);
    store.begin_session("/s/1");
    for n in 1..=5 {
        store.push("/s/1", entry(&format!("line {n}")));
    }

    assert_eq!(captured_messages(&store), ["line 3", "line 4", "line 5"]);
}

/// The session object is gone by the time the user opens the log window after a
/// failure, so the most recent buffer has to outlive its session.
#[test]
fn the_most_recent_buffer_survives_its_session_being_removed() {
    let mut store = LogStore::new(64);
    store.begin_session("/s/1");
    store.push("/s/1", entry("auth failed"));
    store.remove_session("/s/1");

    assert_eq!(captured_messages(&store), ["auth failed"]);
}

/// Older sessions are not kept — this is a failure escape hatch, not history.
#[test]
fn older_session_buffers_are_dropped_on_removal() {
    let mut store = LogStore::new(64);
    store.begin_session("/s/1");
    store.push("/s/1", entry("old session line"));

    store.begin_session("/s/2");
    store.push("/s/2", entry("new session line"));

    store.remove_session("/s/1");

    assert_eq!(captured_messages(&store), ["new session line"]);
    assert!(
        !store.has_buffer("/s/1"),
        "a removed non-current session should not retain its buffer"
    );
}

/// AE3-adjacent: a retry must not read as a continuation of the failed attempt.
#[test]
fn a_new_attempt_starts_a_fresh_buffer() {
    let mut store = LogStore::new(64);
    store.begin_session("/s/1");
    store.push("/s/1", entry("first attempt failed"));
    store.remove_session("/s/1");

    store.begin_session("/s/2");
    store.push("/s/2", entry("second attempt"));

    assert_eq!(captured_messages(&store), ["second attempt"]);
}

// ------------------------------------------------------- The not-captured case

/// A session adopted from outside the applet never had LogForward enabled, so
/// there is nothing to show. Saying so beats an empty pane, which reads as
/// either a bug or a clean run — and it is neither.
#[test]
fn a_session_never_begun_reports_not_captured() {
    let mut store = LogStore::new(64);
    store.adopt_external_session("/s/external");

    match store.most_recent() {
        SessionLog::NotCaptured { path } => assert_eq!(path, "/s/external"),
        other => panic!("expected NotCaptured for an adopted session, got {other:?}"),
    }
}

/// Same user-facing outcome when a buffer exists but nothing ever arrived.
#[test]
fn an_empty_buffer_reports_not_captured_rather_than_an_empty_list() {
    let mut store = LogStore::new(64);
    store.begin_session("/s/1");

    assert!(
        matches!(store.most_recent(), SessionLog::NotCaptured { .. }),
        "an empty buffer must not present as a captured-but-blank log"
    );
}

#[test]
fn with_no_sessions_at_all_there_is_nothing_to_show() {
    let store = LogStore::new(64);
    assert!(matches!(store.most_recent(), SessionLog::None));
}

// ------------------------------------------------------------- Applet's own notes

/// KTD3 routes unmapped statuses here, and KTD8 routes unsupported attention
/// requests here. Without this the user sees a failed icon and a log that never
/// mentions why.
#[test]
fn applet_notes_land_in_the_buffer_alongside_openvpn3_lines() {
    let mut store = LogStore::new(64);
    store.begin_session("/s/1");
    store.push("/s/1", entry("from openvpn3"));
    store.note("/s/1", "This profile uses browser-based authentication.");

    let messages = captured_messages(&store);
    assert_eq!(messages.len(), 2);
    assert!(messages[1].contains("browser-based"));
}

/// A note about a session that was never begun must still be visible — the
/// unsupported-attention path can fire before anything else was captured.
#[test]
fn a_note_creates_a_buffer_when_none_exists() {
    let mut store = LogStore::new(64);
    store.note("/s/1", "Unsupported request.");

    assert_eq!(captured_messages(&store), ["Unsupported request."]);
}

// ---------------------------------------------------------------- Robustness

/// Signals for a session the applet is not tracking must not panic or
/// resurrect a dropped buffer as the current one.
#[test]
fn pushing_to_an_untracked_session_does_not_disturb_the_current_one() {
    let mut store = LogStore::new(64);
    store.begin_session("/s/current");
    store.push("/s/current", entry("mine"));

    store.push("/s/stranger", entry("not mine"));

    assert_eq!(captured_messages(&store), ["mine"]);
}

#[test]
fn removing_an_unknown_session_is_a_no_op() {
    let mut store = LogStore::new(64);
    store.begin_session("/s/1");
    store.push("/s/1", entry("kept"));

    store.remove_session("/s/does-not-exist");

    assert_eq!(captured_messages(&store), ["kept"]);
}
