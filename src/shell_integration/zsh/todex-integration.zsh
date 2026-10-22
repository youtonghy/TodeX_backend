# TodeX prompt hooks for zsh: OSC 133 A (prompt), C (command starts),
# D;<status> (command finished), OSC 7 (working directory), OSC 2 (title).

(( ${+__todex_running} )) && return 0
builtin typeset -gi __todex_running=0
builtin autoload -Uz add-zsh-hook

__todex_precmd() {
    builtin local ret=$?
    if (( __todex_running )); then
        builtin printf '\e]133;D;%s\a' "$ret"
        __todex_running=0
    fi
    builtin printf '\e]7;kitty-shell-cwd://%s%s\a' "${HOST}" "${PWD}"
    builtin printf '\e]2;%s\a' "${(%):-%~}"
    builtin printf '\e]133;A\a'
}

__todex_preexec() {
    builtin printf '\e]2;%s\a' "${1//[[:cntrl:]]/ }"
    builtin printf '\e]133;C\a'
    __todex_running=1
}

add-zsh-hook precmd __todex_precmd
add-zsh-hook preexec __todex_preexec
