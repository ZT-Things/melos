_melos() {
    local cur prev opts
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    case "$prev" in
        --config)
            if declare -F _filedir >/dev/null; then
                _filedir
            else
                COMPREPLY=( $(compgen -f -- "$cur") )
            fi
            return 0
            ;;
    esac

    if [[ "$cur" == -* ]]; then
        opts="--config --trace-startup --help --version --copy"
        COMPREPLY=( $(compgen -W "$opts" -- "$cur") )
        return 0
    fi

    if [[ $COMP_CWORD -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "status stop import scan" -- "$cur") )
        return 0
    fi

    if [[ "${COMP_WORDS[1]}" == "import" ]]; then
        if declare -F _filedir >/dev/null; then
            _filedir
        else
            COMPREPLY=( $(compgen -f -- "$cur") )
        fi
    fi
    return 0
}

complete -F _melos melos
