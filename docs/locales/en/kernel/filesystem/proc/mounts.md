:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/filesystem/proc/mounts.md

- Translation time: 2026-06-06 18:11:59

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Proc Mount Export Interface

## 1. Overview

DragonOS exports the mount namespace view to user space through procfs, with the main entry points as follows:

| Path | Type | Perspective |
|------|------|------|
| `/proc/mounts` | Symbolic link → `self/mounts` | Current reading process |
| `/proc/self/mounts` | Regular file | Current reading process |
| `/proc/[pid]/mounts` | Regular file | Target `pid` |
| `/proc/[pid]/mountinfo` | Regular file | Target `pid` |
| `/proc/[pid]/mountstats` | Regular file | Target `pid` |

Where:

- **`mounts`**: Traditional format with fewer fields, compatible with `mount(8)`, shell scripts, etc.
- **`mountinfo`**: Modern format including mount ID, parent-child relationships, propagation flags, superblock options, etc.
- **`mountstats`**: Each mount is described with a line prefix, and filesystem-specific statistics (`proc_show_mount_stats`) can be appended.

Propagation types (`shared` / `master` / `propagate_from` / `unbindable`) **only appear in the optional fields of `mountinfo`** and are not written to the regular option column of `/proc/*/mounts`.

## 2. Functions of Each Interface

### 2.1 `/proc/mounts` and `/proc/self/mounts`

`/proc/mounts` is implemented as a **symbolic link pointing to `self/mounts`** (the result of `readlink` is `self/mounts`), and after resolution, it is equivalent to reading `/proc/self/mounts`, which is the mount list of the **current reading process** within its mount namespace and `fs root`.

Each line typically contains:
- Device name (or filesystem name)
- Mount point
- Filesystem type
- Mount options (`rw` and `nosuid,nodev,...` and other per-mount options; excluding propagation flags)
- Two compatibility fields `0 0`

### 2.2 `/proc/[pid]/mounts`

The format is the same as `/proc/self/mounts`, but **when opened, it binds to the `mnt_ns` and `fs_struct.root()` of the target thread group leader**, exporting the mount view visible from the perspective of the target process.

### 2.3 `/proc/[pid]/mountinfo`

Based on `mounts`, it adds:
- Mount ID, parent mount ID
- Major device number (`major:minor`)
- Mount root (`proc_show_mountinfo_root`)
- Per-mount options and superblock options (two optional fields separated by `-`)
- Propagation tagged fields (`MountPropagation::proc_mountinfo_tags()`)
- Filesystem type name

### 2.4 `/proc/[pid]/mountstats`

Each visible mount has at least one line:

```text
device <dev> mounted on <mountpoint> with fstype <type>
```

If the underlying filesystem returns additional content through `proc_show_mount_stats`, it is appended at the end of the same line. The permissions are **0400** (readable only by the owner), different from the 0444 of `mounts` / `mountinfo`.

## 3. Kernel Source Code Layout

The mount export logic is centralized in **`kernel/src/filesystem/procfs/mount/`**, no longer using the historical single-file approach of `mount_view.rs` or scattered implementations like `procfs/mounts.rs` and `pid/mountinfo.rs`.

```
kernel/src/filesystem/procfs/mount/
├── mod.rs                 # 模块入口；导出 render API
├── collect.rs             # ProcMountEntry、collect_visible_mounts()
├── fields.rs              # MountProcFields：open 前预计算各导出字段
├── escape.rs              # proc 字段转义（空格、制表符、反斜杠等）
├── render.rs              # ProcMountRenderKind；open 渲染 + read 读缓存
├── format/
│   ├── mounts_line.rs     # /proc/*/mounts 行格式
│   ├── mountinfo_line.rs  # /proc/*/mountinfo 行格式
│   └── mountstats_line.rs # /proc/*/mountstats 行格式
└── inode/
    ├── mounts_symlink.rs  # /proc/mounts → self/mounts（MountsSymOps）
    └── pid_mount.rs       # /proc/[pid]/{mounts,mountinfo,mountstats}（MountProcFileOps）
```

**Registration locations:**
- `kernel/src/filesystem/procfs/root.rs`: Root directory entry `("mounts", MountsSymOps::new_inode)`
- `kernel/src/filesystem/procfs/pid/mod.rs`: In `PidDirOps::STATIC_ENTRIES`, a `MountProcFileOps` is registered for each of `mountinfo` / `mounts` / `mountstats` (distinguished by `ProcMountRenderKind`)

**Related but not within `procfs/mount/` dependencies:**
- `kernel/src/filesystem/vfs/mount/mod.rs`: `MountFlags::proc_rw_token()`, `proc_per_mount_options()`, `proc_super_block_options()`, `options_string()`
- `kernel/src/filesystem/vfs/mod.rs`: Filesystem trait hooks `proc_show_devname`, `proc_show_mount_options`, `proc_show_mountinfo_root`, `proc_show_mount_stats`
- `kernel/src/process/namespace/propagation.rs`: `MountPropagation::proc_mountinfo_tags()`

User-space test cases: `user/apps/tests/dunitest/suites/normal/proc_mount_exports.cc` (whitelist: `normal/proc_mount_exports`).

## 4. DragonOS Implementation Principles

### 4.1 Unified Rendering Pipeline

The three proc files share a single pipeline (`render.rs`):

1. **`open()`** (opening `MountProcFileOps::open` or accessing `/proc/self/mounts` via symlink)
   - Resolves the target: `ProcPidTarget` → thread group leader `ProcessControlBlock`
2. **`collect_visible_mounts()`** (traversal)
   - Iterates over the target `mnt_ns.mount_list()`, sorted by mount ID
   - Performs **visibility pruning** (using `visible_mountpoint`) based on the target `fs_struct.root()`
3. **`MountProcFields::from_entry()`** (snapshot)
   - Takes a snapshot of devname, fstype, various options, mountinfo root/tags, etc., for each `ProcMountEntry`
4. **Calls `format::*_line::render` based on `ProcMountRenderKind`**
5. Writes the complete text to `FilePrivateData::Procfs.data`
6. **`read_at()`** only copies from the cache via `read_cached_mount_file()` → `proc_read()`

Thus, the current model is: **a full file snapshot is generated at open time, and reads during the same open session do not re-traverse the mount tree**.

### 4.2 Target Process Perspective

- `/proc/mounts` → `self/mounts` → `mounts` under the pid directory of the current process
- `/proc/[pid]/mounts|mountinfo|mountstats` binds fixedly to the namespace and root of the corresponding thread group when opened for the target `pid`

The exported content reflects the **`mnt_ns` + `fs root` of the target process**, not the reader's own mount table (unless the reader is accessing their own `/proc/self/...`).

### 4.3 Visibility Pruning

In `collect.rs`'s `visible_mountpoint(mount_path, root_path)`:
- When the target root is `/`, the mount point path is exported as-is
- When the target is under a restricted root (e.g., chroot), only mounts within that root subtree are retained, and the displayed path is normalized to a view rooted at `/`

### 4.4 Separation of Options and Propagation Fields

| Field Source | Used For | Description |
|--------------|---------|-------------|
| `MountFlags::proc_rw_token()` | mounts / mountinfo per-mount | `ro` or `rw` |
| `MountFlags::proc_per_mount_options()` | mountinfo per-mount | `nosuid,nodev,...`, excluding `rw` |
| `MountFlags::proc_super_block_options()` + sb read-only state | mountinfo superblock section | `sync,mand,...` |
| `FileSystem::proc_show_mount_options()` | mounts line, mountinfo superblock section | Filesystem-private options |
| `MountPropagation::proc_mountinfo_tags()` | Only mountinfo tail | `shared:N`, etc., **not included in mounts** |

`mounts_line` uses pre-merged `mounts_options`; `mountinfo_line` separates per-mount and superblock options with `-`, then appends propagation tags.

### 4.5 Responsibilities of the Three Formats

- **`format/mounts_line.rs`**: Device, mount point, type, options, `0 0`
- **`format/mountinfo_line.rs`**: ID, parent, major:minor, root, mount point, options section, `-`, fstype, super options, tags
- **`format/mountstats_line.rs`**: Generic `device ... mounted on ...` prefix + optional fs stats

Filesystem differences are injected via VFS trait hooks, while procfs only handles the generic line structure and escaping.

## 5. Semantic Characteristics of Current Interfaces

### 5.1 `mounts`

High compatibility, few fields; **does not include** propagation flags. Like Linux, the current process view should be accessed via the `/proc/mounts` symlink.

### 5.2 `mountinfo`

The preferred interface for restoring mount topology and propagation attributes; per-mount and superblock options, propagation flags are displayed separately.

### 5.3 `mountstats`

- Not a mount change notification interface
- Content within the same `open()` is a snapshot; reopening `open()` shows updated mount sets and statistics
- Line format allows `device` or `no device` prefixes (determined by whether devname is empty, see test case `proc_mount_exports.cc`)

## 6. Differences from Linux Implementation

### 6.1 Overall Differences

| Dimension | Linux | DragonOS Current Implementation |
|-----------|-------|---------------------------------|
| Opening Method | `seq_file` + iterator | One-time rendering and caching at `open()` |
| Reading Method | Generated on-demand during read | Read from `FilePrivateData` cache |
| `/proc/mounts` | Symlink → `self/mounts` | Implemented (as `MountsSymOps`) |
| Perspective Binding | Target task's `mnt_ns + fs root` | Same (as `collect_visible_mounts`) |
| `mounts` / `mountinfo` poll | Supports mount namespace events | Not implemented |
| Traversal Basis | Namespace list + cursor | Sorted `mnt_ns.mount_list()` iteration |
| Code Organization | `fs/proc_namespace.c` etc. | `procfs/mount/{collect,fields,format,render,inode}` |

### 6.2 Linux's `seq_file` Semantics

Linux uses `mounts_open_common()` + `seq_file` to iterate over the mount list during reading. DragonOS chooses to concatenate the full string at open time and cache it, resulting in simpler implementation and stable results within the same fd, but not fully equivalent to Linux's iterative model.

### 6.3 `mounts` / `mountinfo` poll in `poll`

Linux can use mount namespace events to perform `_translated_label__poll`/`epoll_en` on `mounts` / `mountinfo`. DragonOS has not yet implemented namespace event numbering and wait queues, so it cannot serve as a source of mount change notifications.

### 6.4 Dynamic Nature of `mountstats` and `poll`

Linux has no special `mountstats` poll semantics; DragonOS also does not invent additional poll for `mountstats`. Statistics and topology changes are observed by reopening the file.

### 6.5 Visibility Pruning Semantics

Linux uses `seq_path_root` and others for root pruning based on path objects. DragonOS currently compares **absolute path strings** with the target `fs root`, with the same general direction but still differing in detail from Linux's path object semantics.

### 6.6 Traversal and Authoritative Data Source

Linux uses the namespace-level mount list as the authoritative data source. DragonOS retrieves the table from `MntNamespace::mount_list()` and sorts it, rather than performing DFS on a single mount tree; further alignment with Linux's iteration order and event model requires continued evolution on the `MntNamespace` side.

## 7. Current Applicable Scenarios and Recommendations

Supported:
- Reading the current process mount table via `/proc/mounts` (symlink) or `/proc/self/mounts`
- Reading `/proc/[pid]/mounts`, `mountinfo`, `mountstats` for debugging
- Container/namespace tools parsing propagation fields in `mountinfo`

Notes:
- User-space tools depending on **mount namespace `poll` notifications** are not yet compatible
- Programs strongly dependent on Linux's `seq_file` line-by-line iteration semantics may observe behavioral differences
- When modifying export logic, `procfs/mount/` and `proc_mount_exports` test cases should also be updated

## 8. Summary

DragonOS consolidates proc mount exports into **`kernel/src/filesystem/procfs/mount/`**:

- **Inode Layer**: `/proc/mounts` symlink + `/proc/[pid]/*` unified `MountProcFileOps`
- **Data Layer**: `collect` → `fields` snapshot → `format` three-line rendering
- **Option Semantics**: Propagation flags only in `mountinfo`; `MountFlags` and VFS hooks divide responsibilities for exporting

The external functional positioning is already close to Linux; the underlying implementation remains **open snapshot + string pruning**, with continued evolution in `poll`, iteration model, and path semantics.
