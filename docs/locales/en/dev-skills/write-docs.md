:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: dev-skills/write-docs.md

- Translation time: 2025-12-26 10:37:02

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Contributing Documentation

## Using Nix

### Live Preview of Document Building

sphinx-autobuild is set as the default nix run target. To write documentation, simply:

```shell
cd docs
nix run
```

Then access port 8000.

### Building Documentation as drv (sphinx-build)

```shell
cd docs
nix build
```

### Previewing Built Documentation (python http-server)

```shell
cd docs
nix run .#release
```

## Building Documentation with pip Makefile

TODO
