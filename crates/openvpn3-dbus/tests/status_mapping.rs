//! U3 — the KTD3 status mapping table.
//!
//! Every row of the table in the plan gets a case here. The mapping is the
//! thing that decides what the panel icon claims, so a wrong entry produces a
//! display that is trusted and wrong — the exact failure the Product Contract
//! rejects.

use openvpn3_dbus::status::{ConnectionState, StateTransition, StatusMajor, StatusMinor, map_status};

/// Convenience: assert a (major, minor) pair maps to a concrete state.
#[track_caller]
fn assert_state(major: StatusMajor, minor: StatusMinor, expected: ConnectionState) {
    match map_status(major, minor) {
        StateTransition::Set(actual) => assert_eq!(
            actual, expected,
            "{major:?}/{minor:?} should map to {expected:?}, got {actual:?}"
        ),
        StateTransition::NoChange { reason } => {
            panic!("{major:?}/{minor:?} should map to {expected:?}, got NoChange({reason})")
        }
    }
}

// ---------------------------------------------------------------- Disconnected

#[test]
fn disconnected_states() {
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnDisconnected,
        ConnectionState::Disconnected,
    );
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnDone,
        ConnectionState::Disconnected,
    );
    assert_state(
        StatusMajor::Session,
        StatusMinor::SessRemoved,
        ConnectionState::Disconnected,
    );
}

// ----------------------------------------------------------------- Connecting

#[test]
fn connecting_states() {
    assert_state(
        StatusMajor::Config,
        StatusMinor::CfgOk,
        ConnectionState::Connecting,
    );
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnInit,
        ConnectionState::Connecting,
    );
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnConnecting,
        ConnectionState::Connecting,
    );
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnReconnecting,
        ConnectionState::Connecting,
    );
}

/// The tunnel is still up while tearing down. Reporting `Disconnected` here
/// would tell the user they are off the VPN while traffic still routes over it.
#[test]
fn disconnecting_reads_as_connecting_not_disconnected() {
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnDisconnecting,
        ConnectionState::Connecting,
    );
}

// ------------------------------------------------------------------ Connected

#[test]
fn connected_state() {
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnConnected,
        ConnectionState::Connected,
    );
}

// ---------------------------------------------------------------- AuthPending

#[test]
fn auth_pending_states() {
    assert_state(
        StatusMajor::Config,
        StatusMinor::CfgRequireUser,
        ConnectionState::AuthPending,
    );
    assert_state(
        StatusMajor::Session,
        StatusMinor::SessAuthUserpass,
        ConnectionState::AuthPending,
    );
    assert_state(
        StatusMajor::Session,
        StatusMinor::SessAuthChallenge,
        ConnectionState::AuthPending,
    );
    // Reported as auth-pending for the icon even though the prompt cannot
    // service it — KTD8 handles the follow-up on the AttentionRequired side.
    assert_state(
        StatusMajor::Session,
        StatusMinor::SessAuthUrl,
        ConnectionState::AuthPending,
    );
}

// --------------------------------------------------------------------- Failed

#[test]
fn failed_states() {
    assert_state(
        StatusMajor::Config,
        StatusMinor::CfgError,
        ConnectionState::Failed,
    );
    assert_state(
        StatusMajor::Config,
        StatusMinor::CfgInlineMissing,
        ConnectionState::Failed,
    );
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnFailed,
        ConnectionState::Failed,
    );
    assert_state(
        StatusMajor::Connection,
        StatusMinor::ConnAuthFailed,
        ConnectionState::Failed,
    );
    assert_state(
        StatusMajor::Process,
        StatusMinor::ProcKilled,
        ConnectionState::Failed,
    );
}

// ------------------------------------------------------------------ Fallthrough

/// An unmapped pair must not collapse to a default. Silently choosing
/// `Disconnected` for an unknown status is how a display becomes confidently
/// wrong; the reason string is what makes the gap visible in the log buffer.
#[test]
fn unmapped_pair_yields_no_change_with_a_reason() {
    match map_status(StatusMajor::Pkcs11, StatusMinor::Pkcs11Sign) {
        StateTransition::NoChange { reason } => {
            assert!(
                reason.contains("PKCS11") || reason.contains("Pkcs11"),
                "reason should name the unmapped status, got: {reason}"
            );
        }
        StateTransition::Set(s) => panic!("expected NoChange for an unmapped pair, got {s:?}"),
    }
}

/// The applet never calls Pause, so these only arrive from an external actor.
/// Treat as connecting (the tunnel is in flux) and record that it happened.
#[test]
fn paused_states_read_as_connecting_and_are_recorded() {
    for minor in [
        StatusMinor::ConnPausing,
        StatusMinor::ConnPaused,
        StatusMinor::ConnResuming,
    ] {
        assert_state(
            StatusMajor::Connection,
            minor,
            ConnectionState::Connecting,
        );
    }
}

// ------------------------------------------------------- Numeric wire decoding

/// D-Bus delivers these as raw integers. The discriminants come straight from
/// openvpn3-linux `src/dbus/constants.hpp`; if they drift, every mapping above
/// is decoding the wrong thing.
#[test]
fn wire_discriminants_match_openvpn3_constants() {
    assert_eq!(StatusMajor::try_from(2u8).unwrap(), StatusMajor::Connection);
    assert_eq!(StatusMajor::try_from(3u8).unwrap(), StatusMajor::Session);
    assert_eq!(
        StatusMinor::try_from(7u16).unwrap(),
        StatusMinor::ConnConnected
    );
    assert_eq!(
        StatusMinor::try_from(11u16).unwrap(),
        StatusMinor::ConnAuthFailed
    );
    assert_eq!(
        StatusMinor::try_from(20u16).unwrap(),
        StatusMinor::SessAuthUserpass
    );
}

/// An unknown numeric value must be an error the caller can log, not a panic
/// and not a silent coercion to a neighbouring variant.
#[test]
fn unknown_wire_values_are_rejected_not_coerced() {
    assert!(StatusMajor::try_from(99u8).is_err());
    assert!(StatusMinor::try_from(999u16).is_err());
}
