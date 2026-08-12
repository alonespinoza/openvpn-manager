//! The libcosmic applet: panel icon, menu popup, and the two on-demand windows.

use std::collections::HashMap;

use cosmic::app::{Core, Task};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{
    destroy_popup, get_popup,
};
use cosmic::iced::window::{self, Id};
use cosmic::iced::{Length, Limits, Subscription};
use cosmic::widget;
use cosmic::{Application, Element};
use openvpn3_dbus::attention::PromptKind;
use openvpn3_dbus::machine::InputResponse;
use openvpn3_dbus::status::ConnectionState;
use tokio::sync::mpsc;

use crate::dbus::{self, Profile, Snapshot, UiCommand};

pub struct App {
    core: Core,
    /// Channel to the D-Bus worker. `None` until the subscription hands it over.
    commands: Option<mpsc::Sender<UiCommand>>,
    snapshot: Snapshot,

    popup: Option<Id>,
    auth_window: Option<Id>,
    log_window: Option<Id>,

    /// Prompt field values, keyed by input-queue id. Cleared on close (R9).
    field_values: HashMap<u32, String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    WorkerReady(mpsc::Sender<UiCommand>),
    Snapshot(Box<Snapshot>),

    TogglePopup,
    Closed(Id),

    SelectProfile(String),
    Disconnect,
    ImportRequested,
    ImportChosen(Option<std::path::PathBuf>),
    DismissFailure,

    OpenLog,
    CloseLog,

    FieldChanged(u32, String),
    SubmitPrompt,
    CancelPrompt,

    /// Re-renders the uptime label. Display only — state itself is pushed.
    Tick,
}

impl Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = crate::APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        (
            Self {
                core,
                commands: None,
                snapshot: Snapshot::default(),
                popup: None,
                auth_window: None,
                log_window: None,
                field_values: HashMap::new(),
            },
            Task::none(),
        )
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::Closed(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            // The worker owns the D-Bus connection and pushes whole snapshots.
            Subscription::run(worker_stream),
            // Uptime is a label refresh, not a state poll — status still
            // arrives by signal.
            cosmic::iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::Tick),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WorkerReady(sender) => {
                self.commands = Some(sender);
            }

            Message::Snapshot(snapshot) => {
                let prompt_appeared =
                    self.snapshot.prompt.is_none() && snapshot.prompt.is_some();
                let prompt_gone = self.snapshot.prompt.is_some() && snapshot.prompt.is_none();
                self.snapshot = *snapshot;

                if prompt_appeared {
                    return self.open_auth_window();
                }
                if prompt_gone {
                    return self.close_auth_window();
                }
            }

            Message::TogglePopup => {
                return match self.popup.take() {
                    Some(id) => destroy_popup(id),
                    None => {
                        let id = Id::unique();
                        self.popup = Some(id);
                        let mut settings = self.core.applet.get_popup_settings(
                            self.core.main_window_id().unwrap(),
                            id,
                            None,
                            None,
                            None,
                        );
                        settings.positioner.size_limits = Limits::NONE
                            .max_width(420.0)
                            .min_width(320.0)
                            .min_height(80.0)
                            .max_height(700.0);
                        get_popup(settings)
                    }
                };
            }

            Message::Closed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                } else if self.log_window == Some(id) {
                    self.log_window = None;
                } else if self.auth_window == Some(id) {
                    // Closing the window is a cancel, not a dismiss. Leaving the
                    // session parked in auth-pending with no window to return
                    // to would strand it.
                    self.auth_window = None;
                    self.clear_fields();
                    self.send(UiCommand::CancelPrompt);
                }
            }

            Message::SelectProfile(config_path) => {
                self.send(UiCommand::SelectProfile(config_path));
                return self.close_popup();
            }

            Message::Disconnect => {
                self.send(UiCommand::Disconnect);
                return self.close_popup();
            }

            Message::DismissFailure => self.send(UiCommand::DismissFailure),

            Message::ImportRequested => {
                let close = self.close_popup();
                return Task::batch([
                    close,
                    Task::perform(pick_ovpn_file(), |path| {
                        cosmic::action::app(Message::ImportChosen(path))
                    }),
                ]);
            }

            Message::ImportChosen(Some(path)) => self.send(UiCommand::Import(path)),
            // Cancelling the portal picker is a no-op, not an error.
            Message::ImportChosen(None) => {}

            Message::OpenLog => {
                let close = self.close_popup();
                return Task::batch([close, self.open_log_window()]);
            }

            Message::CloseLog => return self.close_log_window(),

            Message::FieldChanged(id, value) => {
                self.field_values.insert(id, value);
            }

            Message::SubmitPrompt => {
                let Some(prompt) = self.snapshot.prompt.clone() else {
                    return Task::none();
                };
                let responses = prompt
                    .fields
                    .iter()
                    .map(|field| InputResponse {
                        r#type: prompt.r#type,
                        group: prompt.group,
                        id: field.id,
                        value: self.field_values.get(&field.id).cloned().unwrap_or_default(),
                    })
                    .collect();

                self.send(UiCommand::SubmitPrompt(responses));
                self.clear_fields();
                return self.close_auth_window();
            }

            Message::CancelPrompt => {
                self.send(UiCommand::CancelPrompt);
                self.clear_fields();
                return self.close_auth_window();
            }

            Message::Tick => {}
        }

        Task::none()
    }

    /// The panel icon. R1/R2: this is the whole point — state without opening
    /// anything.
    fn view(&self) -> Element<Message> {
        self.core
            .applet
            .icon_button(self.snapshot.state.icon_name())
            .on_press(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, id: Id) -> Element<Message> {
        if self.popup == Some(id) {
            self.view_menu()
        } else if self.auth_window == Some(id) {
            self.view_auth()
        } else if self.log_window == Some(id) {
            self.view_log()
        } else {
            widget::text("").into()
        }
    }
}

impl App {
    fn send(&self, command: UiCommand) {
        if let Some(sender) = &self.commands {
            let sender = sender.clone();
            tokio::spawn(async move {
                let _ = sender.send(command).await;
            });
        }
    }

    /// R9 — nothing is retained between connects, including in the UI.
    fn clear_fields(&mut self) {
        for value in self.field_values.values_mut() {
            value.clear();
        }
        self.field_values.clear();
    }

    fn close_popup(&mut self) -> Task<Message> {
        match self.popup.take() {
            Some(id) => destroy_popup(id),
            None => Task::none(),
        }
    }

    // Credential prompt and log window are ordinary toplevels, not panel
    // popups: a layer-shell popup dismisses on focus loss, and losing a
    // half-typed one-time code to a stray click is not acceptable (KTD6).
    fn open_auth_window(&mut self) -> Task<Message> {
        if self.auth_window.is_some() {
            return Task::none();
        }
        let (id, task) = window::open(window::Settings {
            size: cosmic::iced::Size::new(420.0, 280.0),
            resizable: false,
            ..Default::default()
        });
        self.auth_window = Some(id);
        task.then(|_| Task::none())
    }

    fn close_auth_window(&mut self) -> Task<Message> {
        match self.auth_window.take() {
            // `close` is generic over the message type: it emits nothing, so it
            // unifies with our task type directly. Adding a `.then` only made
            // the closure's parameter unconstrained.
            Some(id) => window::close(id),
            None => Task::none(),
        }
    }

    fn open_log_window(&mut self) -> Task<Message> {
        if self.log_window.is_some() {
            return Task::none();
        }
        let (id, task) = window::open(window::Settings {
            size: cosmic::iced::Size::new(720.0, 480.0),
            resizable: true,
            ..Default::default()
        });
        self.log_window = Some(id);
        task.then(|_| Task::none())
    }

    fn close_log_window(&mut self) -> Task<Message> {
        match self.log_window.take() {
            // `close` is generic over the message type: it emits nothing, so it
            // unifies with our task type directly. Adding a `.then` only made
            // the closure's parameter unconstrained.
            Some(id) => window::close(id),
            None => Task::none(),
        }
    }

    // ------------------------------------------------------------- the menu

    fn view_menu(&self) -> Element<Message> {
        let spacing = cosmic::theme::active().cosmic().spacing;
        let mut content = widget::column::with_capacity(8).spacing(spacing.space_xxs);

        if let Some(reason) = &self.snapshot.unavailable {
            content = content.push(widget::text::body(reason));
            return self.core.applet.popup_container(content).into();
        }

        content = content.push(self.view_header());
        content = content.push(widget::divider::horizontal::default());

        if self.snapshot.profiles.is_empty() {
            content = content.push(widget::text::body(
                "No profiles yet — import a .ovpn file to get started.",
            ));
        } else {
            for profile in &self.snapshot.profiles {
                content = content.push(self.view_profile_row(profile));
            }
        }

        content = content.push(widget::divider::horizontal::default());
        content = content.push(
            widget::button::text("Import profile…")
                .width(Length::Fill)
                .on_press(Message::ImportRequested),
        );

        self.core.applet.popup_container(content).into()
    }

    /// R3 — the active profile and how long it has been up.
    fn view_header(&self) -> Element<Message> {
        let state = self.snapshot.state;

        let title = match (&self.snapshot.active_name, state) {
            (Some(name), ConnectionState::Connected) => format!("Connected — {name}"),
            (Some(name), ConnectionState::Connecting) => format!("Connecting — {name}"),
            (Some(name), ConnectionState::AuthPending) => format!("Waiting for input — {name}"),
            (Some(name), ConnectionState::Failed) => format!("Failed — {name}"),
            (_, ConnectionState::Failed) => "Connection failed".to_owned(),
            _ => "Not connected".to_owned(),
        };

        let mut column = widget::column::with_capacity(3).push(widget::text::title4(title));

        if state == ConnectionState::Connected {
            if let Some(since) = self.snapshot.session_created {
                column = column.push(widget::text::caption(format!("Up {}", format_uptime(since))));
            }
        }

        if state == ConnectionState::Failed {
            if !self.snapshot.status_message.is_empty() {
                column = column.push(widget::text::caption(&self.snapshot.status_message));
            }
            column = column.push(
                widget::row::with_capacity(2)
                    .spacing(8)
                    .push(widget::button::text("View log").on_press(Message::OpenLog))
                    .push(widget::button::text("Dismiss").on_press(Message::DismissFailure)),
            );
        }

        column.into()
    }

    /// R4/R5 — every profile, its state, and the right action for it.
    fn view_profile_row<'a>(&'a self, profile: &'a Profile) -> Element<'a, Message> {
        let is_active = self.snapshot.active_config.as_deref() == Some(&profile.config_path);

        let (label, message) = if is_active {
            match self.snapshot.state {
                ConnectionState::Connected
                | ConnectionState::Connecting
                | ConnectionState::AuthPending => ("Disconnect", Message::Disconnect),
                _ => (
                    "Connect",
                    Message::SelectProfile(profile.config_path.clone()),
                ),
            }
        } else {
            (
                "Connect",
                Message::SelectProfile(profile.config_path.clone()),
            )
        };

        let status = if is_active {
            state_label(self.snapshot.state)
        } else {
            "Not connected"
        };

        widget::row::with_capacity(3)
            .spacing(8)
            .align_y(cosmic::iced::Alignment::Center)
            .push(
                widget::column::with_capacity(2)
                    .width(Length::Fill)
                    .push(widget::text::body(&profile.name))
                    .push(widget::text::caption(status)),
            )
            .push(widget::button::text(label).on_press(message))
            .into()
    }

    // -------------------------------------------------------- the two windows

    fn view_auth(&self) -> Element<Message> {
        let Some(prompt) = &self.snapshot.prompt else {
            return widget::text("").into();
        };

        let heading = match prompt.kind {
            PromptKind::UserPassword => "Sign in",
            PromptKind::PrivateKeyPassphrase => "Private key passphrase",
            PromptKind::Challenge => "Additional verification",
        };

        let mut content = widget::column::with_capacity(prompt.fields.len() + 4)
            .spacing(12)
            .padding(20)
            .push(widget::text::title3(heading));

        if !prompt.message.trim().is_empty() {
            content = content.push(widget::text::body(&prompt.message));
        }

        for field in &prompt.fields {
            let value = self
                .field_values
                .get(&field.id)
                .map(String::as_str)
                .unwrap_or_default();

            let id = field.id;
            let mut input = widget::text_input(field.label.as_str(), value)
                .on_input(move |v| Message::FieldChanged(id, v))
                .on_submit(|_| Message::SubmitPrompt);

            if field.masked {
                input = input.password();
            }

            content = content.push(input);
        }

        content = content.push(
            widget::row::with_capacity(2)
                .spacing(8)
                .push(widget::button::text("Cancel").on_press(Message::CancelPrompt))
                .push(widget::button::suggested("Connect").on_press(Message::SubmitPrompt)),
        );

        content.into()
    }

    /// R13 — read-only, most recent attempt.
    fn view_log(&self) -> Element<Message> {
        let body: Element<'_, Message> = match &self.snapshot.log {
            Some(lines) if !lines.is_empty() => {
                let mut column = widget::column::with_capacity(lines.len()).spacing(2);
                for line in lines {
                    column = column.push(widget::text::monotext(line));
                }
                widget::scrollable(column).height(Length::Fill).into()
            }
            // KTD4: a session adopted from outside never had log forwarding on.
            // Saying so beats a blank pane, which reads as a bug or a clean run.
            _ => widget::text::body(
                "No log was captured for this session — it was started outside the applet.",
            )
            .into(),
        };

        widget::column::with_capacity(3)
            .spacing(12)
            .padding(20)
            .push(widget::text::title3("Session log"))
            .push(body)
            .push(widget::button::text("Close").on_press(Message::CloseLog))
            .into()
    }
}

fn state_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Disconnected => "Not connected",
        ConnectionState::Connecting => "Connecting…",
        ConnectionState::Connected => "Connected",
        ConnectionState::AuthPending => "Waiting for input",
        ConnectionState::Failed => "Failed",
    }
}

/// Derived from openvpn3's `session_created`, so an adopted session shows its
/// true age rather than the time since the applet noticed it.
fn format_uptime(since_epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(since_epoch);
    let seconds = now.saturating_sub(since_epoch);

    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// KTD7 — the portal is the correct Wayland file-picker path and keeps the
/// applet indifferent to sandboxing.
async fn pick_ovpn_file() -> Option<std::path::PathBuf> {
    use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};

    let request = SelectedFiles::open_file()
        .title("Import OpenVPN profile")
        .accept_label("Import")
        .modal(true)
        .filter(FileFilter::new("OpenVPN profile").glob("*.ovpn"))
        .send()
        .await
        .ok()?
        .response()
        .ok()?;

    request
        .uris()
        .first()
        .and_then(|uri| uri.to_file_path().ok())
}

fn worker_stream() -> impl futures_util::Stream<Item = Message> {
    cosmic::iced::stream::channel(64, |mut output| async move {
        let (command_tx, command_rx) = mpsc::channel(32);
        let (snapshot_tx, mut snapshot_rx) = mpsc::channel(32);

        tokio::spawn(dbus::run(command_rx, snapshot_tx));

        use futures_util::SinkExt;
        let _ = output.send(Message::WorkerReady(command_tx)).await;

        while let Some(snapshot) = snapshot_rx.recv().await {
            let _ = output.send(Message::Snapshot(Box::new(snapshot))).await;
        }
    })
}
