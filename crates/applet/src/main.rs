//! COSMIC panel applet for openvpn3.
//!
//! Everything with interesting logic lives in the `openvpn3-dbus` crate, which
//! builds and tests on any host. This crate is the Wayland-bound shell around
//! it: panel icon, menu popup, credential prompt, log window.

mod app;
mod dbus;

pub const APP_ID: &str = "io.github.alonespinoza.OpenvpnManager";

/// Well-known name used purely as a single-instance lock (KTD9).
///
/// R16 is satisfied by `cosmic-panel` spawning us at session start, so a
/// systemd unit or XDG autostart entry on top would produce a *second* instance
/// racing the panel-spawned one — two icons, two subscriptions, and the
/// single-session invariant enforced by two processes blind to each other.
/// Owning a bus name is a lock with no stale-pidfile failure mode: it is
/// released when the process dies, however it dies.
const INSTANCE_LOCK_NAME: &str = "io.github.alonespinoza.OpenvpnManager.Instance";

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openvpn_manager_applet=info,openvpn3_dbus=info".into()),
        )
        .init();

    match acquire_instance_lock() {
        Ok(Some(_connection)) => {
            // Held for the process lifetime. Dropping it would release the name
            // and let a second instance in, so it lives until exit.
            std::mem::forget(_connection);
        }
        Ok(None) => {
            tracing::info!("another instance already owns {INSTANCE_LOCK_NAME}; exiting");
            return Ok(());
        }
        Err(error) => {
            // A missing session bus is a real problem, but not one worth
            // refusing to start over — the applet will report openvpn3 as
            // unavailable through the normal path, which is more legible than
            // an icon that never appears.
            tracing::warn!(%error, "could not acquire the single-instance lock; continuing");
        }
    }

    cosmic::applet::run::<app::App>(())
}

/// `Ok(None)` means somebody else already holds it — that is the expected
/// "second instance" case, not an error.
fn acquire_instance_lock() -> zbus::Result<Option<zbus::blocking::Connection>> {
    use zbus::blocking::Connection;
    use zbus::fdo::RequestNameFlags;
    use zbus::fdo::RequestNameReply;

    let connection = Connection::session()?;
    let reply = connection.request_name_with_flags(
        INSTANCE_LOCK_NAME,
        RequestNameFlags::DoNotQueue.into(),
    )?;

    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => Ok(Some(connection)),
        RequestNameReply::Exists | RequestNameReply::InQueue => Ok(None),
    }
}
