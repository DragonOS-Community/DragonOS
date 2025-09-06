# gvisor系统调用测试套件

这是DragonOS项目中用于运行gvisor系统调用测试的自动化工具。

仓库：https://cnb.cool/DragonOS-Community/test-suites


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
make test-all
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
# 或者
./run_tests.sh -l
```

## 详细使用方法

### 运行特定测试

```bash
# 运行单个测试（必须在白名单中）
./run_tests.sh socket_test

# 运行匹配模式的测试（仅白名单中的）
./run_tests.sh "*socket*"

# 禁用白名单，运行所有测试
./run_tests.sh --no-whitelist

# 详细输出模式
./run_tests.sh -v socket_test

# 设置超时时间
./run_tests.sh -t 60 socket_test

# 使用自定义白名单文件
./run_tests.sh --whitelist my_custom_whitelist.txt
```

### 使用Makefile

```bash
make help              # 显示帮助
make download          # 仅下载测试套件
make setup             # 设置环境
make test              # 运行白名单中的测试（默认）
make test-all          # 运行所有测试（包括非白名单）
make test-verbose      # 详细模式运行
make test-quick        # 快速测试（短超时）
make list              # 列出可用测试
make clean             # 清理结果文件
make distclean         # 完全清理
```

## Blocklist机制

### 什么是Blocklist

Blocklist用于屏蔽某些在当前环境下不适用或不稳定的测试子用例。这对于逐步完善系统调用支持非常有用。

### 配置Blocklist

1. 在`blocklists/`目录下创建与测试名称相同的文件
2. 每行一个要屏蔽的测试用例名称
3. 支持通配符和注释

示例blocklist文件（`blocklists/socket_test`）：
```
# 屏蔽IPv6相关测试
SocketTest.IPv6*

# 屏蔽特定的不稳定测试
SocketTest.UnstableTest
```

### 禁用白名单或Blocklist

```bash
# 禁用白名单，运行所有测试程序
./run_tests.sh --no-whitelist

# 忽略所有blocklist（但仍使用白名单）
./run_tests.sh --no-blocklist

# 同时禁用白名单和blocklist
./run_tests.sh --no-whitelist --no-blocklist
```

## 文件结构

```
user/apps/tests/syscall/gvisor/
├── download_tests.sh      # 下载脚本
├── run_tests.sh          # 测试运行脚本
├── Makefile              # Make构建文件
├── README.md             # 说明文档
├── blocklists/           # Blocklist目录
│   ├── README.md         # Blocklist说明
│   └── socket_test       # 示例blocklist
├── tests/                # 测试可执行文件（下载后生成）
└── results/              # 测试结果（运行后生成）
    ├── failed_cases.txt  # 失败用例列表
    ├── test_report.txt   # 测试报告
    └── *.output          # 各测试的详细输出
```

## 环境变量

- `SYSCALL_TEST_WORKDIR`: 测试工作目录，默认为`/tmp/gvisor_tests`
- `TEST_TIMEOUT`: 单个测试的超时时间，默认300秒

## 测试报告

测试完成后会生成：

1. **控制台输出**: 实时显示测试进度和结果
2. **测试报告**: `results/test_report.txt` - 包含统计信息和失败用例
3. **失败用例列表**: `results/failed_cases.txt` - 仅包含失败的测试名称
4. **详细输出**: `results/*.output` - 每个测试的详细输出

## 注意事项

1. **网络依赖**: 首次运行需要从网络下载测试套件
2. **存储空间**: 测试套件解压后约占用几百MB空间
3. **运行时间**: 完整测试可能需要较长时间，建议先运行部分测试
4. **权限要求**: 某些测试可能需要特定的系统权限

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

### 测试失败过多
1. 检查系统调用实现是否完整
2. 调整blocklist屏蔽已知问题
3. 检查测试环境配置

## 贡献

如果发现测试相关的问题或有改进建议，请：

1. 提交issue描述问题
2. 更新相应的blocklist文件
3. 提交patch修复脚本问题

## 许可证

本测试框架代码遵循DragonOS项目的许可证。gvisor测试套件本身遵循其原始许可证。 