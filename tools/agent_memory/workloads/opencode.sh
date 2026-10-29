#!/bin/bash
# A real, credential-free OpenCode workload. The launcher supplies isolation/deadlines.
# --pure fixes the profile to built-in tools/provider without external plugins.
set +x
set -eu
umask 077

if [ "$#" -ne 4 ]; then
    printf 'usage: opencode.sh WORKSPACE PAYLOAD_ROOT OUTPUT_DIR prepare|prepare-cold|coding|resume|cold\n' >&2
    exit 2
fi
workspace=$1
payload=$2
output=$3
mode=$4
case "$mode" in prepare|prepare-cold|coding|resume|cold) ;; *) exit 2 ;; esac
for path in "$workspace" "$payload" "$output"; do
    case "$path" in /*) ;; *) exit 2 ;; esac
done
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
node="$payload/bin/node"
agent="$payload/bin/opencode"
workspace_env() {
    export HOME="$workspace/home"
    export XDG_CONFIG_HOME="$workspace/config"
    export XDG_DATA_HOME="$workspace/data"
    export XDG_CACHE_HOME="$workspace/cache"
    export XDG_STATE_HOME="$workspace/state"
}
case "$mode" in prepare|prepare-cold)
    # This phase runs outside the measured leaf, without credentials or Agent calls.
    # A partially prepared workspace has no marker and cannot run a measured task.
    [ -x "$node" ] && [ -x "$agent" ] && [ ! -e "$workspace" ] || exit 1
    # The caller owns/creates the parent; mkdir itself claims the fresh workspace.
    mkdir -- "$workspace"
    workspace=$(CDPATH= cd -- "$workspace" && pwd -P)
    workspace_env
    mkdir -p -- "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME" "$workspace/project"
    cp -- "$script_dir/input.json" "$workspace/project/input.json"
    prepared_mode=cold
    if [ "$mode" = prepare ]; then
        [ -d "$payload/sdk/@ai-sdk/deepseek/node_modules/@ai-sdk/deepseek" ] || exit 1
        mkdir -p -- "$XDG_CACHE_HOME/opencode/packages"
        cp -a -- "$payload/sdk/." "$XDG_CACHE_HOME/opencode/packages/"
        prepared_mode=coding
    fi
    printf '%s\n' "$prepared_mode" > "$workspace/prepared"
    exit 0
esac
mkdir -p -- "$output"
chmod 700 -- "$output"
output=$(CDPATH= cd -- "$output" && pwd -P)
# A result directory is single-use; a retry must receive a different one.
if [ -e "$output/workload.json" ] || [ -e "$output/events.jsonl" ]; then exit 2; fi
trap 'status=$?; if [ ! -f "$output/workload.json" ]; then printf '\''{"schema":1,"success":false,"failure":"workload_setup_failed"}\n'\'' > "$output/workload.json"; fi; exit "$status"' EXIT
fail() {
    printf '{"schema":1,"success":false,"failure":"%s"}\n' "$1" > "$output/workload.json"
    exit 1
}
[ -x "$node" ] && [ -x "$agent" ] || fail missing_runtime

if [ "$mode" = resume ]; then
    [ -d "$workspace/project" ] && [ -f "$workspace/session-id" ] || fail missing_session
else
    [ -f "$workspace/prepared" ] || fail workspace_not_prepared
    [ "$(cat -- "$workspace/prepared")" = "$mode" ] || fail wrong_preparation
    [ ! -e "$workspace/project/sum.mjs" ] && [ ! -e "$workspace/project/result.json" ] && [ ! -e "$workspace/session-id" ] || fail workspace_not_fresh
    # Atomic consumption prevents two measured launches from sharing this state.
    mv -- "$workspace/prepared" "$workspace/started" || fail preparation_consumed
fi
workspace=$(CDPATH= cd -- "$workspace" && pwd -P)
case "$output/" in "$workspace/"*) fail output_inside_workspace ;; esac
workspace_env
export PATH="$payload/bin:/usr/local/bin:/usr/bin:/bin"
export OPENCODE_CONFIG="$script_dir/opencode.json"
# Prevent an inherited inline config from overriding the fixed public profile.
unset OPENCODE_CONFIG_CONTENT OPENCODE_CONFIG_DIR
model=${AGENT_MODEL:-deepseek/deepseek-flash}
# Public provider/model identifier only, never an endpoint or credential argument.
[[ "$model" =~ ^deepseek/[A-Za-z0-9._-]+$ ]] || fail unsupported_model

# The credential file is an explicit private runtime input, never a payload file.
env_file=${AGENT_ENV_FILE:-/opt/env.sh}
[ -r "$env_file" ] || fail missing_credentials
# Source only in the agent subshell: the independent checker never inherits the key.
cd -- "$workspace/project"
if [ "$mode" = resume ]; then
    session=$(cat -- "$workspace/session-id")
    [[ "$session" =~ ^ses_[A-Za-z0-9]+$ ]] || fail invalid_session
    # Initial artifacts have already passed the independent checker. Removing
    # them excludes direct project-file rereads from this continuation profile.
    # The actual OpenCode database, SDK and session identity remain untouched.
    rm -f -- "$workspace/project/input.json" "$workspace/project/result.json" \
        "$workspace/project/sum.mjs" "$workspace/project/resume-result.json"
    prompt='Continue our existing conversation. Without reading input.json, result.json, or sum.mjs, recall the input total from our prior task. Use the bash tool to run node and write resume-result.json containing exactly {"previousSum":NUMBER,"nextSum":NUMBER_PLUS_ONE}, substituting the remembered numbers. Do not install packages. Finish only after the command succeeds.'
    set -- --session "$session"
else
    prompt='Work only in the current project. Use the read tool to read input.json. Create sum.mjs exporting function sum(values), without CLI side effects when imported. When executed directly, its CLI must read the JSON array from process.argv[2] and print exactly JSON {"sum":number}. Do not install packages. Use the bash tool to run node sum.mjs input.json and save stdout as result.json. Run Node assertions for sum([]) === 0, sum([-4,1]) === -3, and sum([2,7,11]) === 20. Fix errors, then finish.'
    set --
fi
agent_exit=0
(
    . "$env_file"
    set +x
    export DEEPSEEK_API_KEY
    [ -n "${DEEPSEEK_API_KEY:-}" ] || exit 125
    exec "$agent" run --pure --format json --model "$model" "$@" "$prompt"
) > "$output/events.jsonl" 2> "$output/agent-stderr.log" || agent_exit=$?

# No raw transcript or stderr is echoed to the serial/public result channel.
unset DEEPSEEK_API_KEY
"$node" "$script_dir/check.cjs" "$workspace" "$output" "$mode" "$agent_exit"
