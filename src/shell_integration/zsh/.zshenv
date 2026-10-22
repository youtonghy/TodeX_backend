# TodeX shell integration for zsh. The daemon points ZDOTDIR here, so zsh
# reads this file first: it puts the user's ZDOTDIR back (zsh then reads the
# user's remaining startup files from there), loads the user's .zshenv, and
# in interactive shells adds the prompt hooks.

if [[ -n "${TODEX_ZSH_ZDOTDIR+X}" ]]; then
    'builtin' 'export' ZDOTDIR="$TODEX_ZSH_ZDOTDIR"
    'builtin' 'unset' 'TODEX_ZSH_ZDOTDIR'
else
    'builtin' 'unset' 'ZDOTDIR'
fi

{
    'builtin' 'typeset' __todex_file=${ZDOTDIR-$HOME}"/.zshenv"
    [[ ! -r "$__todex_file" ]] || 'builtin' 'source' '--' "$__todex_file"
} always {
    if [[ -o 'interactive' ]]; then
        __todex_file="${${(%):-%x}:A:h}"/todex-integration.zsh
        [[ ! -r "$__todex_file" ]] || 'builtin' 'source' '--' "$__todex_file"
    fi
    'builtin' 'unset' '__todex_file'
}
