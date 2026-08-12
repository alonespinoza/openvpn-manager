//! Connection state, and the mapping from openvpn3's status codes onto it.
//!
//! The discriminants below are transcribed from openvpn3-linux
//! `src/dbus/constants.hpp`. They travel the bus as raw integers, so they are
//! the contract — not the variant names.

use crate::wire::{UnknownWireValue, wire_enum};

wire_enum! {
    /// `StatusMajor` — the broad category of a status event.
    pub enum StatusMajor: u8 {
        Unset = 0,
        Config = 1,
        Connection = 2,
        Session = 3,
        Pkcs11 = 4,
        Process = 5,
    }
}

wire_enum! {
    /// `StatusMinor` — the specific status within a category.
    pub enum StatusMinor: u16 {
        Unset = 0,
        CfgError = 1,
        CfgOk = 2,
        CfgInlineMissing = 3,
        CfgRequireUser = 4,
        ConnInit = 5,
        ConnConnecting = 6,
        ConnConnected = 7,
        ConnDisconnecting = 8,
        ConnDisconnected = 9,
        ConnFailed = 10,
        ConnAuthFailed = 11,
        ConnReconnecting = 12,
        ConnPausing = 13,
        ConnPaused = 14,
        ConnResuming = 15,
        ConnDone = 16,
        SessNew = 17,
        SessBackendCompleted = 18,
        SessRemoved = 19,
        SessAuthUserpass = 20,
        SessAuthChallenge = 21,
        SessAuthUrl = 22,
        Pkcs11Sign = 23,
        Pkcs11Encrypt = 24,
        Pkcs11Decrypt = 25,
        Pkcs11Verify = 26,
        ProcStarted = 27,
        ProcStopped = 28,
        ProcKilled = 29,
    }
}

/// What the panel icon claims. Five states, not four: a connect stalled waiting
/// on a one-time code is not the same thing as a slow one, and if the prompt is
/// behind another window the panel is the only surface that can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    AuthPending,
    Failed,
}

impl ConnectionState {
    /// Icon name for this state. Lives here rather than in the applet crate so
    /// the distinctness guarantee is testable without Wayland.
    pub fn icon_name(self) -> &'static str {
        match self {
            Self::Disconnected => "network-vpn-disconnected-symbolic",
            Self::Connecting => "network-vpn-acquiring-symbolic",
            Self::Connected => "network-vpn-symbolic",
            Self::AuthPending => "network-vpn-need-auth-symbolic",
            Self::Failed => "network-vpn-error-symbolic",
        }
    }

    /// Every state, for exhaustive iteration in tests and menu rendering.
    pub const ALL: [Self; 5] = [
        Self::Disconnected,
        Self::Connecting,
        Self::Connected,
        Self::AuthPending,
        Self::Failed,
    ];
}

/// The result of interpreting a status event.
///
/// `NoChange` exists so an unrecognised status leaves the display alone instead
/// of collapsing to a default. A status display that lags is bad; one that
/// confidently reports the wrong thing is worse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateTransition {
    Set(ConnectionState),
    NoChange { reason: String },
}

/// The KTD3 mapping table.
pub fn map_status(major: StatusMajor, minor: StatusMinor) -> StateTransition {
    use ConnectionState as State;
    use StatusMajor as Maj;
    use StatusMinor as Min;

    let state = match (major, minor) {
        (Maj::Connection, Min::ConnDisconnected | Min::ConnDone) => State::Disconnected,
        (Maj::Session, Min::SessRemoved) => State::Disconnected,

        (Maj::Config, Min::CfgOk) => State::Connecting,
        (
            Maj::Connection,
            Min::ConnInit | Min::ConnConnecting | Min::ConnReconnecting | Min::ConnDisconnecting,
        ) => State::Connecting,
        // Unreachable in normal operation — the applet never calls Pause. If an
        // external actor pauses a session the tunnel is in flux, which reads as
        // connecting rather than as any settled state.
        (Maj::Connection, Min::ConnPausing | Min::ConnPaused | Min::ConnResuming) => {
            State::Connecting
        }

        (Maj::Connection, Min::ConnConnected) => State::Connected,

        (Maj::Config, Min::CfgRequireUser) => State::AuthPending,
        // SessAuthUrl reads as auth-pending for the icon even though the prompt
        // cannot service browser auth; KTD8 decides what happens next on the
        // AttentionRequired side.
        (
            Maj::Session,
            Min::SessAuthUserpass | Min::SessAuthChallenge | Min::SessAuthUrl,
        ) => State::AuthPending,

        (Maj::Config, Min::CfgError | Min::CfgInlineMissing) => State::Failed,
        (Maj::Connection, Min::ConnFailed | Min::ConnAuthFailed) => State::Failed,
        (Maj::Process, Min::ProcKilled) => State::Failed,

        _ => {
            return StateTransition::NoChange {
                reason: format!("unmapped openvpn3 status: {major:?}/{minor:?}"),
            };
        }
    };

    StateTransition::Set(state)
}

/// A `StatusChange` signal payload, or the `status` property read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub major: StatusMajor,
    pub minor: StatusMinor,
    pub message: String,
}

impl Status {
    /// Decode the `(uint, uint, string)` tuple openvpn3 puts on the bus.
    pub fn from_wire(
        major: u8,
        minor: u16,
        message: impl Into<String>,
    ) -> Result<Self, UnknownWireValue> {
        Ok(Self {
            major: StatusMajor::try_from(major)?,
            minor: StatusMinor::try_from(minor)?,
            message: message.into(),
        })
    }

    pub fn transition(&self) -> StateTransition {
        map_status(self.major, self.minor)
    }
}

impl Status {
    /// The state this status would produce, or the caller's current one when it
    /// maps to no change. Convenience for comparing "what we show" against
    /// "what openvpn3 says".
    pub fn transition_state(&self) -> ConnectionState {
        match self.transition() {
            StateTransition::Set(state) => state,
            StateTransition::NoChange { .. } => ConnectionState::default(),
        }
    }
}
