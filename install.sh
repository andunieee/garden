#!/usr/bin/env bash
# Install garden on a Linux VPS.
#
# Run from an unpacked release:
#   sudo ./install.sh --domain cdn.example.com
#
# Or download it from a release and let it fetch the matching tarball:
#   curl -fsSLO https://github.com/andunieee/garden/releases/latest/download/install.sh
#   sudo bash install.sh --domain cdn.example.com
set -euo pipefail

REPO="${GARDEN_REPO:-andunieee/garden}"
VERSION="${GARDEN_VERSION:-latest}"
SERVICE_USER="garden"
PREFIX="/opt/garden"
CONFIG_DIR="/etc/garden"
STATE_DIR="/var/lib/garden"
DOMAIN=""
CONFIG_SRC=""
START=1
OPERATORS=()

usage() {
    cat <<'EOF'
usage: install.sh [options]

  --domain HOST          public host, e.g. cdn.example.com (https:// is added)
  --operator HEX         hyperb operator public key (64 hex characters), repeatable
  --config FILE          install this complete garden.toml instead of the example
  --version TAG          garden release to install (default: latest)
  --no-start             install without starting the services
  -h, --help             show this help

environment:
  GARDEN_REPO            GitHub repository (default: andunieee/garden)
  GARDEN_VERSION         same as --version
EOF
}

log() {
    printf '==> %s\n' "$*"
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need_value() {
    [[ $# -ge 2 && -n "$2" ]] || die "$1 needs a value"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --domain)
            need_value "$@"
            DOMAIN="$2"
            shift 2
            ;;
        --operator)
            need_value "$@"
            OPERATORS+=("${2,,}")
            shift 2
            ;;
        --config)
            need_value "$@"
            CONFIG_SRC="$2"
            shift 2
            ;;
        --version)
            need_value "$@"
            VERSION="$2"
            shift 2
            ;;
        --no-start)
            START=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (try --help)"
            ;;
    esac
done

[[ "$(uname -s)" == "Linux" ]] || die "this installer is for Linux"
[[ "$(id -u)" -eq 0 ]] || die "run as root (sudo $0 ...)"
command -v systemctl >/dev/null 2>&1 || die "systemd is required (systemctl not found)"

if [[ "$VERSION" != "latest" && "$VERSION" != v* ]]; then
    VERSION="v$VERSION"
fi

case "$(uname -m)" in
    x86_64|amd64)
        TARGET="x86_64-unknown-linux-gnu"
        ;;
    aarch64|arm64)
        TARGET="aarch64-unknown-linux-gnu"
        ;;
    *)
        die "unsupported architecture: $(uname -m) (x86_64 and aarch64 are supported)"
        ;;
esac

if ! command -v curl >/dev/null 2>&1; then
    command -v apt-get >/dev/null 2>&1 || die "curl is required; install it first"
    log "installing curl and CA certificates"
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ca-certificates curl
fi

for cmd in curl tar sha256sum; do
    command -v "$cmd" >/dev/null 2>&1 || die "$cmd is required"
done

if ! getent group "$SERVICE_USER" >/dev/null 2>&1; then
    groupadd --system "$SERVICE_USER"
fi
if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    nologin="$(command -v nologin || echo /bin/false)"
    useradd --system --gid "$SERVICE_USER" --home-dir "$STATE_DIR" --shell "$nologin" "$SERVICE_USER"
fi
if ! id -nG "$SERVICE_USER" | tr ' ' '\n' | grep -qx "$SERVICE_USER"; then
    usermod -aG "$SERVICE_USER" "$SERVICE_USER"
fi

install -d -m 0755 "$PREFIX" "$PREFIX/bin"
install -d -m 0750 -o root -g "$SERVICE_USER" "$CONFIG_DIR"
install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_DIR" "$STATE_DIR/data"

work="$(mktemp -d "${TMPDIR:-/tmp}/garden-install.XXXXXX")"
cleanup() {
    rm -rf "$work"
}
trap cleanup EXIT

src="$work/package"
mkdir -p "$src"

script_dir=""
if [[ -n "${BASH_SOURCE[0]:-}" && -f "${BASH_SOURCE[0]}" ]]; then
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

if [[ -n "$script_dir" && -x "$script_dir/garden" ]]; then
    log "using the release files next to $script_dir/install.sh"
    cp -a "$script_dir/." "$src/"
else
    asset="garden-${TARGET}.tar.gz"
    if [[ "$VERSION" == "latest" ]]; then
        base="https://github.com/${REPO}/releases/latest/download"
    else
        base="https://github.com/${REPO}/releases/download/${VERSION}"
    fi

    log "downloading ${asset}"
    curl -fL --retry 3 --retry-delay 2 --proto '=https' --tlsv1.2 \
        -o "$work/$asset" "$base/$asset"
    curl -fL --retry 3 --retry-delay 2 --proto '=https' --tlsv1.2 \
        -o "$work/$asset.sha256" "$base/$asset.sha256"
    (cd "$work" && sha256sum -c "$asset.sha256")
    tar -xzf "$work/$asset" -C "$src"
fi

[[ -x "$src/garden" ]] || die "the release does not contain the garden binary"
[[ -f "$src/garden.example.toml" ]] || die "the release does not contain garden.example.toml"
[[ -f "$src/systemd/garden.service" ]] || die "the release does not contain systemd/garden.service"

log "installing garden"
install -m 0755 "$src/garden" "$PREFIX/bin/garden"

config="$CONFIG_DIR/garden.toml"
created_config=0
if [[ -n "$CONFIG_SRC" ]]; then
    [[ -f "$CONFIG_SRC" ]] || die "config file not found: $CONFIG_SRC"
    if [[ -e "$config" ]]; then
        backup="$config.bak.$(date +%Y%m%d%H%M%S)"
        log "backing up the existing config to $backup"
        cp -a "$config" "$backup"
    fi
    install -m 0640 -o root -g "$SERVICE_USER" "$CONFIG_SRC" "$config"
elif [[ ! -e "$config" ]]; then
    log "creating $config from the example"
    created_config=1
    install -m 0640 -o root -g "$SERVICE_USER" "$src/garden.example.toml" "$config"

    sed -i 's|^listen = .*|listen = "127.0.0.1:3000"|' "$config"
    sed -i 's|^data_dir = .*|data_dir = "/var/lib/garden/data"|' "$config"

    if [[ -n "$DOMAIN" ]]; then
        public_url="$DOMAIN"
        case "$public_url" in
            http://*|https://*) ;;
            *) public_url="https://$public_url" ;;
        esac
        public_url="${public_url%/}"
        sed -i "0,/^public_url = .*/s|^public_url = .*|public_url = \"$public_url\"|" "$config"
    fi

    seed="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
    sed -i "s|^seed = .*|seed = \"$seed\"|" "$config"

    if (( ${#OPERATORS[@]} > 0 )); then
        ops=""
        for key in "${OPERATORS[@]}"; do
            key="${key,,}"
            [[ "$key" =~ ^[0-9a-f]{64}$ ]] || die "operator key must be 64 hex characters: $key"
            ops+="\"${key}\", "
        done
        ops="${ops%, }"
        sed -i "s|^operators = .*|operators = [${ops}]|" "$config"
    fi
else
    log "keeping the existing $config"
fi
chmod 0640 "$config"
chown root:"$SERVICE_USER" "$config"

if grep -q '^listen = "0.0.0.0' "$config" || grep -q '^listen = "\[::\]' "$config"; then
    warn "garden listens on all interfaces; use 127.0.0.1 behind a TLS reverse proxy"
fi

if grep -q '^kind = "tigerbeetle"' "$config"; then
    warn "$config still uses the TigerBeetle ledger, which garden no longer supports."
    warn "the ledger is now kept in data_dir/ledger.redb; replace the [ledger] section with"
    warn '  kind = "orangedb". the accounts in the old TigerBeetle file are not migrated.'
fi
if [[ -f /etc/systemd/system/tigerbeetle.service ]]; then
    warn "a tigerbeetle.service from an older release is still installed; garden doesn't use"
    warn "it anymore. stop it when you no longer need its data: systemctl disable --now tigerbeetle"
fi

if (( ${#OPERATORS[@]} > 0 )) && ! grep -Fq "${OPERATORS[0]}" "$config"; then
    warn "the existing config was kept; add the operator key(s) to operators in $config"
fi

ldd_out="$(ldd "$PREFIX/bin/garden" 2>&1 || true)"
if [[ "$ldd_out" == *"not found"* ]]; then
    warn "garden is missing shared libraries:"
    printf '%s\n' "$ldd_out" | grep 'not found' >&2 || true
    if command -v apt-get >/dev/null 2>&1; then
        log "trying to install the missing runtime libraries"
        apt-get install -y -qq libssl3t64 2>/dev/null ||
            apt-get install -y -qq libssl3 2>/dev/null ||
            true
        apt-get install -y -qq zlib1g libbrotli1 libzstd1 libgcc-s1 2>/dev/null || true
        ldd_out="$(ldd "$PREFIX/bin/garden" 2>&1 || true)"
    fi
    if [[ "$ldd_out" == *"not found"* ]]; then
        printf '%s\n' "$ldd_out" | grep 'not found' >&2 || true
        die "install the missing libraries and run the installer again"
    fi
fi

log "installing systemd units"
install -m 0644 "$src/systemd/garden.service" /etc/systemd/system/garden.service
if [[ -f "$src/Caddyfile.example" ]]; then
    install -m 0644 "$src/Caddyfile.example" "$CONFIG_DIR/Caddyfile.example"
fi
systemctl daemon-reload

if grep -q '^public_url = "https://cdn.example.com"' "$config"; then
    warn "public_url is still the example; set it to the real public URL"
fi

config_ready=1
if [[ -n "$CONFIG_SRC" ]]; then
    config_ready=1
elif grep -q 'access_key_id = "\.\.\."' "$config" ||
    grep -q 'secret_access_key = "\.\.\."' "$config"; then
    config_ready=0
elif [[ "$created_config" -eq 1 && -z "$DOMAIN" ]]; then
    config_ready=0
elif grep -q '^public_url = "https://cdn.example.com"' "$config"; then
    config_ready=0
fi

if [[ "$config_ready" -eq 1 ]]; then
    systemctl enable garden.service >/dev/null
    if [[ "$START" -eq 1 ]]; then
        log "starting the services"
        systemctl restart garden.service
        log "garden should now be running"
    else
        log "services are enabled but not started (--no-start)"
    fi
else
    log "services are installed but not enabled yet"
fi

cat <<EOF

garden is installed.

  binary   $PREFIX/bin/garden
  config   $config
  data     $STATE_DIR

EOF

if [[ "$config_ready" -eq 0 ]]; then
    cat <<EOF
Finish the configuration first:

  1. edit $config
     - set public_url to the public https url
     - replace the provider bucket, endpoint, and credentials
     - add your hyperb operator key to operators (optional)
  2. enable and start:

     systemctl enable --now garden.service

EOF
fi

cat <<EOF
Back up $config (it holds the Arkade seed and provider keys) and $STATE_DIR.
Put a TLS reverse proxy in front of the server; an example is at
$CONFIG_DIR/Caddyfile.example. Do not expose port 3000.
EOF
