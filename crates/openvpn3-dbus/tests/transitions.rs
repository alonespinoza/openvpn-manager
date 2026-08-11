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
    m.handle(Event::StatusChanged {
        session_path: session.into(),
        status: connected(),
    });
    assert_eq!(m.state(), ConnectionState::Connected);
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
            Command::Connect {
                session_path: "/s/1".into()
            },
        ],
        "LogForward must precede Connect"
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
        .position(|c| matches!(c, Command::Ready { .. }));
    assert!(provide.is_some() && ready.is_some());
    assert!(
        provide < ready,
        "input must be provided before Ready is called"
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
