# Proc Mount Export Interface

## 1. Overview

DragonOS exports the mount-namespace view to userspace through procfs. The main entry points are:

| Path | Type | Viewpoint |
|------|------|-----------|
| `/proc/mounts` | symlink → `self/mounts` | current reading process |
| `/proc/self/mounts` | regular file | current reading process |
| `/proc/[pid]/mounts` | regular file | target `pid` |
| `/proc/[pid]/mountinfo` | regular file | target `pid` |
| `/proc/[pid]/mountstats` | regular file | target `pid` |

Of these:

- **`mounts`**: traditional format with fewer fields; compatible with `mount(8)`, shell scripts, and similar tools.
- **`mountinfo`**: modern format that includes mount id, parent/child relationships, propagation tags, superblock options, and more.
- **`mountstats`**: one descriptive prefix line per mount, with optional filesystem-specific statistics appended (`proc_show_mount_stats`).

Propagation types (`shared` / `master` / `propagate_from` / `unbindable`) **appear only in the optional fields of `mountinfo`**. They are never written into the ordinary option column of `/proc/*/mounts`.

## 2. Role of Each Interface

### 2.1 `/proc/mounts` and `/proc/self/mounts`

`/proc/mounts` is implemented as a **symlink to `self/mounts`** (`readlink` returns `self/mounts`). After resolution it is equivalent to reading `/proc/self/mounts`, i.e. the mount list of the **current reading process** under its mount namespace and `fs root`.

Each line typically contains:

- device name (or filesystem name)
- mount point
- filesystem type
- mount options (`rw` and per-mount options such as `nosuid,nodev,...`; no propagation tags)
- two compatibility fields `0 0`

### 2.2 `/proc/[pid]/mounts`

The format is the same as `/proc/self/mounts`, but **open binds the target thread-group leader's** `mnt_ns` and `fs_struct.root()`, so the export is the set of mounts visible from the target process's viewpoint.

### 2.3 `/proc/[pid]/mountinfo`

On top of `mounts`, this interface adds:

- mount id and parent mount id
- major device number (`major:minor`)
- mount root (`proc_show_mountinfo_root`)
- per-mount options and superblock options (two optional-field segments separated by `-`)
- propagation tagged fields (`MountPropagation::proc_mountinfo_tags()`)
- filesystem type name

### 2.4 `/proc/[pid]/mountstats`

Each visible mount has at least one line:

```text
device <dev> mounted on <mountpoint> with fstype <type>
```

If the underlying filesystem returns extra content through `proc_show_mount_stats`, that content is appended to the same line. The permission is **0400** (owner-readable only), unlike the 0444 of `mounts` / `mountinfo`.

## 3. Kernel Source Layout

Mount-export logic is concentrated in **`kernel/src/filesystem/procfs/mount/`**. The historical single-file `mount_view.rs` and the scattered `procfs/mounts.rs` / `pid/mountinfo.rs` implementations are no longer used.

```
kernel/src/filesystem/procfs/mount/
├── mod.rs                 # module entry; export render API
├── collect.rs             # ProcMountEntry, collect_visible_mounts()
├── fields.rs              # MountProcFields: precompute export fields before open
├── escape.rs              # proc field escaping (space, tab, backslash, ...)
├── render.rs              # ProcMountRenderKind; render on open + read from cache
├── format/
│   ├── mounts_line.rs     # /proc/*/mounts line format
│   ├── mountinfo_line.rs  # /proc/*/mountinfo line format
│   └── mountstats_line.rs # /proc/*/mountstats line format
└── inode/
    ├── mounts_symlink.rs  # /proc/mounts → self/mounts (MountsSymOps)
    └── pid_mount.rs       # /proc/[pid]/{mounts,mountinfo,mountstats} (MountProcFileOps)
```

**Registration sites:**

- `kernel/src/filesystem/procfs/root.rs`: root directory entry `("mounts", MountsSymOps::new_inode)`
- `kernel/src/filesystem/procfs/pid/mod.rs`: `PidDirOps::STATIC_ENTRIES` registers one `MountProcFileOps` each for `mountinfo` / `mounts` / `mountstats` (format selected via `ProcMountRenderKind`)

**Related dependencies that live outside `procfs/mount/`:**

- `kernel/src/filesystem/vfs/mount/mod.rs`: `MountFlags::proc_rw_token()`, `proc_per_mount_options()`, `proc_super_block_options()`, `options_string()`
- `kernel/src/filesystem/vfs/mod.rs`: filesystem trait hooks `proc_show_devname`, `proc_show_mount_options`, `proc_show_mountinfo_root`, `proc_show_mount_stats`
- `kernel/src/process/namespace/propagation.rs`: `MountPropagation::proc_mountinfo_tags()`

Userspace test: `user/apps/tests/dunitest/suites/normal/proc_mount_exports.cc` (whitelist: `normal/proc_mount_exports`).

## 4. DragonOS Implementation

### 4.1 Unified Render Pipeline

The three proc files share one pipeline (`render.rs`):

1. **`open()`** (`MountProcFileOps::open`, or opening `/proc/self/mounts` via the symlink)
   - Resolve the target: `ProcPidTarget` → thread-group leader `ProcessControlBlock`
2. **`collect_visible_mounts()`** (`collect.rs`)
   - Walk the target `mnt_ns.mount_list()`, sorted by mount id
   - Apply **visibility clipping** against the target `fs_struct.root()` (`visible_mountpoint`)
3. **`MountProcFields::from_entry()`** (`fields.rs`)
   - Snapshot each `ProcMountEntry`: devname, fstype, the various options, mountinfo root/tags, and so on
4. **Call `format::*_line::render` according to `ProcMountRenderKind`**
5. Write the complete text into `FilePrivateData::Procfs.data`
6. **`read_at()`** only copies from the cache via `read_cached_mount_file()` → `proc_read()`

The current model is therefore: **the entire file snapshot is generated at open time; reads during the same open do not re-walk the mount tree**.

### 4.2 Target-Process Viewpoint

- `/proc/mounts` → `self/mounts` → the `mounts` file under the current process's pid directory
- `/proc/[pid]/mounts|mountinfo|mountstats` is bound at open time to that `pid`'s thread-group namespace and root

The exported content reflects the **target process's `mnt_ns` + `fs root`**, not the reader's own mount table (unless the reader is opening its own `/proc/self/...`).

### 4.3 Visibility Clipping

`visible_mountpoint(mount_path, root_path)` in `collect.rs`:

- When the target root is `/`, mount-point paths are exported as-is
- When the target is under a restricted root such as chroot, only mounts inside that root subtree are kept, and displayed paths are normalized to a view rooted at `/`

### 4.4 Split of Options and Propagation Fields

| Field source | Used by | Notes |
|--------------|---------|-------|
| `MountFlags::proc_rw_token()` | mounts / mountinfo per-mount | `ro` or `rw` |
| `MountFlags::proc_per_mount_options()` | mountinfo per-mount | `nosuid,nodev,...`, without `rw` |
| `MountFlags::proc_super_block_options()` + sb read-only state | mountinfo superblock segment | `sync,mand,...` |
| `FileSystem::proc_show_mount_options()` | mounts line, mountinfo superblock segment | filesystem-private options |
| `MountPropagation::proc_mountinfo_tags()` | mountinfo tail only | `shared:N` and similar; **never enter mounts** |

`mounts_line` uses the pre-merged `mounts_options`; `mountinfo_line` separates per-mount and superblock options with `-`, then appends propagation tags.

### 4.5 Responsibilities of the Three Formats

- **`format/mounts_line.rs`**: device, mount point, type, options, `0 0`
- **`format/mountinfo_line.rs`**: id, parent, major:minor, root, mount point, option segments, `-`, fstype, super options, tags
- **`format/mountstats_line.rs`**: common `device ... mounted on ...` prefix + optional fs stats

Filesystem-specific differences are injected through VFS trait hooks. procfs itself only owns the common line structure and escaping.

## 5. Semantic Characteristics of the Current Interfaces

### 5.1 `mounts`

Highly compatible, few fields; **does not include** propagation tags. As on Linux, the current-process view should be accessed through the `/proc/mounts` symlink.

### 5.2 `mountinfo`

The preferred interface for recovering mount topology and propagation attributes. Per-mount options, superblock options, and propagation tags are shown in separate columns.

### 5.3 `mountstats`

- Not a mount-change notification interface
- Content within a single `open()` is a snapshot; a new `open()` can see an updated mount set and statistics
- The line format allows a `device` or `no device` prefix (chosen by whether devname is empty; see `proc_mount_exports.cc`)

## 6. Differences from Linux

### 6.1 Overview of Differences

| Dimension | Linux | DragonOS current implementation |
|-----------|-------|--------------------------------|
| Open model | `seq_file` + iterator | one-shot render and cache at `open()` |
| Read model | generated on demand while reading | read from the `FilePrivateData` cache |
| `/proc/mounts` | symlink → `self/mounts` | implemented (`MountsSymOps`) |
| Viewpoint binding | target task's `mnt_ns + fs root` | same as Linux (`collect_visible_mounts`) |
| `mounts` / `mountinfo` poll | mount-namespace events | not implemented |
| Traversal basis | namespace list + cursor | iterate `mnt_ns.mount_list()` after sorting |
| Code organization | `fs/proc_namespace.c` and related | `procfs/mount/{collect,fields,format,render,inode}` |

### 6.2 Linux `seq_file` Semantics

Linux uses `mounts_open_common()` + `seq_file` to iterate the mount list during read. DragonOS concatenates the full string at open and caches it. The implementation is simpler and the result is stable for a given fd, but it is not fully equivalent to Linux's iterator model.

### 6.3 `poll` on `mounts` / `mountinfo`

Linux can `poll`/`epoll` `mounts` / `mountinfo` via mount-namespace events. DragonOS has not yet implemented the namespace event sequence number or wait queue, so these files cannot serve as a mount-change notification source.

### 6.4 Dynamism of `mountstats` and `poll`

Linux has no dedicated `mountstats` poll semantics; DragonOS likewise does not invent extra poll behavior for `mountstats`. Changes in statistics and topology are observed by reopening the file.

### 6.5 Visibility-Clipping Semantics

Linux uses path-object-based root clipping such as `seq_path_root`. DragonOS currently compares **absolute path strings** against the target `fs root`. The overall direction matches, but details still differ from Linux path-object semantics.

### 6.6 Traversal and Authoritative Data Source

Linux treats the namespace-level mount linked list as the authoritative source. DragonOS takes the table from `MntNamespace::mount_list()` and sorts it, rather than DFS-walking a single mount tree. Aligning Linux iteration order and the event model later will require further evolution on the `MntNamespace` side.

## 7. Current Use Cases and Recommendations

Already supported:

- Read the current process's mount table via `/proc/mounts` (symlink) or `/proc/self/mounts`
- Read `/proc/[pid]/mounts`, `mountinfo`, and `mountstats` while debugging
- Container/namespace tools can parse propagation fields in `mountinfo`

Notes:

- Userspace tools that depend on **mount-namespace `poll` notification** are not yet compatible
- Programs that strongly depend on Linux `seq_file` line-by-line iteration semantics may observe behavioral differences
- When changing export logic, update both `procfs/mount/` and the `proc_mount_exports` test

## 8. Summary

DragonOS consolidates proc mount export into **`kernel/src/filesystem/procfs/mount/`**:

- **inode layer**: `/proc/mounts` symlink + unified `MountProcFileOps` for `/proc/[pid]/*`
- **data layer**: `collect` → `fields` snapshot → `format` for the three line renderers
- **option semantics**: propagation tags appear only in `mountinfo`; `MountFlags` and VFS hooks split the export work

The external functional surface is already close to Linux. The underlying model is still **open-time snapshot + string clipping**, and work continues on `poll`, the iteration model, and path semantics.
