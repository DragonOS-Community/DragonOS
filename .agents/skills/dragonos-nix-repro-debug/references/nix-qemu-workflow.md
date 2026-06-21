# Nix / QEMU Workflow For DragonOS Debugging

这份清单把 `docs/introduction/develop_nix.md` 中和主动复现调试直接相关的步骤收拢起来，方便在 skill 触发后快速执行。

## 标准链路

在仓库根目录按这个顺序执行：

```sh
nix develop
make kernel
nix run .#rootfs-x86_64
nix run .#start-x86_64
```

含义：

- `nix develop`：进入 DragonOS 的开发环境
- `make kernel`：编译内核，默认产物在 `./bin/kernel/kernel.elf`
- `nix run .#rootfs-x86_64`：构建根文件系统镜像，产物为 `./bin/qemu-system-x86_64.img`
- `nix run .#start-x86_64`：启动 DragonOS

退出 QEMU：

- 输入 `Ctrl + A`
- 然后输入 `x`

## 调试时的执行规则

### 什么时候必须重建 rootfs

遇到这些变更时，不要只编内核：

- 修改 `user/` 下会进入镜像的程序
- 修改 init 脚本或系统配置
- 增加新的复现程序

这时需要重新执行：

```sh
nix run .#rootfs-x86_64
```

### 什么时候至少要重新编内核

只要改了 `kernel/` 下的内容，就至少重新执行：

```sh
make kernel
```

## 启动前检查

每轮启动前确认：

1. 这轮改动已经保存到工作区
2. 最新构建已经完成，没有沿用旧产物
3. 如果修改了用户态或 rootfs 内容，已经重建镜像
4. 即将启动的是新的 QEMU 实例，不是旧会话

## 取证建议

- 启动前记录本轮假设和新增插桩点
- 启动后先确认系统版本/标识符合预期
- 在系统内执行最小复现步骤，不要只看启动成功
- 记录“最后一个正常事件”和“第一个异常证据”

## 可能需要提前问用户的事项

如果你预判启动或调试链路可能需要 root 密码，应在一开始就问用户，而不是中途卡住再问。

常见场景：

- KVM 或宿主网络配置需要提权
- 需要挂载、loop、tap、bridge
- 需要访问受限系统调试接口
