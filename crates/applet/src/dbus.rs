//! The async worker that owns the D-Bus connection.
//!
//! It drives the (already tested) `Machine` and `LogStore` from `openvpn3-dbus`
//! and publishes a whole `Snapshot` whenever anything changes. Publishing a
//! snapshot rather than fine-grained deltas keeps the UI a pure function of
//! state — there is no way for the panel to drift out of sync with the worker.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use openvpn3_dbus::attention::{ClientAttentionGroup, ClientAttentionType, PromptField, PromptKind};
use openvpn3_dbus::logbuf::{LogEntry, LogStore, SessionLog};
use openvpn3_dbus::machine::{Command, Event, InputResponse, Machine};
use openvpn3_dbus::proxy::{
    ConfigurationManagerProxy, ConfigurationProxy, SessionManagerProxy, SessionProxy,
};
use openvpn3_dbus::status::{ConnectionState, Status};
use tokio::sync::mpsc;
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
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(error) => {
            let _ = snapshots
                .send(Snapshot {
                    unavailable: Some(format!("No D-Bus session bus: {error}")),
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
            let key = path.to_string();
            // Profiles are keyed by object path, never by name — openvpn3
            // permits duplicate names and the menu has to stay unambiguous.
            let name = match ConfigurationProxy::builder(&self.connection)
                .path(path.clone())
                .and_then(|b| Ok(b))
            {
                Ok(builder) => match builder.build().await {
                    Ok(proxy) => proxy.name().await.unwrap_or_else(|_| key.clone()),
                    Err(_) => key.clone(),
                },
                Err(_) => key.clone(),
            };

            self.profile_names.insert(key.clone(), name.clone());
            self.profiles.push(Profile {
                config_path: key,
                name,
            });
        }

        self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
    }

    fn mark_unavailable(&mut self, error: zbus::Error) {
        self.unavailable = Some(format!(
            "openvpn3 is not reachable ({error}). Is openvpn3-linux installed and running?"
        ));
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
                // EventType 3 is session destruction in openvpn3's
                // SessionManager::EventType; anything else we treat as "exists".
                let removed = args.event_type == 3;
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

    async fn session_proxy(&self, path: &OwnedObjectPath) -> zbus::Result<SessionProxy<'_>> {
        SessionProxy::builder(&self.connection)
            .path(path.clone())?
            .build()
            .await
    }

    /// Forward one session's three signal streams into the shared wire channel.
    async fn subscribe_session(&mut self, path: &OwnedObjectPath, wire: &mpsc::Sender<Wire>) {
        let Ok(session) = self.session_proxy(path).await else {
            return;
        };
        let session = Arc::new(session.into_owned());
        let key = path.to_string();

        if let Ok(mut stream) = session.receive_status_change().await {
            let (wire, key) = (wire.clone(), key.clone());
            let handle = tokio::spawn(async move {
                while let Some(signal) = stream.next().await {
                    let Ok(args) = signal.args() else { continue };
                    let Ok(status) =
                        Status::from_wire(args.major as u8, args.minor as u16, args.message.clone())
                    else {
                        continue;
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

        if let Ok(mut stream) = session.receive_attention_required().await {
            let (wire, key) = (wire.clone(), key.clone());
            let handle = tokio::spawn(async move {
                while let Some(signal) = stream.next().await {
                    let Ok(args) = signal.args() else { continue };
                    let _ = wire
                        .send(Wire::Attention {
                            session: key.clone(),
                            r#type: args.type_,
                            group: args.group,
                            message: args.message.clone(),
                        })
                        .await;
                }
            });
            self.forwarders.push(handle.abort_handle());
        }

        if let Ok(mut stream) = session.receive_log().await {
            let (wire, key) = (wire.clone(), key.clone());
            let handle = tokio::spawn(async move {
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
    }

    // --------------------------------------------------------------- events

    async fn on_ui_command(&mut self, command: UiCommand, wire: &mpsc::Sender<Wire>) {
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
        };

        if let Some(event) = event {
            let commands = self.machine.handle(event);
            self.execute(commands, wire).await;
        }
    }

    async fn on_wire(&mut self, wire_event: Wire, wire: &mpsc::Sender<Wire>) {
        let event = match wire_event {
            Wire::Status { session, status } => {
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
                                self.logs.note(
                                    &session_path,
                                    format!("Submitting credentials failed: {error}"),
                                );
                            }
                        }
                    }
                    // R9: nothing is kept. The responses are dropped here and
                    // the UI zeroes its own buffers on close.
                    drop(responses);
                }

                Command::Ready { session_path } => {
                    if let Some(session) = self.proxy_for(&session_path).await {
                        if let Err(error) = session.ready().await {
                            self.logs
                                .note(&session_path, format!("Backend not ready: {error}"));
                        }
                    }
                }

                Command::Note {
                    session_path,
                    message,
                } => self.logs.note(&session_path, message),
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

    async fn proxy_for(&self, session_path: &str) -> Option<SessionProxy<'_>> {
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
        let Ok(ids) = session.user_input_queue_check(type_wire, group_wire).await else {
            return Vec::new();
        };

        let mut fields = Vec::new();
        for id in ids {
            if let Ok((_, _, id, name, description, hidden_input)) = session
                .user_input_queue_fetch(type_wire, group_wire, id)
                .await
            {
                fields.push(
                    openvpn3_dbus::attention::InputRequest {
                        r#type,
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

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
