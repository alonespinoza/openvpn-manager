//! Client for the `openvpn3-linux` D-Bus services, plus the connection state
//! model the tray applet renders.
//!
//! This crate is deliberately free of any GUI dependency so it builds and tests
//! on any host — the applet crate that consumes it only builds on Linux under
//! Wayland.

pub mod attention;
pub mod event;
pub mod logbuf;
pub mod machine;
pub mod profile;
pub mod proxy;
pub mod status;
mod wire;

pub use wire::UnknownWireValue;
