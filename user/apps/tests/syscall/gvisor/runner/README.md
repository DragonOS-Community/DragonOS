# gvisor系统调用测试运行器 (Rust版本)

这是原Shell脚本 `run_tests.sh` 的Rust重写版本，用于在DragonOS上运行gvisor系统调用测试。

## 功能特点

- 🚀 使用Rust编写，性能更好，错误处理更完善
- 📋 支持白名单和黑名单模式过滤测试
- ⏱️ 可配置的测试超时时间
- 📊 详细的测试报告和统计信息
- 🎨 彩色输出，易于识别测试结果
- 📁 自动创建测试目录和结果文件

## 构建和安装

```bash
# 进入runner目录
cd user/apps/tests/syscall/gvisor/runner

# 构建项目
cargo build --release

# 二进制文件位于 target/release/runner
```

## 使用方法

### 基本用法

```bash
# 运行白名单中的测试程序
./target/release/runner

# 列出所有可用的测试用例
./target/release/runner --list

# 详细模式运行测试
./target/release/runner --verbose

# 设置测试超时为180秒
./target/release/runner --timeout 180
```

### 高级选项

```bash
# 禁用白名单，运行所有测试
./target/release/runner --no-whitelist

# 禁用黑名单过滤
./target/release/runner --no-blocklist

# 使用自定义白名单文件
./target/release/runner --whitelist my_whitelist.txt

# 指定额外的黑名单目录
./target/release/runner --extra-blocklist extra_blocks

# 运行特定的测试程序
./target/release/runner socket_test chdir_test

# 详细帮助
./target/release/runner --help
```

## 配置文件

### 白名单文件 (`whitelist.txt`)

```text
# gvisor测试程序白名单
# 每行一个测试程序名称，只有在此列表中的测试程序才会被执行
# 以#开头的行为注释，空行会被忽略

# 基础系统调用测试
chdir_test
read_test

# 文件系统相关测试  
# stat_test  # 被注释掉的测试不会运行
```

### 黑名单文件 (`blocklists/测试名称`)

每个测试程序都可以有对应的黑名单文件，用于屏蔽特定的子测试：

```text
# epoll_test黑名单文件 (blocklists/epoll_test)
# 屏蔽的子测试名称，每行一个
EpollTest.Timeout_NoRandomSave  
EpollTest.CycleOfOneDisallowed
EpollTest.CycleOfThreeDisallowed
```

## 目录结构

```
runner/
├── Cargo.toml          # Rust项目配置
├── src/
│   ├── main.rs         # 主程序入口
│   └── lib_sync.rs     # 同步版本的核心库
├── target/             # 编译输出目录
│   └── release/
│       └── runner      # 最终可执行文件
└── README.md          # 本文件
```

## 输出文件

程序运行时会在 `results/` 目录下生成以下文件：

- `test_report.txt` - 完整的测试报告
- `failed_cases.txt` - 失败的测试用例列表  
- `[测试名称].output` - 每个测试的详细输出

## 环境变量

- `SYSCALL_TEST_WORKDIR` - 临时工作目录路径（默认：`/tmp/gvisor_tests`）

## 与Shell版本的区别

1. **性能**: Rust版本启动更快，内存使用更少
2. **错误处理**: 更好的错误信息和异常处理
3. **并发**: 为未来的并行测试执行准备了基础架构
4. **可维护性**: 类型安全，更容易扩展和维护
5. **依赖**: 减少了对系统命令的依赖

## 故障排除

### 常见问题

1. **测试套件未找到**
   ```
   错误: 测试目录不存在
   ```
   解决: 先运行 `./download_tests.sh` 下载测试套件

2. **权限问题**
   ```
   错误: 测试不存在或不可执行
   ```
   解决: 确保测试文件有执行权限 `chmod +x tests/*_test`

3. **超时问题**
   ```
   错误: 测试超时
   ```
   解决: 使用 `--timeout` 参数增加超时时间

### 调试模式

使用 `--verbose` 参数获得详细的调试信息：

```bash
./target/release/runner --verbose --timeout 600 socket_test
```

## 开发说明

如果需要修改或扩展功能：

1. 主要逻辑在 `src/lib_sync.rs` 中
2. 命令行参数处理在 `src/main.rs` 中
3. 使用 `cargo test` 运行单元测试（如果有）
4. 使用 `cargo fmt` 格式化代码
5. 使用 `cargo clippy` 进行代码检查

## 许可证

与DragonOS项目保持一致的许可证。