# Nix Tips

## Avoid Repeatedly Specifying `nix run .#start-x86_64` in the Same Terminal

Note that it is only recommended to reuse the qemu startup script `dragonos-run`, because `dragonos-rootfs` only copies the disk image that was already built at eval time into `bin/`. Changes under `user/` require re-invoking nix build/shell/run to re-evaluate and rebuild, so `nix shell .#rootfs-x86_64` is not very meaningful.

```shell
❯ nix shell .#start-x86_64 .#rootfs-x86_64
❯ which dragonos-rootfs
/nix/store/rpsb6f76hxzcfblghfgch2jl8413w6m4-dragonos-rootfs/bin/dragonos-rootfs
❯ which dragonos-run
/nix/store/1sdnhsjihj2h3mkdx7x0v8zy9ldfijml-dragonos-run/bin/dragonos-run
❯ dragonos-run
# DragonOS QEMU Start
❯ make kernel # 重新构建内核
❯ dragonos-run # 不涉及启动脚本和rootfs的更改，直接qemu启动
```

## View Available Build Artifacts

```shell
❯ nix flake show
warning: Git tree '/workspace' is dirty
git+file:///workspace
├───apps
│   └───x86_64-linux
│       ├───gdb-x86_64: app: Connect GDB to running DragonOS (x86_64)
│       ├───rootfs-x86_64: app: 构建 x86_64 rootfs 镜像
│       ├───start-debug-x86_64: app: 以调试模式启动DragonOS (开启GDB stub, x86_64)
│       ├───start-system-x86_64: app: 以系统 QEMU 启动DragonOS (x86_64)
│       ├───start-x86_64: app: 以 x86_64 启动DragonOS
│       ├───yolo-x86_64: app: 一键化构建并启动DragonOS (x86_64)
│       └───... (same for riscv64)
├───checks
│   └───x86_64-linux
│       ├───rootfs-image-x86_64: derivation 'check-rootfs-image-x86_64'
│       └───treefmt: derivation 'treefmt-check'
├───devShells
│   └───x86_64-linux
│       └───default: development environment 'nix-shell'
├───formatter
│   └───x86_64-linux: package 'treefmt'
└───packages
    └───x86_64-linux
        ├───rootfs-x86_64: package 'dragonos-rootfs'
        ├───start-x86_64: package 'dragonos-run'
        ├───yolo-x86_64: package 'dragonos-yolo'
        └───...

user/apps/about ❯ nix flake show
git+file:///workspace?dir=user/apps/about
└───packages
    ├───aarch64-darwin
    │   └───default omitted (use '--all-systems' to show)
    ├───aarch64-linux
    │   └───default omitted (use '--all-systems' to show)
    ├───x86_64-darwin
    │   └───default omitted (use '--all-systems' to show)
    └───x86_64-linux
        └───default: package 'about-static-x86_64-unknown-linux-musl-0.1.0'

tests/syscall/gvisor ❯ nix flake show
git+file:///workspace?dir=user/apps/tests/syscall/gvisor
└───packages
    └───x86_64-linux
        └───default: package 'gvisor-tests'
```

## Recycle Nix Build History Cache

```shell
❯ nix store gc
```
