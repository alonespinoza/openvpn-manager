//! The libcosmic applet: panel icon and its popup.
//!
//! **KTD6 was revised here.** The plan called for the credential prompt and log
//! to be ordinary toplevel windows, so a half-typed one-time code could not be
//! lost to a stray click. In practice a panel applet cannot open a usable
//! toplevel — the surface renders mispositioned and takes no input — and
//! COSMIC's own network applet does Wi-Fi password entry inside its popup
//! rather than in a window. The plan's risk row named this fallback: a
//! layer-shell popup with keyboard input, accepting the dismiss-on-focus-loss
//! cost. That is what this is.

use std::collections::HashMap;

use cosmic::app::{Core, Task};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::window::Id;
use cosmic::iced::{Length, Limits, Subscription};
use cosmic::widget;
use cosmic::{Application, Element};
use openvpn3_dbus::attention::PromptKind;
use openvpn3_dbus::machine::InputResponse;
use openvpn3_dbus::status::ConnectionState;
use tokio::sync::mpsc;

use crate::dbus::{self, Profile, Snapshot, UiCommand};

/// What the popup is currently showing. One surface, several pages — the panel
/// applet has exactly one place it can put interactive content.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    Menu,
    Manage(Profile),
    Auth,
    Log,
}

pub struct App {
    core: Core,
    /// Channel to the D-Bus worker. `None` until the subscription hands it over.
    commands: Option<mpsc::Sender<UiCommand>>,
    snapshot: Snapshot,

    popup: Option<Id>,
    page: Page,

    rename_value: String,
    /// Delete is two-step: the button arms before it acts. A destructive,
    /// irreversible action one click away in a menu is too easy to hit.
    delete_armed: bool,

    /// Prompt field values, keyed by input-queue id. Cleared on close (R9).
    field_values: HashMap<u32, String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    WorkerReady(mpsc::Sender<UiCommand>),
    Snapshot(Box<Snapshot>),

    TogglePopup,
    Closed(Id),
    BackToMenu,

    SelectProfile(String),
    Disconnect,
    ImportRequested,
    ImportChosen(Option<std::path::PathBuf>),
    DismissFailure,

    OpenLog,
    OpenAuth,

    ManageProfile(String),
    RenameChanged(String),
    ConfirmRename,
    ArmDelete,
    DisarmDelete,
    ConfirmDelete,

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
                page: Page::Menu,
                rename_value: String::new(),
                delete_armed: false,
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
            Message::WorkerReady(sender) => self.commands = Some(sender),

            Message::Snapshot(snapshot) => {
                let prompt_appeared = self.snapshot.prompt.is_none() && snapshot.prompt.is_some();
                let prompt_gone = self.snapshot.prompt.is_some() && snapshot.prompt.is_none();
                self.snapshot = *snapshot;

                if prompt_appeared {
                    // A connect that needs input has stalled until it gets some,
                    // so open the popup rather than waiting to be noticed.
                    self.field_values.clear();
                    self.page = Page::Auth;
                    return self.open_popup();
                }
                if prompt_gone && self.page == Page::Auth {
                    self.page = Page::Menu;
                    self.clear_fields();
                }
            }

            Message::TogglePopup => {
                return match self.popup {
                    Some(_) => self.close_popup(),
                    None => {
                        self.page = Page::Menu;
                        self.open_popup()
                    }
                };
            }

            Message::Closed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    self.cancel_prompt_if_open();
                    self.page = Page::Menu;
                }
            }

            Message::BackToMenu => {
                self.cancel_prompt_if_open();
                self.delete_armed = false;
                self.page = Page::Menu;
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

            Message::OpenLog => self.page = Page::Log,
            Message::OpenAuth => self.page = Page::Auth,

            Message::ManageProfile(config_path) => {
                if let Some(profile) = self
                    .snapshot
                    .profiles
                    .iter()
                    .find(|p| p.config_path == config_path)
                    .cloned()
                {
                    self.rename_value = profile.name.clone();
                    self.delete_armed = false;
                    self.page = Page::Manage(profile);
                }
            }

            Message::RenameChanged(value) => {
                self.rename_value = value;
                // Editing the name is a signal the user is not here to delete.
                self.delete_armed = false;
            }

            Message::ConfirmRename => {
                if let Page::Manage(profile) = &self.page {
                    let new_name = self.rename_value.trim().to_owned();
                    if !new_name.is_empty() && new_name != profile.name {
                        self.send(UiCommand::RenameProfile {
                            config_path: profile.config_path.clone(),
                            new_name,
                        });
                    }
                }
                self.page = Page::Menu;
            }

            Message::ArmDelete => self.delete_armed = true,
            Message::DisarmDelete => self.delete_armed = false,

            Message::ConfirmDelete => {
                if let Page::Manage(profile) = &self.page {
                    self.send(UiCommand::DeleteProfile(profile.config_path.clone()));
                }
                self.delete_armed = false;
                self.page = Page::Menu;
            }

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
                self.page = Page::Menu;
                return self.close_popup_without_cancel();
            }

            Message::CancelPrompt => {
                self.send(UiCommand::CancelPrompt);
                self.clear_fields();
                self.page = Page::Menu;
                return self.close_popup_without_cancel();
            }

            Message::Tick => {}
        }

        Task::none()
    }

    /// The panel icon. R1/R2: this is the whole point — state without opening
    /// anything.
    fn view(&self) -> Element<'_, Message> {
        self.core
            .applet
            .icon_button(self.snapshot.state.icon_name())
            .on_press(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, id: Id) -> Element<'_, Message> {
        if self.popup != Some(id) {
            return widget::text("").into();
        }

        let content = match &self.page {
            Page::Menu => self.view_menu(),
            Page::Manage(profile) => self.view_manage(profile),
            Page::Auth => self.view_auth(),
            Page::Log => self.view_log(),
        };

        self.core.applet.popup_container(content).into()
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

    /// Dismissing the prompt is a cancel, not a defer (AE2). Leaving the session
    /// parked in auth-pending with no way back to it would strand it.
    ///
    /// This is the cost of the popup fallback: a click elsewhere dismisses the
    /// popup, and that ends the connect attempt.
    fn cancel_prompt_if_open(&mut self) {
        if self.page == Page::Auth && self.snapshot.prompt.is_some() {
            self.send(UiCommand::CancelPrompt);
            self.clear_fields();
        }
    }

    fn open_popup(&mut self) -> Task<Message> {
        if self.popup.is_some() {
            return Task::none();
        }

        // openvpn3 owns the profile list and it can change without us — an
        // `openvpn3 config-import` from a terminal, say.
        self.send(UiCommand::RefreshProfiles);

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
            .max_width(460.0)
            .min_width(340.0)
            .min_height(80.0)
            .max_height(700.0);
        get_popup(settings)
    }

    fn close_popup(&mut self) -> Task<Message> {
        self.cancel_prompt_if_open();
        self.close_popup_without_cancel()
    }

    /// Used when the prompt was already resolved, so the cancel path must not
    /// fire and undo a submission.
    fn close_popup_without_cancel(&mut self) -> Task<Message> {
        self.page = Page::Menu;
        match self.popup.take() {
            Some(id) => destroy_popup(id),
            None => Task::none(),
        }
    }

    // ------------------------------------------------------------- the menu

    fn view_menu(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::active().cosmic().spacing;

        if let Some(reason) = &self.snapshot.unavailable {
            return widget::text::body(reason).into();
        }

        let mut content = widget::column::with_capacity(8).spacing(spacing.space_xxs);
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

        content.into()
    }

    /// R3 — the active profile and how long it has been up.
    fn view_header(&self) -> Element<'_, Message> {
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

        // A dismissed prompt used to be unreachable. Since dismissing now
        // cancels, this is the way back only while the request is still live.
        if state == ConnectionState::AuthPending && self.snapshot.prompt.is_some() {
            column =
                column.push(widget::button::suggested("Enter credentials").on_press(Message::OpenAuth));
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
            .push(
                widget::button::text("⋯")
                    .on_press(Message::ManageProfile(profile.config_path.clone())),
            )
            .into()
    }

    // ------------------------------------------------------- manage a profile

    fn view_manage<'a>(&'a self, profile: &'a Profile) -> Element<'a, Message> {
        let is_active = self.snapshot.active_config.as_deref() == Some(&profile.config_path);
        let renamed =
            self.rename_value.trim() != profile.name && !self.rename_value.trim().is_empty();

        let mut rename_button = widget::button::suggested("Rename");
        if renamed {
            rename_button = rename_button.on_press(Message::ConfirmRename);
        }

        let mut content = widget::column::with_capacity(8)
            .spacing(10)
            .push(widget::text::title4("Manage profile"))
            .push(
                widget::text_input("Profile name", &self.rename_value)
                    .on_input(Message::RenameChanged)
                    .on_submit(|_| Message::ConfirmRename),
            )
            .push(rename_button)
            .push(widget::divider::horizontal::default());

        // Deleting a profile out from under a live session orphans it and leaves
        // the menu describing something gone. Refuse and say why, rather than
        // tearing down their tunnel as a side effect of a delete.
        if is_active {
            content = content.push(widget::text::caption(
                "This profile is in use. Disconnect it before deleting.",
            ));
        } else if self.delete_armed {
            content = content
                .push(widget::text::body(format!(
                    "Delete “{}”? This cannot be undone.",
                    profile.name
                )))
                .push(
                    widget::row::with_capacity(2)
                        .spacing(8)
                        .push(widget::button::text("Cancel").on_press(Message::DisarmDelete))
                        .push(
                            widget::button::destructive("Delete").on_press(Message::ConfirmDelete),
                        ),
                );
        } else {
            content = content
                .push(widget::button::destructive("Delete profile…").on_press(Message::ArmDelete));
        }

        content
            .push(widget::button::text("Back").on_press(Message::BackToMenu))
            .into()
    }

    // ------------------------------------------------------------ credentials

    fn view_auth(&self) -> Element<'_, Message> {
        let Some(prompt) = &self.snapshot.prompt else {
            return widget::text::body("No credentials are being requested.").into();
        };

        let heading = match prompt.kind {
            PromptKind::UserPassword => "Sign in",
            PromptKind::PrivateKeyPassphrase => "Private key passphrase",
            PromptKind::Challenge => "Additional verification",
        };

        let mut content = widget::column::with_capacity(prompt.fields.len() + 4)
            .spacing(10)
            .push(widget::text::title4(heading));

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

            let input = widget::text_input(field.label.as_str(), value)
                .on_input(move |v| Message::FieldChanged(id, v))
                .on_submit(|_| Message::SubmitPrompt);

            content = content.push(if field.masked {
                input.password()
            } else {
                input
            });
        }

        content
            .push(
                widget::row::with_capacity(2)
                    .spacing(8)
                    .push(widget::button::text("Cancel").on_press(Message::CancelPrompt))
                    .push(widget::button::suggested("Connect").on_press(Message::SubmitPrompt)),
            )
            .into()
    }

    // ------------------------------------------------------------- the log

    /// R13 — read-only, most recent attempt.
    fn view_log(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = match &self.snapshot.log {
            Some(lines) if !lines.is_empty() => {
                let mut column = widget::column::with_capacity(lines.len()).spacing(2);
                for line in lines {
                    column = column.push(widget::text::monotext(line));
                }
                widget::scrollable(column)
                    .height(Length::Fixed(360.0))
                    .into()
            }
            // KTD4: a session adopted from outside never had log forwarding on.
            // Saying so beats a blank pane, which reads as a bug or a clean run.
            _ => widget::text::body(
                "No log was captured for this session — it was started outside the applet.",
            )
            .into(),
        };

        widget::column::with_capacity(3)
            .spacing(10)
            .push(widget::text::title4("Session log"))
            .push(body)
            .push(widget::button::text("Back").on_press(Message::BackToMenu))
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

fn worker_stream() -> impl cosmic::iced::futures::Stream<Item = Message> {
    // Go through iced's own futures re-export rather than a separate
    // futures-util dependency, so the Sender and the SinkExt in scope are
    // guaranteed to be the same crate version.
    use cosmic::iced::futures::SinkExt;
    use cosmic::iced::futures::channel::mpsc as iced_mpsc;

    cosmic::iced::stream::channel(64, |mut output: iced_mpsc::Sender<Message>| async move {
        let (command_tx, command_rx) = mpsc::channel(32);
        let (snapshot_tx, mut snapshot_rx) = mpsc::channel(32);

        tokio::spawn(dbus::run(command_rx, snapshot_tx));

        let _ = output.send(Message::WorkerReady(command_tx)).await;

        while let Some(snapshot) = snapshot_rx.recv().await {
            let _ = output.send(Message::Snapshot(Box::new(snapshot))).await;
        }
    })
}
