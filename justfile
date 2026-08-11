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
    for icon in data/icons/*.svg; do \
        install -Dm644 "$icon" {{icondir}}/"$(basename "$icon")"; \
    done
    @echo ""
    @echo "Installed. One manual step remains, once:"
    @echo "  Settings → Desktop → Panel → Configure panel applets → add 'OpenVPN'"
    @echo "After that it starts with the panel at every login (R16)."

uninstall:
    rm -f {{bindir}}/{{name}}
    rm -f {{appdir}}/{{appid}}.desktop
    rm -f {{metadir}}/{{appid}}.metainfo.xml
    rm -f {{icondir}}/network-vpn-*-symbolic.svg

# Run in the foreground with logging, outside the panel, for debugging.
run:
    RUST_LOG=debug cargo run -p {{name}}
