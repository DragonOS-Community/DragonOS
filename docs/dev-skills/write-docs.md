# 贡献文档

## 使用 nix

### 实时构建预览文档

sphinx-autobuild 作为默认 nix run 目标，写文档只需要

```shell
cd docs
nix run
```

然后访问 8000 端口即可。

### 构建文档为 drv (sphinx-build)

```shell
cd docs
nix build
```

### 预览构建好的文档 (python http-server)

```shell
cd docs
nix run .#release
```

## 使用 pip Makefile 构建文档

TODO
