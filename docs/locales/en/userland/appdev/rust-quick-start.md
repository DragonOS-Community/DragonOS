:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: userland/appdev/rust-quick-start.md

- Translation time: 2025-05-19 01:41:52

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Quick Start Guide for Rust Application Development

## Compilation Environment

&emsp;&emsp;DragonOS has partial binary compatibility with Linux, so you can use the Rust compiler for Linux to compile.

## Project Configuration

### Creating from a Template

:::{note}
This feature requires dadk version 0.2.0 or higher. For older versions, please refer to the historical DragonOS documentation.
:::

1. Use the `bootstrap.sh` script in the tools directory of DragonOS to initialize the environment.
2. Enter `cargo install cargo-generate` in the terminal.
3. Enter the following command in the terminal:

```shell
cargo generate --git https://github.com/DragonOS-Community/Rust-App-Template
```
To create the project. If your network is slow, please use a mirror site.
```shell
cargo generate --git https://git.mirrors.dragonos.org/DragonOS-Community/Rust-App-Template
```

4. Use `cargo run` to run the project.
5. In the `user/dadk/config` directory of DragonOS, refer to the template [userapp_config.toml](https://github.com/DragonOS-Community/DADK/blob/main/dadk-config/templates/config/userapp_config.toml) to create a compilation configuration, and install it to the `/` directory of DragonOS.
(When using the compilation command options of dadk, please use the `make install` configuration in the Makefile for compilation and installation)
6. Compile DragonOS to install.

### Manual Configuration

If you need to port other libraries or programs to DragonOS, please refer to the configuration in the template.

Since DragonOS currently does not support dynamic linking, you need to specify `-C target-feature=+crt-static -C link-arg=-no-pie` in RUSTFLAGS.
