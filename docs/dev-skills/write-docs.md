# Contributing documentation

`docs/zh/` is the Chinese documentation and the `docs/` root is the English documentation (the site default locale). Both are authored by hand: when you change one side, update the other. There is no automatic translation.

## npm / VitePress

```shell
cd docs
npm install
npm run docs:dev
```

Open the dev server in a browser. The default homepage is English; switch to 简体中文 from the navbar.

### Build the static site

```shell
cd docs
npm run docs:build
```

Output is written to `docs/.vitepress/dist`.

### Preview the build

```shell
cd docs
npm run docs:preview
```

From the repository root you can also run `make docs` / `make clean-docs`.

## Nix

```shell
cd docs
nix run          # vitepress dev
nix build        # static build
nix run .#release  # preview the build
```
