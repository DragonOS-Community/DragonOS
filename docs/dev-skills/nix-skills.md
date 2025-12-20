# Nix 技巧

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
