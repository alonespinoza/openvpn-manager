# openvpn-manager

A COSMIC panel applet for [openvpn3-linux](https://github.com/OpenVPN/openvpn3-linux).

Connection state is visible in the panel at all times. Connect, disconnect,
switch profiles, and import a `.ovpn` from the menu. Two extra views open only
when needed — the credentials openvpn3 asks for mid-connect, and a read-only log
after a failed connect.

Nothing is stored: every connect that needs a password, one-time code, or
passphrase asks for it again.

**Target:** Pop!_OS 24.04 with COSMIC, under Wayland. There is no GNOME, KDE, or
X11 fallback, and openvpn3 is the only supported backend — see the plan in
[`docs/plans/`](docs/plans/) for why those are identity boundaries rather than
missing features.

---

## Install

Download the `.deb` from the [latest
release](https://github.com/alonespinoza/openvpn-manager/releases/latest) and
install it:

```bash
sudo apt install ./openvpn-manager_1.0.0_amd64.deb
```

Then, **once**: Settings → Desktop → Panel → Configure panel applets → add
**OpenVPN**.

That is the only manual step. From then on `cosmic-panel` starts the applet at
every login. There is deliberately no systemd unit or autostart entry — a second
launcher would race the panel-spawned instance — and a D-Bus name lock makes it
safe if one appears anyway: the second instance exits quietly.

Root is needed once to install the package and never again to run it. Connecting,
disconnecting, and importing all operate as your own user.

### openvpn3

The applet needs `openvpn3-linux`, which is not in Ubuntu's archive — it comes
from OpenVPN's own repository:

```bash
sudo mkdir -p /etc/apt/keyrings
curl -sSfL https://packages.openvpn.net/packages-repo.gpg \
  | sudo tee /etc/apt/keyrings/openvpn.asc >/dev/null
echo "deb [signed-by=/etc/apt/keyrings/openvpn.asc] \
  https://packages.openvpn.net/openvpn3/debian noble main" \
  | sudo tee /etc/apt/sources.list.d/openvpn3.list
sudo apt update && sudo apt install openvpn3
```

Substitute your Ubuntu codename for `noble` if you are not on 24.04, and check
OpenVPN's own documentation if the above drifts.

The applet detects openvpn3 and distinguishes the two failures that matter — not
installed, or installed but not answering — but it will never install it for you.
That needs root, and this applet deliberately has no path to root at all.

## Supported authentication

| Works | Not supported |
|---|---|
| Username and password | Browser-based / web auth (`OPEN_URL`) |
| One-time codes and challenge-response | Smartcard / PKCS#11 |
| Encrypted private-key passphrases | HTTP proxy credentials |

Unsupported requests are not silently ignored — the applet ends the attempt and
says why in the log view. Connect those profiles from a terminal with
`openvpn3 session-start`.

## Troubleshooting

```bash
just doctor    # checks Rust, just, Wayland, the panel, and openvpn3
```

To watch what the applet is actually doing, stop the panel's copy first — the
single-instance lock will make a second one exit immediately:

```bash
pkill -f openvpn-manager-applet
RUST_LOG=debug cargo run -p openvpn-manager-applet
```

Useful for cross-checking what the applet believes against openvpn3 itself:

```bash
openvpn3 configs-list
openvpn3 sessions-list
```

---

# Developer guide

## Prerequisites

- Rust **1.93 or newer** — libcosmic requires it. `rustup update stable`.
- `just`, `cmake`, `pkg-config`, and the Wayland/graphics dev headers

```bash
sudo apt install just cmake pkg-config libxkbcommon-dev libwayland-dev
```

## Layout

Two crates, split so most of the project can be developed and tested on any
machine — including one with no Wayland and no openvpn3:

| Crate | Builds on | Contains |
|---|---|---|
| `crates/openvpn3-dbus` | any host | Status mapping, attention routing, log capture, the transition state machine, and the D-Bus proxies |
| `crates/applet` | Linux + Wayland only | The libcosmic panel icon, popup, and its pages |

`cargo test` at the root runs the portable crate only, so it works on macOS and
in CI without a desktop. The applet is excluded from `default-members` and built
explicitly:

```bash
cargo test                                  # portable logic — anywhere
cargo build -p openvpn-manager-applet       # needs Wayland
```

## Building

```bash
just check      # cargo test — the portable crate
just build      # release build of the applet
just run        # run outside the panel with RUST_LOG=debug
just doctor     # report missing prerequisites
```

### Installing from source

Straight into your home directory, with no root at any point:

```bash
git clone https://github.com/alonespinoza/openvpn-manager.git
cd openvpn-manager
just install    # everything lands under ~/.local
just uninstall  # removes exactly what install placed
```

### Building the package

```bash
cargo install cargo-deb        # once
just deb                       # writes target/debian/*.deb
```

The package installs to `/usr`. It declares openvpn3 as `Recommends` rather than
`Depends`, because a hard dependency would make the package refuse to install on
a machine that has not added OpenVPN's repository yet — worse than installing and
reporting what is missing, which the applet does.

## Design notes

The five state icons are compiled into the binary rather than installed as theme
icons. Names like `network-vpn-symbolic` already exist in the system theme, so a
file of that name would both lose the lookup and override that icon for every
other application on the machine.

Two decisions were revised during implementation and are documented where they
live in the code: the credential prompt and log render as popup pages rather than
separate windows (a panel applet cannot open a usable toplevel), and a bounded
400ms poll runs while a connection transition is in flight, because a status
signal that goes astray otherwise leaves the panel asserting something false.

## License

MIT
