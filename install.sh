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
#   --yes             Skip confirmations (required for --purge without a terminal)
#   --help            Show this help
#
# The script also works when piped to a shell:
#   curl -fsSL https://raw.githubusercontent.com/youtonghy/TodeX_backend/main/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- uninstall --purge --yes
#
# All work happens inside main(), which runs only after the whole file has been
# read, so a truncated download cannot execute a partial script.

set -euo pipefail

REPO="${TODEX_REPO:-youtonghy/TodeX_backend}"
BIN_NAME="todex-agentd"
MAX_BINARY_BYTES=$((256 * 1024 * 1024))
MAX_SUMS_BYTES=$((1024 * 1024))

COMMAND=""
PIN_VERSION=""
PURGE=0
ASSUME_YES=0
INSTALL_DIR=""
DATA_DIR=""
BIN_PATH=""
PLATFORM=""
WSL=0

# Removed by the EXIT trap, so failures never leave temp files or a held lock.
WORK_DIR=""
LOCK_DIR=""

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
  --yes             Skip confirmations (required for --purge without a terminal)
  --help            Show this help

Also works when piped: curl -fsSL <url>/install.sh | bash -s -- update
EOF
}

cleanup() {
    if [[ -n "$LOCK_DIR" ]]; then rm -rf "$LOCK_DIR"; fi
    if [[ -n "$WORK_DIR" ]]; then rm -rf "$WORK_DIR"; fi
}

# ---------- argument parsing ----------

parse_args() {
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
                [[ $# -ge 2 && -n "$2" ]] || die "--prefix requires a value"
                INSTALL_DIR="$2"; shift ;;
            --prefix=*)
                INSTALL_DIR="${1#--prefix=}"
                [[ -n "$INSTALL_DIR" ]] || die "--prefix requires a value" ;;
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
    [[ -z "$PIN_VERSION" ]] || [[ "$PIN_VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
        || die "--version must be a stable version like 1.2.3"
}

# ---------- paths ----------

# Expand a leading ~ and anchor relative paths at $PWD, as the daemon does.
absolute_path() {
    local path="$1"
    case "$path" in
        "~") path="$HOME" ;;
        "~/"*) path="$HOME/${path#"~/"}" ;;
    esac
    [[ "$path" == /* ]] || path="$PWD/${path#./}"
    while [[ "$path" == */ && "$path" != / ]]; do path="${path%/}"; done
    printf '%s\n' "$path"
}

# ---------- platform detection ----------

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            # A shell running under Rosetta reports x86_64 on Apple Silicon.
            if [[ "$arch" == arm64 || "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" == 1 ]]; then
                echo "macos-arm64"; return
            fi
            die "unsupported macOS architecture '$arch' (releases ship macos-arm64 only); build from source with: cargo build --release" ;;
        Linux)
            if [[ "$arch" != "x86_64" ]]; then
                die "unsupported Linux architecture '$arch' (releases ship linux-x64-gnu only); build from source with: cargo build --release"
            fi
            if command -v ldd >/dev/null 2>&1; then
                # musl's ldd exits non-zero for --version, so capture instead of piping under pipefail.
                local libc
                libc="$(ldd --version 2>&1 || true)"
                if [[ "$libc" == *[Mm]usl* ]]; then
                    die "musl-based Linux is not supported by release binaries (linux-x64-gnu only); build from source with: cargo build --release"
                fi
            fi
            echo "linux-x64-gnu"; return ;;
        *) die "unsupported operating system '$os'; on Windows use the windows-x64 release asset or WSL" ;;
    esac
}

is_wsl() {
    [[ -n "${WSL_DISTRO_NAME:-}" ]] || grep -qi microsoft /proc/version 2>/dev/null
}

# ---------- dependency checks ----------

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

check_dependencies() {
    need_cmd curl
    need_cmd uname
    command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 \
        || die "missing required command: sha256sum or shasum"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# ---------- version helpers ----------

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
    body="$(curl -fsSL --proto '=https' --tlsv1.2 --max-time 15 \
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

# Returns non-zero if the daemon is still running afterwards; callers decide how to abort.
stop_daemon() {
    info "Stopping the running daemon"
    "$BIN_PATH" daemon stop || warn "daemon stop reported an error"
    ! daemon_running
}

start_daemon() {
    info "Starting the daemon"
    # The binary was just installed deliberately; skip the startup self-update
    # so a pinned --version is not replaced by the latest release.
    TODEX_AUTO_UPDATE=0 "$BIN_PATH" daemon start \
        || warn "daemon start failed; check $DATA_DIR/logs/todex-agentd-daemon.log"
}

# ---------- download & install ----------

download() {
    local url="$1" dest="$2" max_bytes="$3"
    curl -fL --proto '=https' --tlsv1.2 --max-time 300 --max-filesize "$max_bytes" \
        -H "User-Agent: todex-agentd-installer" \
        -o "$dest" "$url" \
        || die "download failed: $url"
    local size
    size="$(wc -c < "$dest" | tr -d ' ')"
    [[ "$size" -le "$max_bytes" ]] || die "downloaded file exceeds the size limit ($url)"
}

# Download and verify a release into WORK_DIR without touching the install.
fetch_release() {
    local version="$1"
    local asset="${BIN_NAME}-v${version}-${PLATFORM}.bin"
    local base="https://github.com/$REPO/releases/download/v${version}"
    WORK_DIR="$(mktemp -d)"
    local bin="$WORK_DIR/$BIN_NAME"

    info "Downloading $asset"
    download "$base/SHA256SUMS" "$WORK_DIR/SHA256SUMS" "$MAX_SUMS_BYTES"
    download "$base/$asset" "$bin" "$MAX_BINARY_BYTES"

    info "Verifying SHA-256 checksum"
    local expected actual
    expected="$(awk -v f="$asset" '
        { name = $NF; sub(/^\*/, "", name); if (name == f) { n++; hash = $1 } }
        END { if (n == 1) print hash }' "$WORK_DIR/SHA256SUMS")"
    [[ -n "$expected" ]] || die "SHA256SUMS does not list $asset exactly once; refusing to install"
    actual="$(sha256_of "$bin")"
    [[ "$actual" == "$expected" ]] || die "checksum mismatch for $asset (expected $expected, got $actual)"

    chmod 755 "$bin"
    local reported errors
    reported="$("$bin" --version 2>"$WORK_DIR/version.err" || true)"
    if [[ "$reported" != "$BIN_NAME $version" ]]; then
        errors="$(cat "$WORK_DIR/version.err")"
        if [[ "$errors" == *GLIBC_* ]]; then
            die "the $PLATFORM build needs a newer glibc than this system provides; build from source with: cargo build --release"$'\n'"$errors"
        fi
        die "downloaded binary reports '$reported', expected '$BIN_NAME $version'${errors:+$'\n'$errors}"
    fi
}

# Shares the built-in updater's lock so the two never replace the binary concurrently.
acquire_lock() {
    local lock="$BIN_PATH.update-lock"
    if ! mkdir "$lock" 2>/dev/null; then
        [[ -e "$lock" ]] || die "cannot write to $INSTALL_DIR"
        die "another update holds $lock; if none is running, remove that directory and retry"
    fi
    LOCK_DIR="$lock"
}

release_lock() {
    rm -rf "$LOCK_DIR"
    LOCK_DIR=""
}

# Keep only the newest rollback copy (ours or the built-in updater's).
prune_backups() {
    local keep="$1" old
    for old in "$BIN_PATH".previous-*; do
        [[ -e "$old" && "$old" != "$keep" ]] || continue
        rm -f "$old"
    done
}

# Like the built-in updater: hard-link the old binary as the rollback copy, then
# rename the staged file over the installed path, which therefore never goes missing.
replace_binary() {
    local staged="$1" backup=""
    if [[ -e "$BIN_PATH" ]]; then
        backup="$BIN_PATH.previous-$(date +%Y%m%d%H%M%S)-$$"
        ln "$BIN_PATH" "$backup" 2>/dev/null || cp -p "$BIN_PATH" "$backup" \
            || die "cannot back up $BIN_PATH; nothing was changed"
    fi
    if ! mv -f "$staged" "$BIN_PATH"; then
        if [[ -n "$backup" ]]; then rm -f "$backup"; fi
        return 1
    fi
    if [[ -n "$backup" ]]; then
        prune_backups "$backup"
        info "Previous binary saved to $backup"
    fi
}

check_path() {
    case ":$PATH:" in
        *":$INSTALL_DIR:"*|*":$INSTALL_DIR/:"*) ;;
        *)
            warn "$INSTALL_DIR is not in your PATH"
            printf '  Add it with:  export PATH="%s:$PATH"\n' "$INSTALL_DIR" >&2 ;;
    esac
}

confirm() {
    local prompt="$1" reply
    if [[ "$ASSUME_YES" == 1 ]]; then return 0; fi
    # When piped, stdin is the script itself, so ask the terminal directly.
    if ! { : </dev/tty; } 2>/dev/null; then
        warn "no terminal available to confirm; re-run with --yes to proceed"
        return 1
    fi
    printf '%s%s [y/N]%s ' "$C_WARN" "$prompt" "$C_RESET" >&2
    read -r reply </dev/tty || return 1
    [[ "$reply" =~ ^[Yy]$ ]]
}

# The daemon follows a data_dir key in the default directory's config.toml
# (only when TODEX_AGENTD_DATA_DIR is unset). Print a target outside DATA_DIR, if any.
redirected_data_dir() {
    [[ -z "${TODEX_AGENTD_DATA_DIR:-}" && -f "$DATA_DIR/config.toml" ]] || return 0
    local raw target
    raw="$(awk -v sq="'" '
        /^[[:space:]]*\[/ { exit }
        /^[[:space:]]*data_dir[[:space:]]*=/ {
            v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); q = substr(v, 1, 1)
            if (q == "\"" || q == sq) { v = substr(v, 2); i = index(v, q); if (i) print substr(v, 1, i - 1) }
            exit
        }' "$DATA_DIR/config.toml")"
    [[ -n "$raw" ]] || return 0
    case "$raw" in
        "~"|"~/"*|/*) target="$(absolute_path "$raw")" ;;
        *) target="$(absolute_path "$DATA_DIR/$raw")" ;;
    esac
    case "$target/" in
        "$DATA_DIR"/*) ;;
        *) printf '%s\n' "$target" ;;
    esac
}

purge_data_dir() {
    local home redirected
    home="$(absolute_path "$HOME")"
    case "$home/" in
        "$DATA_DIR"/*) die "refusing to delete $DATA_DIR because it contains your home directory; check TODEX_AGENTD_DATA_DIR" ;;
    esac
    [[ "$DATA_DIR" != / ]] || die "refusing to delete /; check TODEX_AGENTD_DATA_DIR"

    redirected="$(redirected_data_dir)"
    if [[ -n "$redirected" ]]; then
        warn "$DATA_DIR/config.toml redirects data_dir to $redirected; --purge does not remove that directory, delete it manually after checking its contents"
    fi
    if confirm "Permanently delete the data directory $DATA_DIR (conversations, devices, config)?"; then
        rm -rf "$DATA_DIR"
        ok "Removed $DATA_DIR"
    else
        info "Kept $DATA_DIR"
    fi
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

    # Nothing on disk or in the running daemon changes until the release is verified.
    fetch_release "$target"

    mkdir -p "$INSTALL_DIR"
    acquire_lock
    local staged="$LOCK_DIR/new"
    cp "$WORK_DIR/$BIN_NAME" "$staged"
    chmod 755 "$staged"

    local was_running=0
    if daemon_running; then
        was_running=1
        stop_daemon || die "could not stop the running daemon; $BIN_PATH was left unchanged"
    fi
    if ! replace_binary "$staged"; then
        if [[ "$was_running" == 1 ]]; then start_daemon; fi
        die "could not install $BIN_PATH; the previous binary is unchanged"
    fi
    release_lock

    ok "Installed $BIN_NAME $target to $BIN_PATH"
    check_path
    if [[ "$was_running" == 1 ]]; then start_daemon; fi
    if [[ -n "$PIN_VERSION" ]]; then
        info "serve, tui, and daemon start update to the latest release on launch; set TODEX_AUTO_UPDATE=0 to stay on $target"
    fi
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

    if [[ -e "$BIN_PATH" ]]; then
        found=1
        acquire_lock
        if daemon_running; then
            stop_daemon || die "could not stop the running daemon; stop it manually, then retry"
        fi
        # Also drop rollback copies left by updates.
        rm -f "$BIN_PATH" "$BIN_PATH".previous-*
        release_lock
        ok "Removed $BIN_PATH"
    fi

    if [[ -d "$DATA_DIR" ]]; then
        found=1
        if [[ "$PURGE" == 1 ]]; then
            purge_data_dir
        else
            info "Data directory kept at $DATA_DIR (use uninstall --purge to remove it)"
        fi
    fi

    [[ "$found" == 1 ]] || warn "Nothing to uninstall: $BIN_PATH not found"
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

main() {
    parse_args "$@"
    : "${HOME:?HOME must be set}"

    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    INSTALL_DIR="$(absolute_path "${INSTALL_DIR:-${TODEX_INSTALL_DIR:-$HOME/.local/bin}}")"
    DATA_DIR="$(absolute_path "${TODEX_AGENTD_DATA_DIR:-$HOME/.todex-agent}")"
    BIN_PATH="$INSTALL_DIR/$BIN_NAME"

    check_dependencies
    PLATFORM="$(detect_platform)"
    if [[ "$PLATFORM" == linux-* ]] && is_wsl; then
        WSL=1
        info "WSL detected; using the linux-x64-gnu build"
    fi

    case "$COMMAND" in
        install)   cmd_install ;;
        update)    cmd_update ;;
        uninstall) cmd_uninstall ;;
        status)    cmd_status ;;
    esac
}

main "$@"
