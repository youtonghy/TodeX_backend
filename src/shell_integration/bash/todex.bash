# TodeX shell integration for bash. The daemon starts interactive bash with
# `--rcfile` pointing here, so this file stands in for ~/.bashrc: it loads the
# user's ~/.bashrc first, then marks prompts and commands for the terminal:
# OSC 133 A (prompt), C (command starts), D;<status> (command finished),
# OSC 7 (working directory) and OSC 2 (title).

if [ -r "$HOME/.bashrc" ]; then
    . "$HOME/.bashrc"
fi

# PS0 needs bash 4.4+; older shells (macOS /bin/bash 3.2) keep just the rc file.
if [ "${BASH_VERSINFO[0]:-0}" -gt 4 ] || { [ "${BASH_VERSINFO[0]:-0}" -eq 4 ] && [ "${BASH_VERSINFO[1]:-0}" -ge 4 ]; }; then
    __todex_running=0

    __todex_prompt() {
        local ret=$?
        if [ "$__todex_running" = 1 ]; then
            builtin printf '\e]133;D;%s\a' "$ret"
            __todex_running=0
        fi
        builtin printf '\e]7;kitty-shell-cwd://%s%s\a' "${HOSTNAME}" "${PWD}"
        builtin printf '\e]2;%s\a' "${PWD/#$HOME/\~}"
        builtin printf '\e]133;A\a'
        return "$ret"
    }

    # Runs first, so it sees the command's status; it hands the status on.
    if [[ "$(builtin declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
        PROMPT_COMMAND=(__todex_prompt "${PROMPT_COMMAND[@]}")
    else
        PROMPT_COMMAND="__todex_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
    fi

    # PS0 is expanded in this shell after a command line is read and before
    # it runs; the empty array lookup only sets the flag.
    PS0="${PS0}"'\e]133;C\a${__todex_unset[__todex_running=1]}'
fi
