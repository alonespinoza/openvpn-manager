//! Typed proxies for the `openvpn3-linux` D-Bus services.
//!
//! Every signature here was read off openvpn3-linux's own service definitions
//! (`src/sessionmgr/sessionmgr-session.cpp`, `src/events/status.cpp`) rather
//! than inferred from prose docs. The integer widths matter: the C++ enums are
//! `uint8_t`/`uint16_t` internally but are marshalled as D-Bus `u` (u32), so
//! the wire types below are u32 and narrowing happens in `crate::status` and
//! `crate::attention`.

use zbus::proxy;
use zbus::zvariant::OwnedObjectPath;

/// Configuration manager root object — the profile list and import.
#[proxy(
    interface = "net.openvpn.v3.configuration",
    default_service = "net.openvpn.v3.configuration",
    default_path = "/net/openvpn/v3/configuration"
)]
pub trait ConfigurationManager {
    /// Register a profile. `config_str` is the whole `.ovpn` as one blob.
    ///
    /// `persistent = true` so it survives a restart (R11); `single_use = false`
    /// because the point is to connect it repeatedly.
    fn import(
        &self,
        name: &str,
        config_str: &str,
        single_use: bool,
        persistent: bool,
    ) -> zbus::Result<OwnedObjectPath>;

    fn fetch_available_configs(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    fn lookup_config_name(&self, config_name: &str) -> zbus::Result<Vec<OwnedObjectPath>>;
}

/// A single stored profile.
#[proxy(
    interface = "net.openvpn.v3.configuration",
    default_service = "net.openvpn.v3.configuration"
)]
pub trait Configuration {
    fn fetch(&self) -> zbus::Result<String>;

    fn remove(&self) -> zbus::Result<()>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn valid(&self) -> zbus::Result<bool>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn persistent(&self) -> zbus::Result<bool>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn import_timestamp(&self) -> zbus::Result<u64>;
}

/// Session manager root object.
#[proxy(
    interface = "net.openvpn.v3.sessions",
    default_service = "net.openvpn.v3.sessions",
    default_path = "/net/openvpn/v3/sessions"
)]
pub trait SessionManager {
    /// Start a backend client process for a profile. Returns as soon as the
    /// process exists — it does *not* wait for the tunnel, which is why KTD5's
    /// transition machine has to await status rather than trust this returning.
    fn new_tunnel(&self, config_path: &OwnedObjectPath) -> zbus::Result<OwnedObjectPath>;

    fn fetch_available_sessions(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    fn lookup_config_name(&self, config_name: &str) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// Fires on session creation and destruction regardless of who caused it.
    /// This is what makes AE4 — reflecting a change made from a terminal —
    /// work at all.
    #[zbus(signal)]
    fn session_manager_event(&self, path: OwnedObjectPath, event_type: u16, owner: u32)
    -> zbus::Result<()>;
}

/// A single VPN session.
#[proxy(
    interface = "net.openvpn.v3.sessions",
    default_service = "net.openvpn.v3.sessions"
)]
pub trait Session {
    fn connect(&self) -> zbus::Result<()>;

    fn disconnect(&self) -> zbus::Result<()>;

    /// Errors if the backend still needs input before it can proceed.
    fn ready(&self) -> zbus::Result<()>;

    fn restart(&self) -> zbus::Result<()>;

    /// Forward this session's log events to us. Must be enabled *before*
    /// `Connect` or the handshake lines — the ones a failed connect needs —
    /// are never seen (KTD4).
    fn log_forward(&self, enable: bool) -> zbus::Result<()>;

    /// Pending `(type, group)` pairs awaiting input.
    fn user_input_queue_get_type_group(&self) -> zbus::Result<Vec<(u32, u32)>>;

    /// Ids of outstanding requests for a `(type, group)`.
    fn user_input_queue_check(&self, r#type: u32, group: u32) -> zbus::Result<Vec<u32>>;

    /// `(type, group, id, name, description, hidden_input)`.
    fn user_input_queue_fetch(
        &self,
        r#type: u32,
        group: u32,
        id: u32,
    ) -> zbus::Result<(u32, u32, u32, String, String, bool)>;

    fn user_input_provide(
        &self,
        r#type: u32,
        group: u32,
        id: u32,
        value: &str,
    ) -> zbus::Result<()>;

    /// Last status as `(major, minor, message)`. Read at startup to adopt
    /// sessions that already exist.
    #[zbus(property(emits_changed_signal = "false"))]
    fn status(&self) -> zbus::Result<(u32, u32, String)>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn session_name(&self) -> zbus::Result<String>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn config_name(&self) -> zbus::Result<String>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn config_path(&self) -> zbus::Result<OwnedObjectPath>;

    /// Unix epoch seconds. R3's uptime is computed from this rather than from a
    /// timer the applet starts, so an adopted session shows its true age.
    #[zbus(property(emits_changed_signal = "false"))]
    fn session_created(&self) -> zbus::Result<u64>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn device_name(&self) -> zbus::Result<String>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn backend_pid(&self) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn status_change(&self, major: u32, minor: u32, message: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn attention_required(&self, r#type: u32, group: u32, message: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn log(&self, group: u32, level: u32, message: String) -> zbus::Result<()>;
}
