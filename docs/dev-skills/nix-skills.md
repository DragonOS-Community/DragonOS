# Nix 技巧

## 同一终端内避免反复指定 `nix run .#start-x86_64`

需要注意的是，仅推荐重复使用 qemu 启动脚本 `dragonos-run`，因为 rootfs 在 `nix shell` eval 完毕后生成的仅仅是最终 `rootfs.tar` 拷贝到磁盘文件的过程，而 `rootfs.tar` 需要重新调用 nix build/shell/run 命令来生成。你的所有对 nix 的修改都需要通过重新执行 nix 命令来进行 eval 与构建，因此 `nix shell .#rootfs-x86_64` 意义不大。

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
│       ├───rootfs-riscv64: app: 构建 riscv64 rootfs 镜像
│       ├───rootfs-x86_64: app: 构建 x86_64 rootfs 镜像
│       ├───start-riscv64: app: 以 riscv64 启动DragonOS
│       └───start-x86_64: app: 以 x86_64 启动DragonOS
└───packages
    └───x86_64-linux
        ├───rootfs-riscv64: package 'build-rootfs-image'
        ├───rootfs-x86_64: package 'build-rootfs-image'
        ├───start-riscv64: package 'run-dragonos'
        └───start-x86_64: package 'run-dragonos'

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
