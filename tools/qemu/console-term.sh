#!/bin/bash

# Resolve the terminal description for the x86_64 BusyBox/hvc0 console.
# Callers remain responsible for adding the validated result to their kernel
# command line.

_dragonos_console_term_is_valid() {
    local LC_ALL=C
    local value="$1"

    [ "${#value}" -le 32 ] && [[ "${value}" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]]
}

dragonos_resolve_console_term() {
    local transport="${1:-}"
    local candidate=""
    local source=""
    local explicit=0

    DRAGONOS_RESOLVED_CONSOLE_TERM=""
    DRAGONOS_RESOLVED_CONSOLE_TERM_SOURCE=""

    case "${transport}" in
        stdio|socket) ;;
        *)
            echo "[ERROR] console TERM resolver received an unsupported transport"
            return 1
            ;;
    esac

    if [ -n "${DRAGONOS_QEMU_CONSOLE_TERM:-}" ]; then
        candidate="${DRAGONOS_QEMU_CONSOLE_TERM}"
        source="explicit"
        explicit=1
    elif [ "${transport}" = "stdio" ] && [ -t 0 ] && [ -t 1 ]; then
        case "${TERM:-}" in
            ""|dumb|linux) return 0 ;;
            *)
                candidate="${TERM}"
                source="host"
                ;;
        esac
    else
        return 0
    fi

    if [ "${explicit}" -eq 1 ] && [ "${candidate}" = "linux" ]; then
        echo "[ERROR] DRAGONOS_QEMU_CONSOLE_TERM=linux cannot be preserved by BusyBox on hvc0; use an accurate non-linux terminfo name"
        return 1
    fi

    if ! _dragonos_console_term_is_valid "${candidate}"; then
        if [ "${explicit}" -eq 1 ]; then
            echo "[ERROR] DRAGONOS_QEMU_CONSOLE_TERM must be a 1-32 byte ASCII terminfo name matching [A-Za-z0-9][A-Za-z0-9._+-]*"
            return 1
        fi
        echo "[WARN] Host TERM is not a safe terminfo name; keeping the guest console fallback"
        return 0
    fi

    DRAGONOS_RESOLVED_CONSOLE_TERM="${candidate}"
    DRAGONOS_RESOLVED_CONSOLE_TERM_SOURCE="${source}"
}
