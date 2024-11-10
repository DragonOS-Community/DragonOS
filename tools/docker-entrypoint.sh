#!/bin/bash

CONFIG_FILE=/root/.cargo/config.toml

change_rust_src_to_official() {
echo -e "[source.crates-io]                             \n \
registry = \"sparse+https://index.crates.io/\"            \n \
[net]                                                   \n \
git-fetch-with-cli = true                               \n \
" > $CONFIG_FILE
}

change_rust_src_to_official

exec "$@"
