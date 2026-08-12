//! The async worker that owns the D-Bus connection.
//!
//! It drives the (already tested) `Machine` and `LogStore` from `openvpn3-dbus`
//! and publishes a whole `Snapshot` whenever anything changes. Publishing a
//! snapshot rather than fine-grained deltas keeps the UI a pure function of
//! state — there is no way for the panel to drift out of sync with the worker.

use std::collections::HashMap;
use std::path::PathBuf;

use futures_util::StreamExt;
use openvpn3_dbus::attention::{ClientAttentionGroup, ClientAttentionType, PromptField, PromptKind};
use openvpn3_dbus::event::SessionEventType;
use openvpn3_dbus::logbuf::{LogEntry, LogStore, SessionLog};
use openvpn3_dbus::machine::{Command, Event, InputResponse, Machine};
use openvpn3_dbus::profile::display_name;
use openvpn3_dbus::proxy::{
    ConfigurationManagerProxy, ConfigurationProxy, SessionManagerProxy, SessionProxy,
};
use openvpn3_dbus::status::{ConnectionState, Status};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use zbus::zvariant::OwnedObjectPath;

/// How many log lines to keep per session. A failed connect produces on the
/// order of a hundred; this leaves generous headroom without unbounded growth.
const LOG_CAPACITY: usize = 1000;

/// A profile as the menu needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub config_path: String,
    pub name: String,
}

/// What the credential prompt should render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSpec {
    pub session_path: String,
    pub kind: PromptKind,
    pub r#type: ClientAttentionType,
    pub group: ClientAttentionGroup,
    pub message: String,
    pub fields: Vec<PromptField>,
}

/// Everything the UI renders. Replaced wholesale on every change.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub state: ConnectionState,
    pub profiles: Vec<Profile>,
    pub active_config: Option<String>,
    pub active_name: Option<String>,
    /// Unix epoch seconds, so uptime stays correct for adopted sessions.
    pub session_created: Option<u64>,
    pub status_message: String,
    pub prompt: Option<PromptSpec>,
    pub log: Option<Vec<String>>,
    /// Set when openvpn3 itself is unreachable, so the menu can say so instead
    /// of rendering an empty and unexplained profile list.
    pub unavailable: Option<String>,
}

/// Requests from the UI.
#[derive(Debug, Clone)]
pub enum UiCommand {
    SelectProfile(String),
    Disconnect,
    SubmitPrompt(Vec<InputResponse>),
    CancelPrompt,
    Import(PathBuf),
    RenameProfile { config_path: String, new_name: String },
    DeleteProfile(String),
    DismissFailure,
    RefreshProfiles,
}

/// Signals folded into one stream so the worker loop stays flat.
#[derive(Debug)]
enum Wire {
    Status { session: String, status: Status },
    Attention { session: String, r#type: u32, group: u32, message: String },
    Log { session: String, entry: LogEntry },
    SessionManager { path: String, removed: bool },
}

pub async fn run(mut commands: mpsc::Receiver<UiCommand>, snapshots: mpsc::Sender<Snapshot>) {
    // The system bus, not the session bus. openvpn3's services default to
    // SYSTEM (session is a debug-only `--use-session-bus` flag), and access for
    // unprivileged users comes from its D-Bus policy in
    // /etc/dbus-1/system.d/net.openvpn.v3.conf — which is why R15 holds without
    // sudo or a root helper.
    let connection = match zbus::Connection::system().await {
        Ok(connection) => connection,
        Err(error) => {
            let _ = snapshots
                .send(Snapshot {
                    unavailable: Some(format!("Cannot reach the D-Bus system bus: {error}")),
                    ..Default::default()
                })
                .await;
            return;
        }
    };

    let mut worker = Worker {
        connection,
        machine: Machine::new(),
        logs: LogStore::new(LOG_CAPACITY),
        profiles: Vec::new(),
        profile_names: HashMap::new(),
        prompt: None,
        status_message: String::new(),
        unavailable: None,
        forwarders: Vec::new(),
        session_created: None,
    };

    let (wire_tx, mut wire_rx) = mpsc::channel(256);

    // Signals stay the primary path, but a connect that reaches CONN_CONNECTED
    // while a signal goes astray would leave the icon claiming "connecting"
    // indefinitely — the trusted-and-wrong display the whole design rejects.
    // This ticks only while a transition is actually in flight and goes idle the
    // moment it settles, so it is a bounded safety net rather than polling.
    let mut settle = tokio::time::interval(std::time::Duration::from_millis(400));
    settle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    worker.refresh_profiles().await;
    worker.adopt_existing_sessions(&wire_tx).await;
    worker.watch_session_manager(&wire_tx).await;
    worker.publish(&snapshots).await;

    loop {
        tokio::select! {
            Some(command) = commands.recv() => {
                worker.on_ui_command(command, &wire_tx).await;
                worker.publish(&snapshots).await;
            }
            Some(wire) = wire_rx.recv() => {
                worker.on_wire(wire, &wire_tx).await;
                worker.publish(&snapshots).await;
            }
            _ = settle.tick(), if worker.transition_in_flight() => {
                let before = worker.machine.state();
                worker.reconcile_teardown(&wire_tx).await;
                worker.reconcile_active_status(&wire_tx).await;
                if worker.machine.state() != before {
                    tracing::debug!(?before, after = ?worker.machine.state(),
                        "settled a transition the signal did not report");
                    worker.publish(&snapshots).await;
                }
            }
            else => break,
        }
    }
}

struct Worker {
    connection: zbus::Connection,
    machine: Machine,
    logs: LogStore,
    profiles: Vec<Profile>,
    profile_names: HashMap<String, String>,
    prompt: Option<PromptSpec>,
    status_message: String,
    unavailable: Option<String>,
    forwarders: Vec<AbortHandle>,
    session_created: Option<u64>,
}

impl Worker {
    /// Is the applet mid-transition, i.e. is the display expected to change on
    /// its own shortly? Settled states need no watching.
    fn transition_in_flight(&self) -> bool {
        self.machine.awaiting_teardown().is_some()
            || matches!(self.machine.state(), ConnectionState::Connecting)
    }

    async fn publish(&self, snapshots: &mpsc::Sender<Snapshot>) {
        let log = match self.logs.most_recent() {
            SessionLog::Captured { entries, .. } => {
                Some(entries.iter().map(|e| e.message.clone()).collect())
            }
            SessionLog::NotCaptured { .. } | SessionLog::None => None,
        };

        let active_config = self.machine.active_config().map(str::to_owned);
        let active_name = active_config
            .as_ref()
            .and_then(|c| self.profile_names.get(c).cloned());

        let _ = snapshots
            .send(Snapshot {
                state: self.machine.state(),
                profiles: self.profiles.clone(),
                active_config,
                active_name,
                session_created: self.session_created,
                status_message: self.status_message.clone(),
                prompt: self.prompt.clone(),
                log,
                unavailable: self.unavailable.clone(),
            })
            .await;
    }

    // ------------------------------------------------------------- profiles

    async fn refresh_profiles(&mut self) {
        let manager = match ConfigurationManagerProxy::new(&self.connection).await {
            Ok(manager) => manager,
            Err(error) => return self.mark_unavailable(error),
        };

        let paths = match manager.fetch_available_configs().await {
            Ok(paths) => paths,
            Err(error) => return self.mark_unavailable(error),
        };

        self.unavailable = None;
        self.profiles.clear();
        self.profile_names.clear();

        for path in paths {
            // Profiles are keyed by object path, never by name — openvpn3
            // permits duplicate names and the menu has to stay unambiguous.
            let key = path.to_string();
            let raw_name = self.config_name(&path).await;
            let name = display_name(&raw_name, &key);

            self.profile_names.insert(key.clone(), name.clone());
            self.profiles.push(Profile {
                config_path: key,
                name,
            });
        }

        self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Empty string on failure — `display_name` turns that into a usable label.
    /// The warning matters: a silent fallback here is what put raw D-Bus object
    /// paths in the menu, and nothing said why.
    async fn config_name(&self, path: &OwnedObjectPath) -> String {
        let builder = match ConfigurationProxy::builder(&self.connection).path(path.clone()) {
            Ok(builder) => builder,
            Err(error) => {
                tracing::warn!(%path, %error, "could not address config object");
                return String::new();
            }
        };

        match builder.build().await {
            Ok(proxy) => match proxy.name().await {
                Ok(name) => name,
                Err(error) => {
                    tracing::warn!(%path, %error, "could not read the profile name");
                    String::new()
                }
            },
            Err(error) => {
                tracing::warn!(%path, %error, "could not build a config proxy");
                String::new()
            }
        }
    }

    fn mark_unavailable(&mut self, error: zbus::Error) {
        // "Not installed" and "installed but not answering" need different
        // fixes, and the applet is the only thing in a position to tell them
        // apart. Saying "openvpn3 is unavailable" for both leaves the user to
        // guess which problem they have.
        self.unavailable = Some(if openvpn3_on_path() {
            format!(
                "openvpn3 is installed but its services are not answering on the \
                 D-Bus system bus ({error}). Try `openvpn3 configs-list` in a \
                 terminal — if that fails too, the openvpn3 services are not running."
            )
        } else {
            "openvpn3-linux is not installed.\n\n\
             Add OpenVPN's apt repository, then:\n\
             sudo apt install openvpn3"
                .to_owned()
        });
        self.profiles.clear();
    }

    // ------------------------------------------------------------- sessions

    async fn adopt_existing_sessions(&mut self, wire: &mpsc::Sender<Wire>) {
        let Ok(manager) = SessionManagerProxy::new(&self.connection).await else {
            return;
        };
        let Ok(paths) = manager.fetch_available_sessions().await else {
            return;
        };

        // AE4: a session started from a terminal is the truth, and the icon
        // should say so the moment the applet starts rather than after the next
        // signal happens to arrive.
        for path in paths {
            let Ok(session) = self.session_proxy(&path).await else {
                continue;
            };
            let Ok((major, minor, message)) = session.status().await else {
                continue;
            };
            let Ok(status) = Status::from_wire(major as u8, minor as u16, message) else {
                continue;
            };

            let config_path = session
                .config_path()
                .await
                .map(|p| p.to_string())
                .unwrap_or_default();
            self.session_created = session.session_created().await.ok();

            let session_path = path.to_string();
            self.logs.adopt_external_session(&session_path);
            let commands = self.machine.handle(Event::ExternalSessionSeen {
                session_path: session_path.clone(),
                config_path,
                status,
            });
            self.execute(commands, wire).await;
            self.subscribe_session(&path, wire).await;
            break; // single-session model
        }
    }

    async fn watch_session_manager(&mut self, wire: &mpsc::Sender<Wire>) {
        let Ok(manager) = SessionManagerProxy::new(&self.connection).await else {
            return;
        };
        let Ok(mut stream) = manager.receive_session_manager_event().await else {
            return;
        };

        let wire = wire.clone();
        let handle = tokio::spawn(async move {
            while let Some(signal) = stream.next().await {
                let Ok(args) = signal.args() else { continue };
                let removed = SessionEventType::try_from(args.event_type)
                    .map(SessionEventType::is_destroyed)
                    .unwrap_or(false);
                let _ = wire
                    .send(Wire::SessionManager {
                        path: args.path.to_string(),
                        removed,
                    })
                    .await;
            }
        });
        self.forwarders.push(handle.abort_handle());
    }

    /// `'static` matters: the returned proxy outlives the borrow of `self`,
    /// which is what lets a caller log into `self.logs` while holding it, and
    /// lets each signal task own its own clone.
    async fn session_proxy(&self, path: &OwnedObjectPath) -> zbus::Result<SessionProxy<'static>> {
        SessionProxy::builder(&self.connection)
            .path(path.clone())?
            .build()
            .await
    }

    /// Forward one session's three signal streams into the shared wire channel.
    ///
    /// Returns only once every subscription is actually registered on the bus.
    /// This matters: `Connect` is issued straight after, and openvpn3 can reach
    /// CONN_CONNECTED before a match rule that was still being set up. The icon
    /// then waits forever for a signal that already came and went.
    async fn subscribe_session(&mut self, path: &OwnedObjectPath, wire: &mpsc::Sender<Wire>) {
        let mut ready_signals = Vec::new();
        let Ok(session) = self.session_proxy(path).await else {
            return;
        };
        let key = path.to_string();

        {
            let (wire, key, session) = (wire.clone(), key.clone(), session.clone());
            let (ready_tx, ready_rx) = oneshot::channel();
            ready_signals.push(ready_rx);
            let handle = tokio::spawn(async move {
                let stream = session.receive_status_change().await;
                let _ = ready_tx.send(());
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(session = %key, %error, "no StatusChange subscription");
                        return;
                    }
                };
                tracing::debug!(session = %key, "subscribed to StatusChange");
                while let Some(signal) = stream.next().await {
                    let Ok(args) = signal.args() else { continue };
                    let status = match Status::from_wire(
                        args.major as u8,
                        args.minor as u16,
                        args.message.clone(),
                    ) {
                        Ok(status) => status,
                        Err(error) => {
                            tracing::warn!(
                                major = args.major, minor = args.minor, %error,
                                "undecodable StatusChange"
                            );
                            continue;
                        }
                    };
                    let _ = wire
                        .send(Wire::Status {
                            session: key.clone(),
                            status,
                        })
                        .await;
                }
            });
            self.forwarders.push(handle.abort_handle());
        }

        {
            let (wire, key, session) = (wire.clone(), key.clone(), session.clone());
            let (ready_tx, ready_rx) = oneshot::channel();
            ready_signals.push(ready_rx);
            let handle = tokio::spawn(async move {
                let stream = session.receive_attention_required().await;
                let _ = ready_tx.send(());
                let Ok(mut stream) = stream else {
                    tracing::warn!(session = %key, "no AttentionRequired subscription");
                    return;
                };
                while let Some(signal) = stream.next().await {
                    let Ok(args) = signal.args() else { continue };
                    let _ = wire
                        .send(Wire::Attention {
                            session: key.clone(),
                            r#type: args.r#type,
                            group: args.group,
                            message: args.message.clone(),
                        })
                        .await;
                }
            });
            self.forwarders.push(handle.abort_handle());
        }

        {
            let (wire, key) = (wire.clone(), key.clone());
            let (ready_tx, ready_rx) = oneshot::channel();
            ready_signals.push(ready_rx);
            let handle = tokio::spawn(async move {
                let stream = session.receive_log().await;
                let _ = ready_tx.send(());
                let Ok(mut stream) = stream else {
                    tracing::warn!(session = %key, "no Log subscription");
                    return;
                };
                while let Some(signal) = stream.next().await {
                    let Ok(args) = signal.args() else { continue };
                    let _ = wire
                        .send(Wire::Log {
                            session: key.clone(),
                            entry: LogEntry {
                                group: args.group,
                                level: args.level,
                                message: args.message.clone(),
                            },
                        })
                        .await;
                }
            });
            self.forwarders.push(handle.abort_handle());
        }

        for ready in ready_signals {
            let _ = ready.await;
        }
        tracing::debug!(session = %key, "all signal subscriptions established");
    }

    // --------------------------------------------------------------- events

    /// Resolve a teardown we are still waiting on against what openvpn3
    /// actually reports. Signals can be missed — a dropped one used to leave the
    /// machine waiting forever, with every later click doing nothing — so before
    /// acting on a user request, confirm the session really is still there.
    /// Re-read the active session's real status.
    ///
    /// Status is pushed, not polled — but a signal that never arrives leaves the
    /// icon asserting something false, and a display that is trusted and wrong is
    /// the failure this whole design is meant to avoid. Checking at the moment
    /// the user opens the menu is not a timer: it is making sure that what they
    /// are about to look at is true.
    async fn reconcile_active_status(&mut self, wire: &mpsc::Sender<Wire>) {
        let Some(session_path) = self.machine.active_session().map(str::to_owned) else {
            return;
        };
        let Some(session) = self.proxy_for(&session_path).await else {
            return;
        };

        let Ok((major, minor, message)) = session.status().await else {
            return;
        };
        let Ok(status) = Status::from_wire(major as u8, minor as u16, message) else {
            return;
        };

        if self.machine.state() != status.transition_state() {
            tracing::debug!(
                %session_path, ?status.major, ?status.minor,
                "reconciling a stale icon against the session's real status"
            );
        }

        self.session_created = session.session_created().await.ok().or(self.session_created);
        let commands = self.machine.handle(Event::StatusChanged {
            session_path,
            status,
        });
        self.execute(commands, wire).await;
    }

    async fn reconcile_teardown(&mut self, wire: &mpsc::Sender<Wire>) {
        let Some(pending) = self.machine.awaiting_teardown().map(str::to_owned) else {
            return;
        };

        let Ok(manager) = SessionManagerProxy::new(&self.connection).await else {
            return;
        };
        let Ok(paths) = manager.fetch_available_sessions().await else {
            return;
        };

        let still_present = paths.iter().any(|p| p.to_string() == pending);
        if !still_present {
            tracing::debug!(session = %pending, "teardown already completed; recovering");
            self.logs.remove_session(&pending);
            let commands = self.machine.handle(Event::SessionRemoved {
                session_path: pending,
            });
            self.execute(commands, wire).await;
        }
    }

    async fn on_ui_command(&mut self, command: UiCommand, wire: &mpsc::Sender<Wire>) {
        if matches!(
            command,
            UiCommand::SelectProfile(_) | UiCommand::Disconnect | UiCommand::RefreshProfiles
        ) {
            self.reconcile_teardown(wire).await;
            self.reconcile_active_status(wire).await;
        }

        let event = match command {
            UiCommand::SelectProfile(config_path) => Some(Event::ProfileSelected { config_path }),
            UiCommand::Disconnect => Some(Event::DisconnectRequested),
            UiCommand::DismissFailure => Some(Event::FailureDismissed),
            UiCommand::CancelPrompt => self.prompt.as_ref().map(|p| Event::PromptCancelled {
                session_path: p.session_path.clone(),
            }),
            UiCommand::SubmitPrompt(responses) => {
                self.prompt.as_ref().map(|p| Event::PromptSubmitted {
                    session_path: p.session_path.clone(),
                    responses,
                })
            }
            UiCommand::RefreshProfiles => {
                self.refresh_profiles().await;
                None
            }
            UiCommand::Import(path) => {
                self.import(path).await;
                None
            }
            UiCommand::RenameProfile {
                config_path,
                new_name,
            } => {
                self.rename_profile(&config_path, &new_name).await;
                None
            }
            UiCommand::DeleteProfile(config_path) => {
                self.delete_profile(&config_path).await;
                None
            }
        };

        if let Some(event) = event {
            let commands = self.machine.handle(event);
            self.execute(commands, wire).await;
        }
    }

    async fn on_wire(&mut self, wire_event: Wire, wire: &mpsc::Sender<Wire>) {
        let event = match wire_event {
            Wire::Status { session, status } => {
                tracing::debug!(
                    %session, ?status.major, ?status.minor,
                    active = ?self.machine.active_session(),
                    "StatusChange received"
                );
                if self.machine.active_session() != Some(session.as_str())
                    && self.machine.awaiting_teardown() != Some(session.as_str())
                {
                    tracing::warn!(
                        %session, active = ?self.machine.active_session(),
                        "status is for a session we are not tracking; path mismatch?"
                    );
                }
                self.status_message = status.message.clone();
                Some(Event::StatusChanged {
                    session_path: session,
                    status,
                })
            }
            Wire::Attention {
                session,
                r#type,
                group,
                message,
            } => {
                match (
                    ClientAttentionType::try_from(r#type as u8),
                    ClientAttentionGroup::try_from(group as u8),
                ) {
                    (Ok(r#type), Ok(group)) => Some(Event::AttentionRequired {
                        session_path: session,
                        r#type,
                        group,
                        message,
                    }),
                    _ => {
                        // Unknown codes must not be swallowed: an unanswered
                        // request otherwise parks the session silently.
                        self.logs.note(
                            &session,
                            format!("Unrecognised input request from openvpn3: type={type} group={group}"),
                        );
                        None
                    }
                }
            }
            Wire::Log { session, entry } => {
                self.logs.push(&session, entry);
                None
            }
            Wire::SessionManager { path, removed } => {
                if removed {
                    self.logs.remove_session(&path);
                    Some(Event::SessionRemoved { session_path: path })
                } else {
                    None
                }
            }
        };

        if let Some(event) = event {
            let commands = self.machine.handle(event);
            self.execute(commands, wire).await;
        }
    }

    // ------------------------------------------------------------- commands

    async fn execute(&mut self, commands: Vec<Command>, wire: &mpsc::Sender<Wire>) {
        for command in commands {
            match command {
                Command::NewTunnel { config_path } => self.new_tunnel(config_path, wire).await,

                Command::EnableLogForward { session_path } => {
                    if let Some(session) = self.proxy_for(&session_path).await {
                        // KTD4 — before Connect, or the handshake lines are lost.
                        if let Err(error) = session.log_forward(true).await {
                            tracing::warn!(%error, "could not enable log forwarding");
                            self.logs.note(
                                &session_path,
                                format!("Log forwarding unavailable: {error}"),
                            );
                        }
                    }
                }

                Command::Connect { session_path } => {
                    if let Some(session) = self.proxy_for(&session_path).await {
                        if let Err(error) = session.connect().await {
                            self.logs
                                .note(&session_path, format!("Connect failed: {error}"));
                        }
                    }
                }

                Command::Disconnect { session_path } => {
                    if let Some(session) = self.proxy_for(&session_path).await {
                        let _ = session.disconnect().await;
                    }
                }

                Command::OpenPrompt {
                    session_path,
                    kind,
                    r#type,
                    group,
                    message,
                } => {
                    let fields = self.drain_input_queue(&session_path, r#type, group).await;
                    self.prompt = Some(PromptSpec {
                        session_path,
                        kind,
                        r#type,
                        group,
                        message,
                        fields,
                    });
                }

                Command::ClosePrompt => self.prompt = None,

                Command::ProvideInput {
                    session_path,
                    responses,
                } => {
                    let mut failures = Vec::new();

                    if let Some(session) = self.proxy_for(&session_path).await {
                        for response in &responses {
                            if let Err(error) = session
                                .user_input_provide(
                                    u8::from(response.r#type) as u32,
                                    u8::from(response.group) as u32,
                                    response.id,
                                    &response.value,
                                )
                                .await
                            {
                                tracing::warn!(
                                    session = %session_path, id = response.id, %error,
                                    "UserInputProvide rejected"
                                );
                                failures.push(format!("Submitting credentials failed: {error}"));
                            }
                        }
                    }

                    // R9: nothing is kept. Dropped as early as possible rather
                    // than left to fall out of scope with the rest of the arm.
                    drop(responses);

                    for failure in failures {
                        self.logs.note(&session_path, failure);
                    }
                }

                Command::CheckReady { session_path } => {
                    let next = self.check_ready(&session_path).await;
                    if let Some(event) = next {
                        let commands = self.machine.handle(event);
                        Box::pin(self.execute(commands, wire)).await;
                    }
                }

                Command::Note {
                    session_path,
                    message,
                } => self.logs.note(&session_path, message),
            }
        }
    }

    /// Ask the backend whether it can connect, and find out what it wants if not.
    ///
    /// This is the path a username/password profile takes. openvpn3 reports a
    /// missing password by failing `Ready` with "Missing user credentials" and
    /// queueing what it needs — not by emitting `AttentionRequired`, which is
    /// for challenges that arise later in the handshake. The reference client
    /// loops on `Ready` for the same reason: there may be more than one thing to
    /// ask for.
    async fn check_ready(&mut self, session_path: &str) -> Option<Event> {
        let session = self.proxy_for(session_path).await?;

        let ready = session.ready().await;
        tracing::debug!(session = %session_path, ok = ready.is_ok(), ?ready, "Ready check");

        match ready {
            Ok(()) => Some(Event::SessionReady {
                session_path: session_path.to_owned(),
            }),
            Err(error) => {
                // Ask the queue what it wants rather than parsing the error
                // text; the message is for humans and varies by version.
                let Ok(pairs) = session.user_input_queue_get_type_group().await else {
                    self.logs
                        .note(session_path, format!("Backend not ready: {error}"));
                    return None;
                };

                let Some((r#type, group)) = pairs.first().copied() else {
                    // Not ready, and nothing queued to fix it. Report it rather
                    // than waiting on a request that is never coming.
                    self.logs
                        .note(session_path, format!("Backend not ready: {error}"));
                    return None;
                };

                match (
                    ClientAttentionType::try_from(r#type as u8),
                    ClientAttentionGroup::try_from(group as u8),
                ) {
                    (Ok(r#type), Ok(group)) => Some(Event::CredentialsRequired {
                        session_path: session_path.to_owned(),
                        r#type,
                        group,
                        message: String::new(),
                    }),
                    _ => {
                        self.logs.note(
                            session_path,
                            format!("Unrecognised credential request: type={type} group={group}"),
                        );
                        None
                    }
                }
            }
        }
    }

    async fn new_tunnel(&mut self, config_path: String, wire: &mpsc::Sender<Wire>) {
        let Ok(manager) = SessionManagerProxy::new(&self.connection).await else {
            return;
        };
        let Ok(config) = OwnedObjectPath::try_from(config_path.as_str()) else {
            return;
        };

        match manager.new_tunnel(&config).await {
            Ok(session_path) => {
                let key = session_path.to_string();
                self.logs.begin_session(&key);
                self.session_created = Some(now_epoch());
                self.subscribe_session(&session_path, wire).await;

                let commands = self.machine.handle(Event::SessionCreated {
                    session_path: key,
                    config_path,
                });
                Box::pin(self.execute(commands, wire)).await;
            }
            Err(error) => {
                self.logs
                    .note(&config_path, format!("Could not start session: {error}"));
            }
        }
    }

    async fn proxy_for(&self, session_path: &str) -> Option<SessionProxy<'static>> {
        let path = OwnedObjectPath::try_from(session_path).ok()?;
        self.session_proxy(&path).await.ok()
    }

    /// Ask the session what it actually wants, rather than assuming a shape.
    async fn drain_input_queue(
        &self,
        session_path: &str,
        r#type: ClientAttentionType,
        group: ClientAttentionGroup,
    ) -> Vec<PromptField> {
        let Some(session) = self.proxy_for(session_path).await else {
            return Vec::new();
        };

        let (type_wire, group_wire) = (u8::from(r#type) as u32, u8::from(group) as u32);
        let ids = match session.user_input_queue_check(type_wire, group_wire).await {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%session_path, ?r#type, ?group, %error, "input queue check failed");
                return Vec::new();
            }
        };
        tracing::debug!(%session_path, ?r#type, ?group, count = ids.len(), "input queue items");

        let mut fields = Vec::new();
        for id in ids {
            if let Ok((_, _, id, name, description, hidden_input)) = session
                .user_input_queue_fetch(type_wire, group_wire, id)
                .await
            {
                fields.push(
                    openvpn3_dbus::attention::InputRequest {
                        type_: r#type,
                        group,
                        id,
                        name,
                        description,
                        hidden_input,
                    }
                    .to_field(),
                );
            }
        }
        fields
    }

    async fn config_proxy(&self, config_path: &str) -> Option<ConfigurationProxy<'static>> {
        let path = OwnedObjectPath::try_from(config_path).ok()?;
        ConfigurationProxy::builder(&self.connection)
            .path(path)
            .ok()?
            .build()
            .await
            .ok()
    }

    async fn rename_profile(&mut self, config_path: &str, new_name: &str) {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            self.unavailable = Some("A profile name cannot be empty.".into());
            return;
        }

        let Some(proxy) = self.config_proxy(config_path).await else {
            self.unavailable = Some("That profile is no longer available.".into());
            return;
        };

        // openvpn3 exposes rename as a writable `name` property; this is what
        // `openvpn3 config-manage --rename` does underneath.
        match proxy.set_name(new_name).await {
            Ok(()) => {
                self.unavailable = None;
                self.refresh_profiles().await;
            }
            Err(error) => self.unavailable = Some(format!("Could not rename: {error}")),
        }
    }

    async fn delete_profile(&mut self, config_path: &str) {
        // Removing the profile out from under a running session leaves the
        // session orphaned and the menu describing something that no longer
        // exists. Make the user disconnect first rather than doing it for them —
        // this is destructive and silently tearing down their tunnel is worse
        // than refusing.
        if self.machine.is_active_config(config_path) {
            self.unavailable =
                Some("Disconnect this profile before deleting it.".into());
            return;
        }

        let Some(proxy) = self.config_proxy(config_path).await else {
            self.unavailable = Some("That profile is no longer available.".into());
            return;
        };

        match proxy.remove().await {
            Ok(()) => {
                self.unavailable = None;
                self.refresh_profiles().await;
            }
            Err(error) => self.unavailable = Some(format!("Could not delete: {error}")),
        }
    }

    async fn import(&mut self, path: PathBuf) {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "imported".into());

        let blob = match tokio::fs::read_to_string(&path).await {
            Ok(blob) => blob,
            Err(error) => {
                self.unavailable = Some(format!("Could not read {}: {error}", path.display()));
                return;
            }
        };

        let Ok(manager) = ConfigurationManagerProxy::new(&self.connection).await else {
            return;
        };

        // persistent so it survives a restart (R11); not single-use because the
        // point of importing is to connect it repeatedly.
        match manager.import(&name, &blob, false, true).await {
            Ok(_) => {
                self.unavailable = None;
                // Refresh from the manager rather than appending optimistically:
                // openvpn3 is the source of truth and assigns the real name.
                self.refresh_profiles().await;
            }
            Err(error) => {
                self.unavailable = Some(format!("openvpn3 rejected {name}: {error}"));
            }
        }
    }
}

/// Is the `openvpn3` command on PATH?
///
/// Deliberately only a check. Installing a system package needs root, and R15
/// exists so this applet never has a privilege-escalation path — a tray icon
/// that can install packages is a tray icon that can install anything. Telling
/// the user the command to run is where its responsibility ends.
fn openvpn3_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path).any(|dir| dir.join("openvpn3").is_file())
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
