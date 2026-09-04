# Developing DragonOS with Nix

The introduction of Nix eliminates the need for manually maintained `bootstrap.sh` in DragonOS's development environment. Now, any Linux distribution can quickly build and run DragonOS by installing the Nix environment!

## Installing Nix and Enabling Flake Support

Refer to https://nixos.org/download/ to install Nix: The Nix package manager. (Not NixOS!)

Refer to https://wiki.nixos.org/wiki/Flakes#Setup to enable flake support.

- If you want to experience Nix's declarative management without changing your distribution, try home-manager and configure it to enable flakes and direnv.
- Otherwise, you can directly install flakes in a standalone Nix manner, or add `--experimental-features 'nix-command flakes'` before each command.

## Domestic Mirror Acceleration (Recommended)

If you are in China and do not have a global proxy, the first dependency pull may be slow or even fail. This repository has built-in domestic mirror configurations in `flake.nix`, which will take effect automatically when using `nix develop / nix run`.

If it still doesn't work, it is recommended to append the following content to your user-level configuration (it will not overwrite your existing configuration):

```shell
mkdir -p ~/.config/nix
cat >> ~/.config/nix/nix.conf <<'EOF'
# DragonOS Nix mirror (CN)
extra-substituters = https://mirrors.tuna.tsinghua.edu.cn/nix-channels/store https://mirrors.ustc.edu.cn/nix-channels/store
extra-trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
EOF
```

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
nix develop
```

If you have configured `direnv`, the first time you enter the repository directory, you will be prompted to execute `direnv allow`, which is equivalent to automatically entering the `nix develop` environment.

## Compiling the Kernel

Execute the compilation:

```shell
make kernel
```

By default, this will compile the kernel ELF to `./bin/kernel/kernel.elf`

## Building the Root Filesystem

```shell
nix run .#rootfs-x86_64
```

This will generate `./bin/qemu-system-x86_64.img`

## Starting the Kernel

```shell
nix run .#start-x86_64
```

Now you can see your terminal loading DragonOS.

::: info
To exit the DragonOS (QEMU) environment, type `ctrl + a`, then `x`
:::


## More Nix Command Usage and Nix Script Maintenance

- `cd docs && nix run` Build documentation and start an HTTP server.
- If storage space is tight, `nix store gc` Clean up dangling historical build copies.
- In the project root directory, `nix flake show` View available build targets.
- More Nix-related user-space builds are detailed in the Userland section.

## Freezing a documentation version

Frozen docs live only in `docs/.vitepress/archives/html/<tag>/` in this repository. CI copies those directories onto the site; it does not download archives from secrets or the live site.

To publish a frozen version (for example `V0.5.0`):

1. In `docs/`, run `npm run docs:build`.
2. Copy the latest site out of `.vitepress/dist/` into `.vitepress/archives/html/V0.5.0/`, excluding `V*` and `master` (those are overlays from `docs:build`, do not copy them).
3. Prepend `"V0.5.0"` to [`.vitepress/legacy-tags.json`](../../.vitepress/legacy-tags.json).
4. Commit the new directory and the JSON. The next docs CI run will overlay that version.

`V0.4.0` and older are Sphinx snapshots: Chinese at `/V0.4.0/...`, English at `/V0.4.0/locales/en/...`. A VitePress freeze uses `/V0.5.0/zh/...` and `/V0.5.0/...`. The version switcher will need separate URL mapping for those layouts when the first VitePress freeze is added.
