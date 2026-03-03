:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: userland/rootfs/dadk-rootfs-manifest.md

- Translation time: 2026-02-10 05:29:39

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

(_translated_label___dadk_rootfs_manifest_en)=
# DADK RootFS Manifest Configuration (Non-Nix)

This document explains the current RootFS configuration method for DragonOS based on DADK (corresponding to the traditional pipeline of `make run` / `make write_diskimage`), and aligns with the latest implementation in the repository.

:::{note}
If you are using the Nix RootFS build pipeline, please refer to `diskgen.md` in the same directory.
:::

## 1. Single RootFS Manifest Entry

The current single entry file is:

- `config/rootfs-manifests/<name>.toml`

Examples:

- `config/rootfs-manifests/default.toml`
- `config/rootfs-manifests/ubuntu2404.toml`

Selection via environment variable:

```bash
ROOTFS_MANIFEST=default make run-nographic
ROOTFS_MANIFEST=ubuntu2404 make run-nographic
```

Defaults to `default` if not specified.

## 2. Manifest Field Descriptions

Example (`config/rootfs-manifests/ubuntu2404.toml`):

```toml
[metadata]
name = "ubuntu2404"
arch = "x86_64"

[rootfs]
fs_type = "ext4"
size = "2G"
partition = "mbr"

[base]
image = "ubuntu:24.04"
pull_policy = "if-not-present"

[user]
config_dir = "user/dadk/config/sets/ubuntu2404"
```

Field semantics:

- `metadata.arch`: Must match the current `ARCH` (will error before build if inconsistent).
- `rootfs.fs_type`: Such as `fat32` / `ext4`.
- `rootfs.size`: Disk image size (e.g., `2G`).
- `rootfs.partition`: Partition scheme (currently commonly `mbr`).
- `base.image`: Docker base image; empty string means no base.
- `base.pull_policy`: Image pull policy.
- `user.config_dir`: User program configuration directory corresponding to the current manifest.

## 3. Package Configuration Directory Conventions

Current directory structure:

- `user/dadk/config/all/`: Stores all application `.toml`.
- `user/dadk/config/sets/<set-name>/`: Organizes package collections by scenario, typically referencing `../../all/*.toml` via soft links.

For example:

- `user/dadk/config/sets/default/`
- `user/dadk/config/sets/ubuntu2404/`

Switch the installed package collection by changing `user.config_dir` in the manifest.

## 4. Parsing and Generated Files

Automatically called before build:

- `tools/rootfs_manifest_resolve.sh`

Generates two files:

- `config/rootfs.generated.toml`
- `dadk-manifest.generated.toml`

Subsequent DADK commands uniformly execute based on `dadk-manifest.generated.toml`.

You can also manually perform a check:

```bash
ROOTFS_MANIFEST=ubuntu2404 ARCH=x86_64 make prepare_rootfs_manifest
```

## 5. Common Commands

### 5.1 Full Build and Launch

```bash
ROOTFS_MANIFEST=default make run
ROOTFS_MANIFEST=ubuntu2404 make run-nographic
```

### 5.2 Write Disk Image Only

```bash
ROOTFS_MANIFEST=ubuntu2404 SKIP_GRUB=1 make write_diskimage
```

### 5.3 Build User Space Only and Install to `bin/sysroot`

```bash
ROOTFS_MANIFEST=ubuntu2404 make user
```

## 6. Reinstallation Behavior After Manifest Changes

`user/Makefile` has implemented the following protection logic:

- When the manifest changes, it cleans the DADK output cache and reinstalls user programs.
- When the manifest tags are inconsistent, it deletes and recreates `bin/sysroot`, preventing old results from polluting new configurations.

Therefore, it is usually unnecessary to manually delete `sysroot`; simply re-execute the build command with the new `ROOTFS_MANIFEST`.
