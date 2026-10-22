# TodeX shell integration for fish. The daemon puts the directory holding
# fish/vendor_conf.d first in XDG_DATA_DIRS, so fish loads this file as vendor
# configuration. It takes that entry back out, then marks prompts and
# commands for the terminal: OSC 133 A (prompt), C (command starts),
# D;<status> (command finished) and OSC 7 (working directory). fish 4 and
# later emit these marks themselves, and fish sets the title itself.

if set -q TODEX_FISH_DATA_DIR
    set -l __todex_dirs (string split : -- "$XDG_DATA_DIRS")
    set -l __todex_index (contains -i -- "$TODEX_FISH_DATA_DIR" $__todex_dirs)
    and set -e __todex_dirs[$__todex_index]
    if set -q __todex_dirs[1]
        set -gx XDG_DATA_DIRS (string join : -- $__todex_dirs)
    else
        set -e XDG_DATA_DIRS
    end
    set -e TODEX_FISH_DATA_DIR
end

status is-interactive; or exit 0
test (string split . -- $FISH_VERSION)[1] -lt 4 2>/dev/null; or exit 0

function __todex_preexec --on-event fish_preexec
    printf '\e]133;C\a'
    set -g __todex_running 1
end

function __todex_postexec --on-event fish_postexec
    set -l last_status $status
    if set -q __todex_running
        printf '\e]133;D;%s\a' $last_status
        set -e __todex_running
    end
end

function __todex_prompt --on-event fish_prompt
    printf '\e]7;kitty-shell-cwd://%s%s\a' $hostname "$PWD"
    printf '\e]133;A\a'
end
