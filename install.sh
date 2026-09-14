#!/usr/bin/env bash
# One-click manager for todex-agentd on macOS, Linux, and WSL.
#
#   install.sh                Install the latest release (or update an existing install)
#   install.sh install        Same as above
#   install.sh update         Update an existing install to the latest release
#   install.sh uninstall      Stop the daemon and remove the installed binary
#   install.sh status         Show installed version, latest release, and daemon state
#
# Options:
#   --version X.Y.Z   Install a specific release instead of the latest
#   --prefix DIR      Install directory (default: ~/.local/bin, or $TODEX_INSTALL_DIR)
#   --purge           With uninstall: also remove the data directory (~/.todex-agent)
#   --yes             Skip interactive confirmations
#   --help            Show this help
#
# The script also works when piped to a shell:
#   curl -fsSL https://raw.githubusercontent.com/youtonghy/TodeX_backend/main/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- uninstall --purge --yes

set -euo pipefail

REPO="${TODEX_REPO:-youtonghy/TodeX_backend}"
BIN_NAME="todex-agentd"
INSTALL_DIR="${TODEX_INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${TODEX_AGENTD_DATA_DIR:-$HOME/.todex-agent}"
MAX_BINARY_BYTES=$((256 * 1024 * 1024))

COMMAND=""
PIN_VERSION=""
PURGE=0
ASSUME_YES=0

# ---------- output helpers ----------

if [[ -t 1 ]]; then
    C_INFO=$'\033[36m'; C_OK=$'\033[32m'; C_WARN=$'\033[33m'; C_ERR=$'\033[31m'; C_RESET=$'\033[0m'
else
    C_INFO=""; C_OK=""; C_WARN=""; C_ERR=""; C_RESET=""
fi

info()  { printf '%s==>%s %s\n' "$C_INFO" "$C_RESET" "$*"; }
ok()    { printf '%s==>%s %s\n' "$C_OK" "$C_RESET" "$*"; }
warn()  { printf '%swarn:%s %s\n' "$C_WARN" "$C_RESET" "$*" >&2; }
die()   { printf '%serror:%s %s\n' "$C_ERR" "$C_RESET" "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
One-click manager for todex-agentd on macOS, Linux, and WSL.

  install.sh                Install the latest release (or update an existing install)
  install.sh install        Same as above
  install.sh update         Update an existing install to the latest release
  install.sh uninstall      Stop the daemon and remove the installed binary
  install.sh status         Show installed version, latest release, and daemon state

Options:
  --version X.Y.Z   Install a specific release instead of the latest
  --prefix DIR      Install directory (default: ~/.local/bin, or $TODEX_INSTALL_DIR)
  --purge           With uninstall: also remove the data directory (~/.todex-agent)
  --yes             Skip interactive confirmations
  --help            Show this help

Also works when piped: curl -fsSL <url>/install.sh | bash -s -- update
EOF
}

# ---------- argument parsing ----------

while [[ $# -gt 0 ]]; do
    case "$1" in
        install|update|uninstall|status)
            [[ -z "$COMMAND" ]] || die "only one command may be given"
            COMMAND="$1" ;;
        --version)
            [[ $# -ge 2 ]] || die "--version requires a value"
            PIN_VERSION="${2#v}"; shift ;;
        --version=*) PIN_VERSION="${1#--version=}"; PIN_VERSION="${PIN_VERSION#v}" ;;
        --prefix)
            [[ $# -ge 2 ]] || die "--prefix requires a value"
            INSTALL_DIR="$2"; shift ;;
        --prefix=*) INSTALL_DIR="${1#--prefix=}" ;;
        --purge) PURGE=1 ;;
        --yes|-y) ASSUME_YES=1 ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

COMMAND="${COMMAND:-install}"

if [[ "$PURGE" == 1 && "$COMMAND" != "uninstall" ]]; then
    die "--purge is only valid with uninstall"
fi

# ---------- platform detection ----------

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                arm64) echo "macos-arm64"; return ;;
                *) die "unsupported macOS architecture '$arch' (releases ship macos-arm64 only); build from source with: cargo build --release" ;;
            esac ;;
        Linux)
            if [[ "$arch" != "x86_64" ]]; then
                die "unsupported Linux architecture '$arch' (releases ship linux-x64-gnu only); build from source with: cargo build --release"
            fi
            if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | head -n1 | grep -qi musl; then
                die "musl-based Linux is not supported by release binaries (linux-x64-gnu only); build from source with: cargo build --release"
            fi
            echo "linux-x64-gnu"; return ;;
        *) die "unsupported operating system '$os'; on Windows use the windows-x64 release asset or WSL" ;;
    esac
}

is_wsl() {
    [[ -n "${WSL_DISTRO_NAME:-}" ]] || grep -qi microsoft /proc/version 2>/dev/null
}

WSL=0
PLATFORM="$(detect_platform)"
if [[ "$PLATFORM" == linux-* ]] && is_wsl; then
    WSL=1
    info "WSL detected; using the linux-x64-gnu build"
fi

[[ -z "$PIN_VERSION" ]] || [[ "$PIN_VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
    || die "--version must be a stable version like 1.2.3"

# ---------- dependency checks ----------

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

need_cmd curl
need_cmd uname

if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    die "missing required command: sha256sum or shasum"
fi

# ---------- version helpers ----------

BIN_PATH="$INSTALL_DIR/$BIN_NAME"

installed_version() {
    [[ -x "$BIN_PATH" ]] || return 1
    local out
    out="$("$BIN_PATH" --version 2>/dev/null)" || return 1
    out="${out#"$BIN_NAME "}"
    [[ "$out" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
    echo "$out"
}

semver_gt() {
    # semver_gt A B -> true if A > B (strict numeric X.Y.Z only)
    local a b i
    IFS='.' read -ra a <<< "$1"
    IFS='.' read -ra b <<< "$2"
    for i in 0 1 2; do
        if (( 10#${a[i]:-0} > 10#${b[i]:-0} )); then return 0; fi
        if (( 10#${a[i]:-0} < 10#${b[i]:-0} )); then return 1; fi
    done
    return 1
}

latest_version() {
    local body tag
    body="$(curl -fsSL --max-time 15 \
        -H 'Accept: application/vnd.github+json' \
        -H "User-Agent: todex-agentd-installer" \
        "https://api.github.com/repos/$REPO/releases/latest")" \
        || die "could not query the latest GitHub release for $REPO (network error, or no public release exists yet)"
    tag="$(printf '%s' "$body" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
    [[ -n "$tag" ]] || die "could not parse the latest release tag for $REPO"
    tag="${tag#v}"
    [[ "$tag" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
        || die "latest release tag '$tag' is not a stable version"
    echo "$tag"
}

# ---------- daemon helpers ----------

daemon_running() {
    [[ -x "$BIN_PATH" ]] || return 1
    "$BIN_PATH" daemon status 2>/dev/null | grep -q '^Daemon running:'
}

stop_daemon() {
    if daemon_running; then
        info "Stopping the running daemon"
        "$BIN_PATH" daemon stop || warn "daemon stop reported an error; continuing anyway"
    fi
}

start_daemon() {
    info "Starting the daemon"
    "$BIN_PATH" daemon start \
        || warn "daemon start failed; check $DATA_DIR/logs/todex-agentd-daemon.log"
}

# ---------- download & install ----------

download() {
    local url="$1" dest="$2" max_bytes="$3"
    curl -fL --max-time 300 \
        -H "User-Agent: todex-agentd-installer" \
        -o "$dest" "$url" \
        || die "download failed: $url"
    local size
    size="$(wc -c < "$dest" | tr -d ' ')"
    [[ "$size" -le "$max_bytes" ]] || die "downloaded file exceeds the size limit ($url)"
}

install_release() {
    local version="$1"
    local asset="${BIN_NAME}-v${version}-${PLATFORM}.bin"
    local base="https://github.com/$REPO/releases/download/v${version}"
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    info "Downloading $asset"
    download "$base/SHA256SUMS" "$tmp/SHA256SUMS" 1048576
    download "$base/$asset" "$tmp/$BIN_NAME" "$MAX_BINARY_BYTES"

    info "Verifying SHA-256 checksum"
    local expected actual
    expected="$(awk -v f="$asset" '{name=$NF; sub(/^\*/, "", name); if (name == f) print $1}' "$tmp/SHA256SUMS")"
    [[ -n "$expected" ]] || die "SHA256SUMS does not list $asset; refusing to install"
    actual="$(sha256_of "$tmp/$BIN_NAME")"
    [[ "$actual" == "$expected" ]] || die "checksum mismatch for $asset (expected $expected, got $actual)"

    chmod +x "$tmp/$BIN_NAME"
    local reported
    reported="$("$tmp/$BIN_NAME" --version 2>/dev/null || true)"
    [[ "$reported" == "$BIN_NAME $version" ]] \
        || die "downloaded binary reports '$reported', expected '$BIN_NAME $version'"

    mkdir -p "$INSTALL_DIR"

    local staged="$INSTALL_DIR/.$BIN_NAME.new.$$"
    cp "$tmp/$BIN_NAME" "$staged"
    chmod 755 "$staged"

    # Keep one rollback copy next to the binary, matching the built-in updater.
    if [[ -f "$BIN_PATH" ]]; then
        local backup="$BIN_PATH.previous-$(date +%Y%m%d%H%M%S)-$$"
        mv "$BIN_PATH" "$backup"
        info "Previous binary saved to $backup"
    fi
    mv "$staged" "$BIN_PATH"

    rm -rf "$tmp"
    trap - RETURN
}

check_path() {
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            warn "$INSTALL_DIR is not in your PATH"
            printf '  Add it with:  export PATH="%s:$PATH"\n' "$INSTALL_DIR" >&2 ;;
    esac
}

confirm() {
    local prompt="$1"
    [[ "$ASSUME_YES" == 1 ]] && return 0
    [[ -t 0 ]] || return 0   # non-interactive (piped): the flags themselves are consent
    printf '%s%s [y/N]%s ' "$C_WARN" "$prompt" "$C_RESET"
    local reply
    read -r reply
    [[ "$reply" =~ ^[Yy]$ ]]
}

# ---------- commands ----------

cmd_install() {
    local current target
    current="$(installed_version || true)"
    target="$PIN_VERSION"
    if [[ -z "$target" ]]; then
        info "Checking the latest release"
        target="$(latest_version)"
    fi

    if [[ -n "$current" ]]; then
        if [[ "$current" == "$target" ]]; then
            ok "$BIN_NAME $current is already installed and up to date ($BIN_PATH)"
            return
        fi
        if semver_gt "$current" "$target" && [[ -z "$PIN_VERSION" ]]; then
            ok "Installed version $current is newer than the latest release $target; nothing to do"
            return
        fi
        info "Updating $BIN_NAME $current -> $target"
    else
        if [[ -e "$BIN_PATH" ]]; then
            warn "Existing binary at $BIN_PATH does not report a stable release version; replacing it"
        fi
        info "Installing $BIN_NAME $target"
    fi

    local was_running=0
    daemon_running && was_running=1
    stop_daemon
    install_release "$target"
    ok "Installed $BIN_NAME $target to $BIN_PATH"
    check_path
    [[ "$was_running" == 1 ]] && start_daemon
    if [[ -z "$current" ]]; then
        printf '\nNext steps:\n'
        printf '  %s tui            # interactive daemon controller\n' "$BIN_NAME"
        printf '  %s daemon start   # run in the background\n' "$BIN_NAME"
        printf '  %s serve          # run in the foreground\n' "$BIN_NAME"
    fi
}

cmd_update() {
    [[ -x "$BIN_PATH" ]] || die "$BIN_NAME is not installed at $BIN_PATH; run the install command first"
    cmd_install
}

cmd_uninstall() {
    local found=0

    if [[ -x "$BIN_PATH" ]]; then
        found=1
        stop_daemon
        if daemon_running; then
            warn "Daemon is still running; it will keep serving until stopped"
        fi
    fi

    if [[ -e "$BIN_PATH" ]]; then
        found=1
        rm -f "$BIN_PATH"
        ok "Removed $BIN_PATH"
        # Rollback copies left by updates and any abandoned update lock.
        rm -f "$BIN_PATH".previous-* 2>/dev/null || true
        [[ -d "$BIN_PATH.update-lock" ]] && rm -rf "$BIN_PATH.update-lock"
    fi

    if [[ -d "$DATA_DIR" ]]; then
        if [[ "$PURGE" == 1 ]]; then
            if confirm "Permanently delete the data directory $DATA_DIR (conversations, devices, config)?"; then
                rm -rf "$DATA_DIR"
                ok "Removed $DATA_DIR"
            else
                info "Kept $DATA_DIR"
            fi
        else
            info "Data directory kept at $DATA_DIR (use uninstall --purge to remove it)"
        fi
    fi

    [[ "$found" == 1 || -d "$DATA_DIR" ]] || warn "Nothing to uninstall: $BIN_PATH not found"
}

cmd_status() {
    local current latest=""
    current="$(installed_version || true)"
    printf 'Install dir : %s\n' "$INSTALL_DIR"
    printf 'Binary      : %s\n' "$BIN_PATH"
    printf 'Platform    : %s%s\n' "$PLATFORM" "$([[ "$WSL" == 1 ]] && echo ' (WSL)')"
    if [[ -n "$current" ]]; then
        printf 'Installed   : %s\n' "$current"
    elif [[ -e "$BIN_PATH" ]]; then
        printf 'Installed   : %s\n' "$("$BIN_PATH" --version 2>/dev/null || echo 'unrecognized binary')"
    else
        printf 'Installed   : not installed\n'
    fi
    if latest="$(latest_version 2>/dev/null)"; then
        printf 'Latest      : %s\n' "$latest"
        if [[ -n "$current" ]] && semver_gt "$latest" "$current"; then
            printf 'Update      : %s -> %s available\n' "$current" "$latest"
        fi
    else
        printf 'Latest      : unavailable (no public release or network error)\n'
    fi
    if daemon_running; then
        "$BIN_PATH" daemon status || true
    else
        printf 'Daemon      : not running\n'
    fi
    printf 'Data dir    : %s\n' "$DATA_DIR"
}

case "$COMMAND" in
    install)   cmd_install ;;
    update)    cmd_update ;;
    uninstall) cmd_uninstall ;;
    status)    cmd_status ;;
esac
