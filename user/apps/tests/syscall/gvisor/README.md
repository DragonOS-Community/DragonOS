# gvisor系统调用测试套件

这是DragonOS项目中用于运行gvisor系统调用测试的自动化工具。

测试用例仓库：https://cnb.cool/DragonOS-Community/test-suites

## 概述

gvisor是Google开发的容器运行时沙箱，它包含了大量的系统调用兼容性测试。这些测试可以用来验证操作系统的系统调用实现是否符合Linux标准。

本测试框架默认启用**白名单模式**，只运行`whitelist.txt`文件中指定的测试程序，这样可以逐步验证DragonOS的系统调用实现。同时，每个测试程序内部的测试用例可以通过`blocklists/`目录中的黑名单文件进行过滤。

## 快速开始

### 1. 下载并运行白名单测试

```bash
# 进入gvisor测试目录
cd user/apps/tests/syscall/gvisor

# 运行白名单中的测试（默认行为，会自动下载测试套件）
make test

# 运行所有测试（包括非白名单）
make run ARGS="--no-whitelist"
```

### 2. 仅下载测试套件

```bash
make download
# 或者直接运行脚本
./download_tests.sh
```

### 3. 查看可用测试

```bash
make list
```

## 详细使用方法

### 基本使用

通过Makefile可以方便地运行测试：

```bash
# 显示所有可用命令
make help

# 构建并安装测试运行器
make

# 下载测试套件
make download

# 运行白名单中的测试（自动下载测试套件）
make test

# 列出所有可用测试
make list

# 运行测试并传递参数
make run ARGS="-v"
make run ARGS="epoll_test"
make run ARGS="-j 4 --no-whitelist"

# 清理测试结果
make clean
```

### 使用示例

```bash
# 运行特定测试
make run ARGS="epoll_test"

# 使用模式匹配运行多个测试
make run ARGS="socket*"

# 详细输出模式
make run ARGS="-v"

# 并行运行4个测试
make run ARGS="-j 4"

# 禁用白名单，运行所有测试程序
make run ARGS="--no-whitelist"

# 忽略所有blocklist文件
make run ARGS="--no-blocklist"

# 设置超时时间为60秒
make run ARGS="-t 60"

# 组合多个参数
make run ARGS="-v -j 2 epoll_test"
```

## 添加新的测试

### 添加测试程序到白名单

1. 编辑 `whitelist.txt` 文件
2. 每行添加一个测试程序名称（不带路径）
3. 支持注释（以 `#` 开头）

示例：
```text
# 核心系统调用测试
socket_test
epoll_test
fcntl_test
ioctl_test

# 文件系统测试
open_test
stat_test
mmap_test
```

### 创建blocklist过滤测试用例

对于每个测试程序，可以创建blocklist文件来屏蔽特定的测试用例：

1. 在 `blocklists/` 目录下创建与测试程序同名的文件
2. 每行添加要屏蔽的测试用例名称
3. 支持注释和空行

示例blocklist文件（`blocklists/socket_test`）：
```text
# 屏蔽IPv6相关测试（DragonOS暂不支持IPv6）
SocketTest.IPv6*
SocketTest.IPv6Socket*

# 屏蔽需要特殊权限的测试
SocketTest.PrivilegedSocket

# 屏蔽已知不稳定的测试
SocketTest.UnimplementedFeature
```

### Blocklist文件格式

- 每行一个测试用例名称或模式
- 支持通配符（`*` 匹配任意字符）
- 注释以 `#` 开头
- 空行会被忽略
- 测试用例名称通常格式为 `TestSuite.TestName`

## Blocklist机制详解

### 什么是Blocklist

Blocklist用于屏蔽某些在当前环境下不适用或不稳定的测试子用例。这对于逐步完善系统调用支持非常有用。

### Blocklist的工作原理

1. 当运行测试时，测试运行器会自动查找与测试程序同名的blocklist文件
2. 文件位于 `blocklists/` 目录下
3. 支持多个额外的blocklist目录（通过 `--extra-blocklist` 参数）
4. 匹配的测试用例会被跳过，不计入统计结果

### 示例：完整的测试配置

假设我们要添加对 `epoll_test` 的支持：

1. **添加到白名单** (`whitelist.txt`):
   ```text
   epoll_test
   ```

2. **创建blocklist** (`blocklists/epoll_test`):
   ```text
   # 屏蔽超时测试（需要更精确的时间控制）
   EpollTest.Timeout*

   # 屏蔽循环测试（可能导致死锁）
   EpollTest.CycleOfOneDisallowed
   EpollTest.CycleOfThreeDisallowed

   # 屏蔽信号竞争测试
   # UnblockWithSignal contains races. Better not to enable it.
   EpollTest.UnblockWithSignal
   ```

3. **运行测试**:
   ```bash
   # 使用Makefile运行所有白名单测试（会自动应用blocklist）
   make test

   # 只运行特定测试
   make run ARGS="epoll_test"

   # 查看详细输出
   make run ARGS="-v epoll_test"
   ```

## 文件结构

```
user/apps/tests/syscall/gvisor/
├── download_tests.sh      # 下载脚本
├── Makefile              # Make构建文件
├── README.md             # 说明文档
├── whitelist.txt         # 测试程序白名单
├── runner/               # Rust测试运行器
│   ├── Cargo.toml
│   ├── Makefile
│   └── src/
│       ├── main.rs
│       └── lib_sync.rs
├── blocklists/           # Blocklist目录
│   ├── README.md         # Blocklist说明
│   └── epoll_test        # 示例blocklist
├── tests/                # 测试可执行文件（下载后生成）
└── results/              # 测试结果（运行后生成）
    ├── failed_cases.txt  # 失败用例列表
    ├── test_report.txt   # 测试报告
    └── *.output          # 各测试的详细输出
```

## 环境变量

- `SYSCALL_TEST_WORKDIR`: 测试工作目录，默认为`/tmp/gvisor_tests`
- `TEST_TIMEOUT`: 单个测试的超时时间，默认300秒
- `RUSTFLAGS`: Rust编译器标志

## 测试报告

测试完成后会生成：

1. **控制台输出**: 实时显示测试进度和结果
2. **测试报告**: `results/test_report.txt` - 包含统计信息和失败用例
3. **失败用例列表**: `results/failed_cases.txt` - 仅包含失败的测试名称
4. **详细输出**: `results/*.output` - 每个测试的详细输出

## 开发者指南

### 编译和开发

```bash
# 构建Rust测试运行器
make build

# 安装到指定目录
make install
```

### 性能提示

- 默认串行运行测试，确保稳定性
- 如需并行测试，使用 `-j` 参数（谨慎使用）
- 合理设置超时时间，避免长时间等待
- 使用blocklist跳过已知问题的测试

## 注意事项

1. **网络依赖**: 首次运行 `make test` 时会自动下载测试套件
2. **存储空间**: 测试套件解压后约占用1.1GB空间
3. **运行时间**: 完整测试可能需要较长时间，建议先运行部分测试
4. **权限要求**: 某些测试可能需要特定的系统权限
5. **自动下载**: 运行 `make test` 或 `make list` 时会自动下载所需的测试套件

## 故障排除

### 下载失败
```bash
# 检查网络连接
wget --spider https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_20250626/gvisor-syscalls-tests.tar.xz

# 手动下载并放置
wget -O gvisor-syscalls-tests.tar.xz https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_20250626/gvisor-syscalls-tests.tar.xz
```

### MD5校验失败
```bash
# 重新下载文件
rm -f gvisor-syscalls-tests.tar.xz
./download_tests.sh
```

### 测试运行失败
1. 检查测试二进制文件是否存在
2. 确认可执行权限
3. 查看详细输出了解失败原因

### 测试失败过多
1. 检查系统调用实现是否完整
2. 调整blocklist屏蔽已知问题
3. 检查测试环境配置
4. 考虑增加超时时间

## 贡献

如果发现测试相关的问题或有改进建议，请：

1. 提交issue描述问题
2. 更新相应的blocklist文件
3. 提交patch修复脚本问题
4. 帮助完善测试覆盖

## 许可证

本测试框架代码遵循GPLv2许可证。gvisor测试套件本身遵循其原始许可证。
