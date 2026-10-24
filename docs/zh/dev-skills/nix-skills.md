# Nix 技巧

## 同一终端内避免反复指定 `nix run .#start-x86_64`

需要注意的是，仅推荐重复使用 qemu 启动脚本 `dragonos-run`，因为 `dragonos-rootfs` 只是把 eval 时已经构建好的磁盘镜像拷贝到 `bin/`，`user/` 的改动需要重新调用 nix build/shell/run 命令来 eval 与构建，因此 `nix shell .#rootfs-x86_64` 意义不大。

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

## 查看可用构建产物

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
│       └───...（riscv64 同上）
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

## 回收 nix 构建历史缓存

```shell
❯ nix store gc
```
