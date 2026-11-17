_melosd() {
    local cur prev opts
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    case "$prev" in
        --config|--music-dir)
            if declare -F _filedir >/dev/null; then
                _filedir
            else
                COMPREPLY=( $(compgen -f -- "$cur") )
            fi
            return 0
            ;;
    esac

    opts="--config --music-dir --help --version"
    COMPREPLY=( $(compgen -W "$opts" -- "$cur") )
    return 0
}

complete -F _melosd melosd
