#!/bin/bash

CONFIG_FILE=/root/.cargo/config.toml

change_rust_src_to_official() {
echo -e "[source.crates-io]                             \n \
registry = \"sparse+https://index.crates.io/\"            \n \
[net]                                                   \n \
git-fetch-with-cli = true                               \n \
" > $CONFIG_FILE
}

# Check if the IN_GITHUB_WORKFLOW environment variable is set and not empty
if [ -n "$IN_GITHUB_WORKFLOW" ]; then
    change_rust_src_to_official
fi


exec "$@"
