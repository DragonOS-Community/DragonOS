#!/usr/bin/env bash
set -euo pipefail

dadk_bin=${1:-dadk}
min_version=0.6.1

ready() {
    local output core
    output=$("$dadk_bin" -V 2>/dev/null) || return 1
    [[ $output =~ ^dadk[[:blank:]]+([0-9]+\.[0-9]+\.[0-9]+)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] || return 1
    core=${BASH_REMATCH[1]}
    if [[ $core == "$min_version" && -n ${BASH_REMATCH[2]} ]]; then
        return 1
    fi
    [[ $(printf '%s\n' "$core" "$min_version" | sort -V | head -n1) == "$min_version" ]] || return 1
    printf '%s\n' "$output"
}

if ready; then
    exit 0
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
package_dir="$script_dir/dadk/dadk"
if [[ ! -f $package_dir/Cargo.toml ]]; then
    printf 'DADK source is missing at %s\n' "$package_dir" >&2
    exit 1
fi

# rustup selects the vendored workspace's nightly from its working directory.
(cd -- "$script_dir/dadk" && cargo install --path "$package_dir" --locked)
if ! ready; then
    printf 'DADK installation did not make %s resolve to DADK %s or newer; check DADK and PATH.\n' \
        "$dadk_bin" "$min_version" >&2
    exit 1
fi
