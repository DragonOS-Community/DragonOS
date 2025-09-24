# Blocklist 目录

这个目录包含用于屏蔽特定gvisor测试子用例的blocklist文件。

## 文件格式

每个blocklist文件对应一个测试可执行文件，文件名应与测试可执行文件名相同。

例如：
- `socket_test` - 对应测试可执行文件 `socket_test`
- `pipe_test` - 对应测试可执行文件 `pipe_test`

## 内容格式

blocklist文件中每一行包含一个要屏蔽的测试用例名称：

```
# 这是注释行，会被忽略
# 屏蔽某个特定的测试用例
TestCase.SpecificTest
# 屏蔽某个测试套件下的所有测试
TestSuite.*
# 屏蔽包含特定模式的测试
*PatternName*
```

## 注意事项

- 以 `#` 开头的行会被视为注释并忽略
- 空行会被忽略
- 支持通配符模式匹配
- 测试用例名称格式通常为 `TestSuite.TestCase`

## 示例

如果要屏蔽socket_test中的某些测试，创建文件`socket_test`：

```
# 屏蔽IPv6相关的测试（暂不支持）
SocketTest.IPv6*
# 屏蔽特定的不稳定测试
SocketTest.UnstableTest
``` 