//! The connection transition machine (KTD5).
//!
//! Pure: events in, commands out, no I/O. The applet drives it from D-Bus
//! signals and executes the commands it returns. Keeping it free of zbus is
//! what lets the single-session invariant — the property AE1 turns on — be
//! tested directly rather than inferred from an integration run.

use crate::attention::{
    AttentionRouting, ClientAttentionGroup, ClientAttentionType, PromptKind, route_attention,
};
use crate::status::{ConnectionState, StateTransition, Status};

/// Something that happened. Mostly D-Bus signals; a few are user actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// User picked a profile from the menu.
    ProfileSelected { config_path: String },
    /// User picked disconnect.
    DisconnectRequested,
    /// `NewTunnel` returned a session object.
    SessionCreated {
        session_path: String,
        config_path: String,
    },
    /// `Ready` succeeded — the backend has everything it needs to connect.
    SessionReady { session_path: String },
    /// `Ready` reported missing credentials, and the input queue says what for.
    /// This is how a username/password profile asks: up front, not as an
    /// `AttentionRequired` signal mid-handshake.
    ///
    /// `requests` is every queued `(type, group)`, not just the first. A profile
    /// with a static challenge queues two — credentials and the code — and
    /// answering only one leaves the connect stalled on the other.
    CredentialsRequired {
        session_path: String,
        requests: Vec<(ClientAttentionType, ClientAttentionGroup)>,
        message: String,
    },
    /// A session the applet did not start, seen at startup or via
    /// `SessionManagerEvent` (AE4).
    ExternalSessionSeen {
        session_path: String,
        config_path: String,
        status: Status,
    },
    StatusChanged {
        session_path: String,
        status: Status,
    },
    SessionRemoved {
        session_path: String,
    },
    AttentionRequired {
        session_path: String,
        r#type: ClientAttentionType,
        group: ClientAttentionGroup,
        message: String,
    },
    /// User submitted the credential prompt.
    PromptSubmitted {
        session_path: String,
        responses: Vec<InputResponse>,
    },
    /// User cancelled the prompt, or closed its window.
    PromptCancelled { session_path: String },
    /// User dismissed a failure.
    FailureDismissed,
}

/// One answered input-queue item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputResponse {
    pub r#type: ClientAttentionType,
    pub group: ClientAttentionGroup,
    pub id: u32,
    pub value: String,
}

/// Something the applet should do. The order within a returned batch is
/// significant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    NewTunnel {
        config_path: String,
    },
    /// Must be issued before `Connect` on the same session (KTD4).
    EnableLogForward {
        session_path: String,
    },
    Connect {
        session_path: String,
    },
    Disconnect {
        session_path: String,
    },
    /// Drain the listed queue groups and open one prompt covering all of them.
    OpenPrompt {
        session_path: String,
        kind: PromptKind,
        requests: Vec<(ClientAttentionType, ClientAttentionGroup)>,
        message: String,
    },
    ClosePrompt,
    ProvideInput {
        session_path: String,
        responses: Vec<InputResponse>,
    },
    /// Call `Ready` and report back — either `SessionReady` or, when it says
    /// credentials are missing, `CredentialsRequired`. Connect must never be
    /// issued before this succeeds, or a profile needing a password silently
    /// stalls with nothing asked of the user.
    CheckReady {
        session_path: String,
    },
    /// Write a line into this session's log buffer.
    Note {
        session_path: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveSession {
    session_path: String,
    config_path: String,
}

/// Tracks one session at a time and serialises every transition between them.
#[derive(Debug, Default)]
pub struct Machine {
    state: ConnectionState,
    active: Option<ActiveSession>,
    /// A profile selected while another session still needs tearing down.
    /// Replaced rather than queued: the user's latest choice is the real one.
    pending_target: Option<String>,
    /// Session we issued `Disconnect` to and are waiting to see go away.
    awaiting_teardown: Option<String>,
    /// True once a prompt is open, so a late duplicate `AttentionRequired`
    /// does not reopen a window the user already dealt with.
    prompt_open: bool,
}

impl Machine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> ConnectionState {
        self.state
    }

    pub fn active_session(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.session_path.as_str())
    }

    /// The session we issued `Disconnect` to and have not yet seen go away.
    ///
    /// Exposed so the caller can reconcile against reality: if openvpn3 no
    /// longer lists this session, the teardown completed and we simply missed
    /// the signal. Without that check a single dropped event leaves the machine
    /// waiting forever, and every later profile selection silently does nothing.
    pub fn awaiting_teardown(&self) -> Option<&str> {
        self.awaiting_teardown.as_deref()
    }

    pub fn active_config(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.config_path.as_str())
    }

    /// Is this config the one currently connected (or connecting)?
    pub fn is_active_config(&self, config_path: &str) -> bool {
        self.active_config() == Some(config_path)
    }

    pub fn handle(&mut self, event: Event) -> Vec<Command> {
        match event {
            Event::ProfileSelected { config_path } => self.on_profile_selected(config_path),
            Event::DisconnectRequested => self.on_disconnect_requested(),
            Event::SessionCreated {
                session_path,
                config_path,
            } => self.on_session_created(session_path, config_path),
            Event::SessionReady { session_path } => self.on_session_ready(session_path),
            Event::CredentialsRequired {
                session_path,
                requests,
                message,
            } => self.on_credentials_required(session_path, requests, message),
            Event::ExternalSessionSeen {
                session_path,
                config_path,
                status,
            } => self.on_external_session(session_path, config_path, status),
            Event::StatusChanged {
                session_path,
                status,
            } => self.on_status(session_path, status),
            Event::SessionRemoved { session_path } => self.on_session_removed(session_path),
            Event::AttentionRequired {
                session_path,
                r#type,
                group,
                message,
            } => self.on_credentials_required(session_path, vec![(r#type, group)], message),
            Event::PromptSubmitted {
                session_path,
                responses,
            } => self.on_prompt_submitted(session_path, responses),
            Event::PromptCancelled { session_path } => self.on_prompt_cancelled(session_path),
            Event::FailureDismissed => self.on_failure_dismissed(),
        }
    }

    // ------------------------------------------------------------- selection

    fn on_profile_selected(&mut self, config_path: String) -> Vec<Command> {
        // Already tearing something down: just retarget. Starting a second
        // chain here is what would let two sessions exist at once.
        if self.awaiting_teardown.is_some() {
            self.pending_target = Some(config_path);
            return vec![];
        }

        match self.active.take() {
            Some(active) => {
                // R6/AE1: the running session goes away *before* the new one
                // starts. NewTunnel is deferred until we see it gone.
                self.pending_target = Some(config_path);
                self.awaiting_teardown = Some(active.session_path.clone());
                self.state = ConnectionState::Connecting;
                vec![Command::Disconnect {
                    session_path: active.session_path,
                }]
            }
            None => {
                self.state = ConnectionState::Connecting;
                vec![Command::NewTunnel { config_path }]
            }
        }
    }

    fn on_disconnect_requested(&mut self) -> Vec<Command> {
        self.pending_target = None;

        match self.active.take() {
            Some(active) => {
                self.awaiting_teardown = Some(active.session_path.clone());
                vec![Command::Disconnect {
                    session_path: active.session_path,
                }]
            }
            None => vec![],
        }
    }

    fn on_session_created(&mut self, session_path: String, config_path: String) -> Vec<Command> {
        self.pending_target = None;
        self.active = Some(ActiveSession {
            session_path: session_path.clone(),
            config_path,
        });
        self.state = ConnectionState::Connecting;

        // Ordering is load-bearing twice over. Log forwarding has to be on
        // before the handshake starts (KTD4) or a failed connect has no log to
        // show; and readiness has to be confirmed before Connect, because that
        // is how openvpn3 reports that a profile needs a password.
        vec![
            Command::EnableLogForward {
                session_path: session_path.clone(),
            },
            Command::CheckReady { session_path },
        ]
    }

    fn on_session_ready(&mut self, session_path: String) -> Vec<Command> {
        if self.active_session() != Some(session_path.as_str()) {
            return vec![];
        }
        self.state = ConnectionState::Connecting;
        vec![Command::Connect { session_path }]
    }

    fn on_external_session(
        &mut self,
        session_path: String,
        config_path: String,
        status: Status,
    ) -> Vec<Command> {
        // Only adopt when we are not mid-transition; otherwise our own chain is
        // the authority and an external report would fight it.
        if self.awaiting_teardown.is_some() || self.active.is_some() {
            return vec![];
        }

        self.active = Some(ActiveSession {
            session_path: session_path.clone(),
            config_path,
        });

        self.apply_status(&session_path, status)
    }

    // ---------------------------------------------------------------- status

    fn on_status(&mut self, session_path: String, status: Status) -> Vec<Command> {
        // A status for a session we are tearing down tells us how far along it
        // is; anything else about an untracked session is not ours to render.
        let is_active = self.active_session() == Some(session_path.as_str());
        let is_tearing_down = self.awaiting_teardown.as_deref() == Some(session_path.as_str());

        if !is_active && !is_tearing_down {
            return vec![];
        }

        if is_tearing_down {
            return match status.transition() {
                StateTransition::Set(ConnectionState::Disconnected) => {
                    self.finish_teardown(&session_path)
                }
                _ => vec![],
            };
        }

        self.apply_status(&session_path, status)
    }

    fn apply_status(&mut self, session_path: &str, status: Status) -> Vec<Command> {
        match status.transition() {
            StateTransition::Set(new_state) => {
                self.state = new_state;

                // Leaving auth-pending means the prompt is no longer wanted.
                if self.prompt_open && new_state != ConnectionState::AuthPending {
                    self.prompt_open = false;
                    return vec![Command::ClosePrompt];
                }
                vec![]
            }
            // KTD3: leave the display alone, but make the gap visible.
            StateTransition::NoChange { reason } => vec![Command::Note {
                session_path: session_path.to_owned(),
                message: reason,
            }],
        }
    }

    fn on_session_removed(&mut self, session_path: String) -> Vec<Command> {
        if self.awaiting_teardown.as_deref() == Some(session_path.as_str()) {
            return self.finish_teardown(&session_path);
        }

        if self.active_session() == Some(session_path.as_str()) {
            self.active = None;
            self.prompt_open = false;
            // A session vanishing under a connected/connecting display means it
            // is gone, however it went.
            if self.state != ConnectionState::Failed {
                self.state = ConnectionState::Disconnected;
            }
        }

        vec![]
    }

    fn finish_teardown(&mut self, session_path: &str) -> Vec<Command> {
        if self.awaiting_teardown.as_deref() != Some(session_path) {
            return vec![];
        }

        self.awaiting_teardown = None;
        self.prompt_open = false;

        match self.pending_target.take() {
            Some(config_path) => {
                self.state = ConnectionState::Connecting;
                vec![Command::NewTunnel { config_path }]
            }
            None => {
                if self.state != ConnectionState::Failed {
                    self.state = ConnectionState::Disconnected;
                }
                vec![]
            }
        }
    }

    // ------------------------------------------------------------- attention

    fn on_credentials_required(
        &mut self,
        session_path: String,
        requests: Vec<(ClientAttentionType, ClientAttentionGroup)>,
        message: String,
    ) -> Vec<Command> {
        if self.active_session() != Some(session_path.as_str()) {
            return vec![];
        }

        // The user already dismissed this attempt; a late or duplicate signal
        // must not resurrect the window.
        if self.prompt_open || requests.is_empty() {
            return vec![];
        }

        // KTD8: one unanswerable group fails the whole attempt. Prompting for
        // the rest would collect input the connect can never use.
        for &(r#type, group) in &requests {
            if let AttentionRouting::Unsupported { reason } = route_attention(r#type, group) {
                self.state = ConnectionState::Failed;
                self.active = None;
                self.awaiting_teardown = Some(session_path.clone());
                return vec![
                    Command::Note {
                        session_path: session_path.clone(),
                        message: reason,
                    },
                    Command::Disconnect { session_path },
                ];
            }
        }

        // The heading comes from the first group; the form spans them all.
        let kind = match route_attention(requests[0].0, requests[0].1) {
            AttentionRouting::Prompt(kind) => kind,
            AttentionRouting::Unsupported { .. } => unreachable!("checked above"),
        };

        self.state = ConnectionState::AuthPending;
        self.prompt_open = true;
        vec![Command::OpenPrompt {
            session_path,
            kind,
            requests,
            message,
        }]
    }

    fn on_prompt_submitted(
        &mut self,
        session_path: String,
        responses: Vec<InputResponse>,
    ) -> Vec<Command> {
        if self.active_session() != Some(session_path.as_str()) {
            return vec![];
        }

        self.prompt_open = false;
        self.state = ConnectionState::Connecting;

        // Re-check rather than connecting blind: openvpn3 may have more than one
        // thing to ask for, and the reference client loops on Ready for exactly
        // that reason.
        // ClosePrompt precedes the re-check deliberately. CheckReady can open a
        // second prompt — openvpn3 often asks for a username and a password in
        // separate rounds — and a trailing close would wipe out the prompt that
        // had just been opened.
        vec![
            Command::ProvideInput {
                session_path: session_path.clone(),
                responses,
            },
            Command::ClosePrompt,
            Command::CheckReady { session_path },
        ]
    }

    /// AE2 — cancelling ends the attempt outright. Leaving the session running
    /// would strand it in auth-pending with no window to return to.
    fn on_prompt_cancelled(&mut self, session_path: String) -> Vec<Command> {
        if self.active_session() != Some(session_path.as_str()) {
            return vec![];
        }

        self.prompt_open = false;
        self.state = ConnectionState::Failed;
        self.active = None;
        self.pending_target = None;
        self.awaiting_teardown = Some(session_path.clone());

        vec![
            Command::ClosePrompt,
            Command::Disconnect { session_path },
        ]
    }

    fn on_failure_dismissed(&mut self) -> Vec<Command> {
        if self.state == ConnectionState::Failed {
            self.state = ConnectionState::Disconnected;
        }
        vec![]
    }
}
