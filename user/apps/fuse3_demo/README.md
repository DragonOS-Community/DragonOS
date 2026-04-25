# fuse3_demo（基于 libfuse3 的 DragonOS FUSE 演示）

`fuse3_demo` 是基于 `libfuse3` 的最小可运行 demo，用于推进 `TODO_FUSE_FULL_PLAN_CN.md` 的 P4（生态兼容）目标。

## 设计目标

- 自动下载 `libfuse3` 源码并本地构建静态库
- `fuse3_demo` 二进制静态链接 `libfuse3`
- 提供一个可回归的集成测试 `test_fuse3_demo`

## 构建

在项目根目录下：

```bash
make -C user/apps/fuse3_demo -j8
```

默认行为：

1. 自动下载 `fuse-3.18.1.tar.gz`
2. 使用 meson/ninja 构建 `libfuse3.a`
3. 静态链接生成：
   - `fuse3_demo`
   - `test_fuse3_demo`

可选变量：

- `LIBFUSE_VERSION`：指定 libfuse 版本（默认 `3.18.1`）
- `LIBFUSE_URL_PRIMARY`：主下载地址
- `LIBFUSE_URL_MIRROR`：镜像下载地址
- `LIBFUSE_ARCHIVE`：指定本地 tarball（离线构建时可用）
- `LIBFUSE_MESON_JOBS`：libfuse 编译并行度（默认 `1`，避免 jobserver 兼容问题）

## 运行 demo

```bash
mkdir -p /tmp/fuse3_mnt
fuse3_demo /tmp/fuse3_mnt --single
```

默认会创建临时 backing 目录，并在挂载点导出 `hello.txt`。

## 运行测试

```bash
test_fuse3_demo
```

测试会：

1. 启动 `fuse3_demo`
2. 校验 `hello.txt` 读取
3. 校验创建/写入/重命名/删除文件
4. 发送信号停止 daemon 并清理挂载点
