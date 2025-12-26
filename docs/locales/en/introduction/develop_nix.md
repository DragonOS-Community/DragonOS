:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: introduction/develop_nix.md

- Translation time: 2025-12-26 10:37:03

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Developing DragonOS with Nix

The introduction of Nix eliminates the dependency on manually maintained `bootstrap.sh` for DragonOS development environments. Now, any Linux distribution can quickly build and run DragonOS by installing the Nix environment!

## Installing Nix and Enabling Flake Support

Refer to https://nixos.org/download/ to install Nix: The Nix package manager. (Not NixOS!)

Refer to https://wiki.nixos.org/wiki/Flakes#Setup to enable flake support.

- If you want to experience declarative management with Nix without changing your distribution, try home-manager and configure it to enable flakes and direnv.
- Otherwise, you can directly install flakes in a standalone Nix manner, or add `--experimental-features 'nix-command flakes'` before each command.

## Cloning the Repository

DragonOS now has repository mirrors on multiple hosting platforms:
- `https://github.com/DragonOS-Community/DragonOS.git`
- `https://atomgit.com/DragonOS-Community/DragonOS.git`
- `https://cnb.cool/DragonOS-Community/DragonOS.git`

```shell
git clone https://atomgit.com/DragonOS-Community/DragonOS.git
cd DragonOS
```

## Activating the Kernel Compilation Environment

```shell
nix develop ./tools/nix-dev-shell 
```

If you have configured `direnv`, the first time you enter the repository directory, you will be prompted to execute `direnv allow`, which is equivalent to automatically entering the `nix develop` environment.

## Compiling the Kernel

Perform some dirty fixes (TODO: compatibility improvements or no longer using Makefile for building)

```shell
grep -rlZ '+nightly-2025-08-10' ./build-scripts | xargs -0 sed -i 's/+nightly-2025-08-10//g'
grep -rlZ '+nightly-2025-08-10' ./kernel | xargs -0 sed -i 's/+nightly-2025-08-10//g'
sed -i 's/CCPREFIX=x86_64-linux-gnu-/CCPREFIX=/g' kernel/env.mk
```

Execute the compilation

```shell
make kernel
```

By default, this will compile the kernel ELF to `./bin/kernel/kernel.elf`

## Building the Root Filesystem

```shell
nix run .#rootfs.x86_64
```

This will generate `./bin/qemu-system-x86_64.img`

## Starting the Kernel

```shell
nix run .#start.x86_64
```

Now you can see your terminal loading DragonOS.

:::{note}
To exit the DragonOS (QEMU) environment, type `ctrl + a`, then `x`
:::

## More Nix Command Usage and Nix Script Maintenance

- `cd docs && nix run` Build documentation and start an HTTP server
- If storage space is tight, `nix store gc` Clean up dangling historical build copies
- In the project root directory, `nix flake show` View available build targets
- More
