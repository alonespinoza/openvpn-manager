//! U5/U6 — the transition machine, and the single-session invariant (AE1).
//!
//! `NewTunnel` and `Connect` both return before the tunnel is up, so "disconnect
//! then connect" issued back-to-back leaves a window where two sessions exist.
//! These tests pin the serialisation that closes it.

use openvpn3_dbus::attention::{ClientAttentionGroup as Group, ClientAttentionType as Type};
use openvpn3_dbus::machine::{Command, Event, InputResponse, Machine};
use openvpn3_dbus::status::{ConnectionState, Status, StatusMajor, StatusMinor};

fn status(major: StatusMajor, minor: StatusMinor) -> Status {
    Status {
        major,
        minor,
        message: String::new(),
    }
}

fn connected() -> Status {
    status(StatusMajor::Connection, StatusMinor::ConnConnected)
}

fn disconnected() -> Status {
    status(StatusMajor::Connection, StatusMinor::ConnDisconnected)
}

/// Drive a machine to "connected on `config`" and return it.
fn machine_connected_to(config: &str, session: &str) -> Machine {
    let mut m = Machine::new();
    assert_eq!(
        m.handle(Event::ProfileSelected {
            config_path: config.into()
        }),
        vec![Command::NewTunnel {
            config_path: config.into()
        }]
    );
    m.handle(Event::SessionCreated {
        session_path: session.into(),
        config_path: config.into(),
    });
    m.handle(Event::SessionReady {
        session_path: session.into(),
    });
    m.handle(Event::StatusChanged {
        session_path: session.into(),
        status: connected(),
    });
    assert_eq!(m.state(), ConnectionState::Connected);
    m
}

/// Drive a fresh machine to "session created, awaiting readiness".
fn machine_starting(config: &str, session: &str) -> Machine {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: config.into(),
    });
    m.handle(Event::SessionCreated {
        session_path: session.into(),
        config_path: config.into(),
    });
    m
}

// ------------------------------------------------------------------ Ordering

/// KTD4: log forwarding has to be on before the handshake starts, or a failed
/// connect has nothing to show in the log window.
#[test]
fn log_forwarding_is_enabled_before_connect() {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/a".into(),
    });

    let commands = m.handle(Event::SessionCreated {
        session_path: "/s/1".into(),
        config_path: "/cfg/a".into(),
    });

    assert_eq!(
        commands,
        vec![
            Command::EnableLogForward {
                session_path: "/s/1".into()
            },
            Command::CheckReady {
                session_path: "/s/1".into()
            },
        ],
        "LogForward must precede readiness, and Connect must wait for it"
    );
}

// -------------------------------------------------- Readiness before connect

/// openvpn3 reports a missing password by failing Ready, not by emitting
/// AttentionRequired. Connecting without checking is why username/password
/// profiles silently stalled with nothing asked of the user.
#[test]
fn connect_waits_for_readiness() {
    let mut m = machine_starting("/cfg/a", "/s/1");

    let commands = m.handle(Event::SessionReady {
        session_path: "/s/1".into(),
    });

    assert_eq!(
        commands,
        vec![Command::Connect {
            session_path: "/s/1".into()
        }]
    );
}

#[test]
fn missing_credentials_open_the_prompt_and_do_not_connect() {
    let mut m = machine_starting("/cfg/a", "/s/1");

    let commands = m.handle(Event::CredentialsRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::UserPassword,
        message: String::new(),
    });

    assert_eq!(m.state(), ConnectionState::AuthPending);
    assert!(
        matches!(commands.as_slice(), [Command::OpenPrompt { .. }]),
        "expected a prompt, got {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, Command::Connect { .. })),
        "a session missing credentials must not be connected"
    );
}

/// openvpn3 can want more than one thing, which is why the reference client
/// loops on Ready rather than connecting straight after providing input.
#[test]
fn submitting_credentials_rechecks_readiness_rather_than_connecting_blind() {
    let mut m = machine_starting("/cfg/a", "/s/1");
    m.handle(Event::CredentialsRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::UserPassword,
        message: String::new(),
    });

    let commands = m.handle(Event::PromptSubmitted {
        session_path: "/s/1".into(),
        responses: vec![InputResponse {
            r#type: Type::Credentials,
            group: Group::UserPassword,
            id: 0,
            value: "hunter2".into(),
        }],
    });

    let provide = commands
        .iter()
        .position(|c| matches!(c, Command::ProvideInput { .. }));
    let recheck = commands
        .iter()
        .position(|c| matches!(c, Command::CheckReady { .. }));

    assert!(provide.is_some(), "input must be provided");
    assert!(recheck.is_some(), "readiness must be re-checked");
    assert!(provide < recheck, "provide input before re-checking");
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, Command::Connect { .. })),
        "connecting straight after input skips the second thing openvpn3 may want"
    );
}

// ----------------------------------------------------------------- AE1

/// The heart of AE1: selecting a second profile must not start it until the
/// first is gone.
#[test]
fn switching_disconnects_first_and_does_not_connect_until_the_old_session_is_gone() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");

    let commands = m.handle(Event::ProfileSelected {
        config_path: "/cfg/b".into(),
    });

    assert_eq!(
        commands,
        vec![Command::Disconnect {
            session_path: "/s/1".into()
        }],
        "the only immediate action is tearing down the active session"
    );
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, Command::NewTunnel { .. })),
        "NewTunnel must not be issued while the old session still exists"
    );

    // Only once the old session reports gone does the new one start.
    let commands = m.handle(Event::StatusChanged {
        session_path: "/s/1".into(),
        status: disconnected(),
    });
    assert_eq!(
        commands,
        vec![Command::NewTunnel {
            config_path: "/cfg/b".into()
        }]
    );
}

/// The teardown can also be observed as the object vanishing rather than as a
/// status change; both must complete the switch.
#[test]
fn session_removal_also_completes_a_pending_switch() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/b".into(),
    });

    let commands = m.handle(Event::SessionRemoved {
        session_path: "/s/1".into(),
    });

    assert_eq!(
        commands,
        vec![Command::NewTunnel {
            config_path: "/cfg/b".into()
        }]
    );
}

/// Never two active sessions from the machine's perspective.
#[test]
fn only_one_session_is_ever_active() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");
    assert_eq!(m.active_session(), Some("/s/1"));

    m.handle(Event::ProfileSelected {
        config_path: "/cfg/b".into(),
    });
    assert_eq!(
        m.active_session(),
        None,
        "during teardown no session is active"
    );

    m.handle(Event::StatusChanged {
        session_path: "/s/1".into(),
        status: disconnected(),
    });
    m.handle(Event::SessionCreated {
        session_path: "/s/2".into(),
        config_path: "/cfg/b".into(),
    });
    assert_eq!(m.active_session(), Some("/s/2"));
}

/// Impatient clicking must not fan out into multiple connects.
#[test]
fn reselecting_mid_transition_yields_exactly_one_connect_to_the_last_choice() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");

    m.handle(Event::ProfileSelected {
        config_path: "/cfg/b".into(),
    });
    let interim = m.handle(Event::ProfileSelected {
        config_path: "/cfg/c".into(),
    });
    assert!(
        interim.is_empty(),
        "a reselection mid-teardown starts nothing new, got {interim:?}"
    );

    let commands = m.handle(Event::StatusChanged {
        session_path: "/s/1".into(),
        status: disconnected(),
    });
    assert_eq!(
        commands,
        vec![Command::NewTunnel {
            config_path: "/cfg/c".into()
        }],
        "the last selection wins, and only one tunnel is started"
    );
}

// ------------------------------------------------------------ Disconnect

#[test]
fn disconnecting_the_active_session_issues_disconnect_and_settles() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");

    let commands = m.handle(Event::DisconnectRequested);
    assert_eq!(
        commands,
        vec![Command::Disconnect {
            session_path: "/s/1".into()
        }]
    );

    let commands = m.handle(Event::StatusChanged {
        session_path: "/s/1".into(),
        status: disconnected(),
    });
    assert!(commands.is_empty(), "nothing should follow a plain disconnect");
    assert_eq!(m.state(), ConnectionState::Disconnected);
}

#[test]
fn disconnecting_with_nothing_active_is_a_no_op() {
    let mut m = Machine::new();
    assert!(m.handle(Event::DisconnectRequested).is_empty());
}

// -------------------------------------------------------------------- AE4

/// A session started from a terminal is adopted, so the icon tells the truth
/// about state the applet did not cause.
#[test]
fn an_externally_started_session_is_adopted() {
    let mut m = Machine::new();

    m.handle(Event::ExternalSessionSeen {
        session_path: "/s/ext".into(),
        config_path: "/cfg/a".into(),
        status: connected(),
    });

    assert_eq!(m.state(), ConnectionState::Connected);
    assert_eq!(m.active_config(), Some("/cfg/a"));
}

/// AE4 proper: a disconnect performed outside the applet moves the icon.
#[test]
fn an_external_disconnect_updates_state_without_any_user_action() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");

    m.handle(Event::StatusChanged {
        session_path: "/s/1".into(),
        status: disconnected(),
    });

    assert_eq!(m.state(), ConnectionState::Disconnected);
}

#[test]
fn status_for_an_untracked_session_is_ignored() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");

    let commands = m.handle(Event::StatusChanged {
        session_path: "/s/stranger".into(),
        status: disconnected(),
    });

    assert!(commands.is_empty());
    assert_eq!(
        m.state(),
        ConnectionState::Connected,
        "a stranger's status must not move our icon"
    );
}

/// KTD3's fallthrough has to reach the log, or the gap is invisible.
#[test]
fn an_unmapped_status_leaves_state_alone_and_records_a_note() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");

    let commands = m.handle(Event::StatusChanged {
        session_path: "/s/1".into(),
        status: status(StatusMajor::Pkcs11, StatusMinor::Pkcs11Sign),
    });

    assert_eq!(m.state(), ConnectionState::Connected);
    assert!(
        matches!(commands.as_slice(), [Command::Note { .. }]),
        "expected a single Note, got {commands:?}"
    );
}

// -------------------------------------------------------------- Credentials

#[test]
fn a_supported_attention_request_opens_the_prompt_and_shows_auth_pending() {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::SessionCreated {
        session_path: "/s/1".into(),
        config_path: "/cfg/a".into(),
    });

    let commands = m.handle(Event::AttentionRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::ChallengeDynamic,
        message: "Enter your token".into(),
    });

    assert_eq!(m.state(), ConnectionState::AuthPending);
    assert!(matches!(
        commands.as_slice(),
        [Command::OpenPrompt { .. }]
    ));
}

/// AE2 — cancel ends the attempt; no session is left running.
#[test]
fn cancelling_the_prompt_disconnects_and_shows_failed_not_connecting() {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::SessionCreated {
        session_path: "/s/1".into(),
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::AttentionRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::ChallengeStatic,
        message: "token".into(),
    });

    let commands = m.handle(Event::PromptCancelled {
        session_path: "/s/1".into(),
    });

    assert_eq!(m.state(), ConnectionState::Failed);
    assert!(
        commands.contains(&Command::Disconnect {
            session_path: "/s/1".into()
        }),
        "cancel must tear the session down, got {commands:?}"
    );
    assert_eq!(m.active_session(), None, "no session may remain running");
}

/// A duplicate or late signal must not reopen a window the user dismissed.
#[test]
fn an_attention_request_after_cancellation_is_ignored() {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::SessionCreated {
        session_path: "/s/1".into(),
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::AttentionRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::ChallengeStatic,
        message: "token".into(),
    });
    m.handle(Event::PromptCancelled {
        session_path: "/s/1".into(),
    });

    let commands = m.handle(Event::AttentionRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::ChallengeStatic,
        message: "token again".into(),
    });

    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, Command::OpenPrompt { .. })),
        "the prompt must not reopen, got {commands:?}"
    );
}

#[test]
fn submitting_the_prompt_provides_input_then_confirms_readiness() {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::SessionCreated {
        session_path: "/s/1".into(),
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::AttentionRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::UserPassword,
        message: String::new(),
    });

    let commands = m.handle(Event::PromptSubmitted {
        session_path: "/s/1".into(),
        responses: vec![InputResponse {
            r#type: Type::Credentials,
            group: Group::UserPassword,
            id: 0,
            value: "alice".into(),
        }],
    });

    let provide = commands
        .iter()
        .position(|c| matches!(c, Command::ProvideInput { .. }));
    let ready = commands
        .iter()
        .position(|c| matches!(c, Command::CheckReady { .. }));
    assert!(provide.is_some() && ready.is_some());
    assert!(
        provide < ready,
        "input must be provided before readiness is re-checked"
    );
}

/// KTD8 — an unsupported request tears the session down and records why,
/// rather than hanging behind a "connecting" icon.
#[test]
fn an_unsupported_attention_request_fails_loudly() {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::SessionCreated {
        session_path: "/s/1".into(),
        config_path: "/cfg/a".into(),
    });

    let commands = m.handle(Event::AttentionRequired {
        session_path: "/s/1".into(),
        r#type: Type::Credentials,
        group: Group::OpenUrl,
        message: String::new(),
    });

    assert_eq!(m.state(), ConnectionState::Failed);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, Command::Note { .. })),
        "the reason must reach the log window"
    );
    assert!(commands.contains(&Command::Disconnect {
        session_path: "/s/1".into()
    }));
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, Command::OpenPrompt { .. })),
        "an unanswerable request must never open a prompt"
    );
}

// ---------------------------------------------------------------- Dismissal

#[test]
fn dismissing_a_failure_returns_to_disconnected() {
    let mut m = Machine::new();
    m.handle(Event::ProfileSelected {
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::SessionCreated {
        session_path: "/s/1".into(),
        config_path: "/cfg/a".into(),
    });
    m.handle(Event::StatusChanged {
        session_path: "/s/1".into(),
        status: status(StatusMajor::Connection, StatusMinor::ConnAuthFailed),
    });
    assert_eq!(m.state(), ConnectionState::Failed);

    m.handle(Event::FailureDismissed);
    assert_eq!(m.state(), ConnectionState::Disconnected);
}

// ------------------------------------------------- Recovering from a missed event

/// The wedge this guards against: a disconnect whose completion signal never
/// arrives used to leave the machine waiting forever, and every later profile
/// selection silently did nothing. The caller can now see what it is waiting on
/// and resolve it against reality.
#[test]
fn a_pending_teardown_is_visible_to_the_caller() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");
    assert_eq!(m.awaiting_teardown(), None);

    m.handle(Event::DisconnectRequested);
    assert_eq!(
        m.awaiting_teardown(),
        Some("/s/1"),
        "the caller must be able to tell what the machine is blocked on"
    );

    m.handle(Event::SessionRemoved {
        session_path: "/s/1".into(),
    });
    assert_eq!(m.awaiting_teardown(), None);
}

/// Selecting a profile while wedged must not be lost — once the teardown is
/// resolved, the queued choice still connects.
#[test]
fn a_selection_made_during_a_stuck_teardown_still_connects_once_resolved() {
    let mut m = machine_connected_to("/cfg/a", "/s/1");
    m.handle(Event::DisconnectRequested);

    // The completion signal never arrived; the user clicks a profile anyway.
    let commands = m.handle(Event::ProfileSelected {
        config_path: "/cfg/b".into(),
    });
    assert!(commands.is_empty());

    // Caller notices openvpn3 no longer lists the session and says so.
    let commands = m.handle(Event::SessionRemoved {
        session_path: "/s/1".into(),
    });
    assert_eq!(
        commands,
        vec![Command::NewTunnel {
            config_path: "/cfg/b".into()
        }],
        "the queued selection must survive the recovery"
    );
    assert_eq!(m.awaiting_teardown(), None);
}
