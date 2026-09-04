# 贡献文档

`docs/zh/` 是中文正式文档，`docs/` 根目录是英文正式文档（站点默认语言）。两边都手写维护：改了一边应同步另一边，不再自动翻译。

## 使用 npm / VitePress

在 `docs/` 下安装依赖后：

```shell
cd docs
npm install
npm run docs:dev
```

浏览器访问开发服务器即可实时预览。默认打开英文首页，可通过导航切换到简体中文。

### 构建静态站点

```shell
cd docs
npm run docs:build
```

产物在 `docs/.vitepress/dist`。

### 预览构建产物

```shell
cd docs
npm run docs:preview
```

也可以在仓库根目录执行 `make docs` / `make clean-docs`。

## 使用 Nix

```shell
cd docs
nix run          # vitepress dev
nix build        # 构建静态站点
nix run .#release  # 预览构建产物
```
