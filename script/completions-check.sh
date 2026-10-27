#!/usr/bin/env bash
# Loads `drt completions bash` the way README.md says to, with `eval`, and
# asks the loaded `_drt` what a partial verb and a partial flag complete
# to. CI runs it on Linux, under Git Bash on Windows, and under macOS's
# own /bin/bash 3.2, where `source <(...)` would have loaded nothing.
# With zsh on PATH it also loads `drt completions zsh` after compinit and
# checks `drt` is bound to `_drt`; that is a load check, not a completion.
#
#   script/completions-check.sh [path/to/drt]    # default: drt on PATH
# No `-u`: the generated script is written for an interactive shell, and
# bash before 4.4 calls an empty array unbound.
set -e
DRT=${1:-drt}
CASES=(
    # words typed            what must be offered
    "drt bui|buildinfo"
    "drt run --fi|--file"
    "drt completions ba|bash"
)

echo "completions-check: bash $BASH_VERSION"
eval "$("$DRT" completions bash)"
for case in "${CASES[@]}"; do
    typed=${case%%|*}
    want=${case##*|}
    read -r -a COMP_WORDS <<< "$typed"
    COMP_CWORD=$((${#COMP_WORDS[@]} - 1))
    COMP_LINE=$typed
    COMP_POINT=${#typed}
    COMPREPLY=()
    _drt drt "${COMP_WORDS[COMP_CWORD]}" "${COMP_WORDS[COMP_CWORD - 1]}"
    case " ${COMPREPLY[*]} " in
        *" $want "*) echo "completions-check: '$typed' offers $want" ;;
        *)
            echo "completions-check: '$typed' offered '${COMPREPLY[*]}', not $want" >&2
            exit 1
            ;;
    esac
done

if command -v zsh > /dev/null; then
    script=$("$DRT" completions zsh)
    # `2>&1` so a load that prints an error fails the comparison too.
    bound=$(zsh -fc 'autoload -Uz compinit && compinit -u && eval "$1" && print -r -- $_comps[drt]' zsh "$script" 2>&1)
    if [ "$bound" != _drt ]; then
        echo "completions-check: zsh did not bind drt to _drt: $bound" >&2
        exit 1
    fi
    echo "completions-check: zsh $(zsh -fc 'print $ZSH_VERSION') binds drt to _drt"
fi
