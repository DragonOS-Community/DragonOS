# dunitest

DragonOS 用户态单元测试框架

## 当前行为（现状）

- 测例源码放在 `suites/<suite>/*.cc`
- 构建输出固定为 `bin/<suite>/<name>_test`
- runner 递归扫描 `bin/`，生成用例名时会去掉 `_test` 后缀  
  例如：`bin/demo/gtest_demo_test` -> `demo/gtest_demo`
- 默认超时是 60 秒，可通过 `--timeout-sec` 覆盖
- 过滤规则：`whitelist` -> `blocklist` -> `pattern`

## 快速使用

在仓库根目录执行：

```bash
make test-dunit-local
```

或在 `dunitest` 目录执行：

```bash
make run
```

## 如何添加新测例

1. 新增源码：`suites/<suite>/<case>.cc`
2. 如果有新创建的目录，在 `Makefile` 的 `SUITES` 里加入 `<suite>`
3. 执行 `make test-local

构建时将自动生成：

```text
编译测例: suites/<suite>/<case>.cc -> bin/<suite>/<case>_test
```

如果要通过白名单启用该测例，在 `whitelist.txt` 里写：

```text
<suite>/<case>
```

## Runner 参数

```text
dunitest-runner [OPTIONS]

  --bin-dir <PATH>       测试二进制目录（默认: bin）
  --timeout-sec <SEC>    单测超时秒数（默认: 60）
  --whitelist <PATH>     白名单路径（默认: whitelist.txt）
  --blocklist <PATH>     黑名单路径（默认: blocklist.txt）
  --results-dir <PATH>   报告目录（默认: results）
  --list                 仅列出测例
  --verbose              详细输出
  --pattern <PATTERN>    名称子串过滤（可多次）
```

## 安装内容

`make install` 后安装运行所需文件：

- `dunitest-runner`
- `run_tests.sh`
- `whitelist.txt`
- `bin/`
