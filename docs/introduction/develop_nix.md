# Developing DragonOS with Nix

The introduction of Nix eliminates the need for manually maintained `bootstrap.sh` in DragonOS's development environment. Now, any Linux distribution can quickly build and run DragonOS by installing the Nix environment!

## Installing Nix and Enabling Flake Support

Refer to https://nixos.org/download/ to install Nix: The Nix package manager. (Not NixOS!)

Refer to https://wiki.nixos.org/wiki/Flakes#Setup to enable flake support.

- If you want to experience Nix's declarative management without changing your distribution, try home-manager and configure it to enable flakes and direnv.
- Otherwise, you can directly install flakes in a standalone Nix manner, or add `--experimental-features 'nix-command flakes'` before each command.

## Allowing the Current User to Use KVM

The Nix launch scripts do not use `sudo`; QEMU runs as the current user, so the current user needs access to `/dev/kvm` (and optionally `/dev/vhost-vsock`). Usually it is enough to join the `kvm` group and log in again:

```shell
sudo usermod -aG kvm $USER
```

For the vsock device, the `vhost_vsock` kernel module must also be loaded. If `/dev/kvm` is unavailable, the launch script prints a hint and falls back to TCG software emulation (slower).

## Domestic Mirror Acceleration (Recommended)

If you are in China and do not have a global proxy, the first dependency pull may be slow or even fail. It is recommended to append the following content to your user-level configuration (it will not overwrite your existing configuration):

```shell
mkdir -p ~/.config/nix
cat >> ~/.config/nix/nix.conf <<'EOF'
# DragonOS Nix mirror (CN)
extra-substituters = https://mirrors.tuna.tsinghua.edu.cn/nix-channels/store https://mirrors.ustc.edu.cn/nix-channels/store
EOF
```

::: info
With a multi-user installation (nix-daemon), `extra-substituters` set by a non-trusted user are ignored. In that case configure them in `/etc/nix/nix.conf` (`nix.settings.substituters` on NixOS), or add the current user to `trusted-users`.
:::

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

The disk image is built and cached in the Nix store; this command only copies it to `./bin/disk-image-x86_64.img` when the image has changed.

For interactive runs, if nothing under `user/` has changed, the copy is skipped so that changes you made to the disk inside DragonOS are kept. To discard them and restore a clean image, run `nix run .#rootfs-x86_64 -- --force`. When image preparation selects `AUTO_TEST=dunit` or `AUTO_TEST=syscall` (including `make test-dunit` and `make test-syscall`), it always restores the clean image and discards guest disk changes.

## Starting the Kernel

```shell
nix run .#start-x86_64
```

Now you can see your terminal loading DragonOS.

::: info
To exit the DragonOS (QEMU) environment, type `ctrl + a`, then `x`
:::

Environment variables that adjust the launch:

- `QEMU_ACCEL=kvm|tcg`: force the accelerator
- `AUTO_TEST=syscall|dunit`: run the corresponding tests automatically after boot
- `QEMU_GDB_WAIT=1`: with `nix run .#start-debug-x86_64`, wait for a GDB connection (`nix run .#gdb-x86_64`) before booting

## Build and Run in One Step

```shell
nix run .#yolo-x86_64
```

Runs `make kernel`, prepares the rootfs image and starts QEMU. The kernel is built with exactly the same toolchain as `nix develop`, so entering the dev shell first is not required.

## More Nix Command Usage and Nix Script Maintenance

- `cd docs && nix run` Build documentation and start an HTTP server.
- If storage space is tight, `nix store gc` Clean up dangling historical build copies.
- To keep the dev environment from being garbage-collected, use direnv (`use flake` keeps it automatically) or `nix develop --profile .nix-gc-root`.
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
