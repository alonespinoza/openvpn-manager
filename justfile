name := 'openvpn-manager-applet'
appid := 'io.github.alonespinoza.OpenvpnManager'

# Everything installs under $HOME — R15, no root step anywhere.
prefix := env_var('HOME') / '.local'
bindir := prefix / 'bin'
sharedir := prefix / 'share'
appdir := sharedir / 'applications'
icondir := sharedir / 'icons' / 'hicolor' / 'scalable' / 'apps'
metadir := sharedir / 'metainfo'

default: build

# Portable crate only: logic + D-Bus client. Works on any host.
check:
    cargo test

# The applet needs Wayland and only builds on Linux.
build:
    cargo build -p {{name}} --release

install: build
    install -Dm755 target/release/{{name}} {{bindir}}/{{name}}
    install -Dm644 data/{{appid}}.desktop {{appdir}}/{{appid}}.desktop
    install -Dm644 data/{{appid}}.metainfo.xml {{metadir}}/{{appid}}.metainfo.xml
    # Only the app's own icon, under its own name. The state icons are compiled
    # into the binary — shipping them as network-vpn-*-symbolic would override
    # those icons for every other application on the system.
    install -Dm644 data/icons/network-vpn-symbolic.svg \
        {{icondir}}/{{appid}}-symbolic.svg
    @echo ""
    @echo "Installed. One manual step remains, once:"
    @echo "  Settings → Desktop → Panel → Configure panel applets → add 'OpenVPN'"
    @echo "After that it starts with the panel at every login (R16)."

uninstall:
    rm -f {{bindir}}/{{name}}
    rm -f {{appdir}}/{{appid}}.desktop
    rm -f {{metadir}}/{{appid}}.metainfo.xml
    rm -f {{icondir}}/{{appid}}-symbolic.svg
    # Clean up the overreaching names an earlier version installed.
    rm -f {{icondir}}/network-vpn-disconnected-symbolic.svg
    rm -f {{icondir}}/network-vpn-acquiring-symbolic.svg
    rm -f {{icondir}}/network-vpn-need-auth-symbolic.svg
    rm -f {{icondir}}/network-vpn-error-symbolic.svg
    rm -f {{icondir}}/network-vpn-symbolic.svg

# Build a .deb. Installs to /usr, so it needs sudo to install but not to run.
# `just install` remains the no-root path into $HOME.
deb:
    cargo deb -p {{name}}
    @echo ""
    @echo "Install it with:  sudo apt install ./target/debian/*.deb"

# Report what is missing before you find out the hard way.
doctor:
    #!/usr/bin/env bash
    ok=0
    check() { if eval "$2" >/dev/null 2>&1; then echo "  ok    $1"; else echo "  MISS  $1 — $3"; ok=1; fi; }
    echo "openvpn-manager prerequisites:"
    check "openvpn3"      "command -v openvpn3"          "add OpenVPN's apt repo, then: sudo apt install openvpn3"
    check "openvpn3 D-Bus" "openvpn3 configs-list"       "installed, but its services are not answering"
    check "rust >= 1.93"  "cargo --version"              "rustup update stable (libcosmic needs 1.93+)"
    check "just"          "command -v just"              "sudo apt install just"
    check "wayland"       "test -n \"$WAYLAND_DISPLAY\"" "not running under Wayland"
    check "cosmic panel"  "pgrep -x cosmic-panel"        "the COSMIC panel is not running"
    exit $ok

# Run in the foreground with logging, outside the panel, for debugging.
run:
    RUST_LOG=debug cargo run -p {{name}}
