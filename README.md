# openvpn-manager

A COSMIC panel applet for [openvpn3-linux](https://github.com/OpenVPN/openvpn3-linux).

Connection state is visible in the panel at all times. Connect, disconnect,
switch profiles, and import a `.ovpn` from the menu. Two windows open only when
needed — the credential prompt openvpn3 asks for mid-connect, and a read-only
log after a failed connect.

Nothing is stored: every connect that needs a password, one-time code, or
passphrase asks for it again.

**Target:** Pop!_OS 24.04 with COSMIC, under Wayland. There is no GNOME, KDE, or
X11 fallback, and openvpn3 is the only supported backend — see the plan in
[`docs/plans/`](docs/plans/) for why those are identity boundaries rather than
missing features.

---

## Requirements

- Pop!_OS 24.04 (or another COSMIC 1.x desktop) on Wayland
- `openvpn3-linux`, with its session D-Bus services reachable as your user
- Rust **1.93 or newer** — libcosmic requires it. `rustup update stable` if unsure.
- `just`, `cmake`, `pkg-config`, and the usual Wayland/graphics dev headers

```bash
sudo apt install just cmake pkg-config libxkbcommon-dev libwayland-dev
```

## Build and install

```bash
git clone https://github.com/alonespinoza/openvpn-manager.git
cd openvpn-manager
just install
```

Everything lands under `~/.local` — there is no root step anywhere in the
install, by design.

Then, **once**: Settings → Desktop → Panel → Configure panel applets → add
**OpenVPN**.

That is the only manual step. From then on `cosmic-panel` starts the applet at
every login; there is deliberately no systemd unit or autostart entry, because a
second launcher would race the panel-spawned instance. A D-Bus name lock makes
that safe if it happens anyway — the second instance exits quietly.

## Supported authentication

| Works | Not supported |
|---|---|
| Username and password | Browser-based / web auth (`OPEN_URL`) |
| One-time codes and challenge-response | Smartcard / PKCS#11 |
| Encrypted private-key passphrases | HTTP proxy credentials |

Unsupported requests are not silently ignored — the applet ends the attempt and
says why in the log window. Connect those profiles from a terminal with
`openvpn3 session-start`.

## Layout

Two crates, split so most of the project can be developed and tested on any
machine:

| Crate | Builds on | Contains |
|---|---|---|
| `crates/openvpn3-dbus` | any host | Status mapping, attention routing, log capture, the transition state machine, and the D-Bus proxies |
| `crates/applet` | Linux + Wayland only | The libcosmic panel icon, menu, and the two windows |

`cargo test` at the root runs the portable crate only, so it works on macOS and
in CI without a desktop. The applet is excluded from `default-members` and built
explicitly:

```bash
cargo test                                  # portable logic — anywhere
cargo build -p openvpn-manager-applet       # needs Wayland
```

## Development

```bash
just check      # cargo test — the portable crate
just build      # release build of the applet
just run        # run outside the panel with RUST_LOG=debug
just uninstall  # remove everything just install placed
```

### Debugging

Run it in the foreground to see what it is doing:

```bash
RUST_LOG=debug cargo run -p openvpn-manager-applet
```

Useful for cross-checking what the applet believes against openvpn3 itself:

```bash
openvpn3 configs-list
openvpn3 sessions-list
```

## License

MIT
