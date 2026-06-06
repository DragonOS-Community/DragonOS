:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/container/cgroup2-pr1961-review-refactor-plan-20260605.md

- Translation time: 2026-06-06 09:33:32

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# PR #_translated_label__1961_en cgroup v2 Review and Module Split Implementation Plan

## Task Scope

This plan addresses two objectives for PR #_translated_label__1961_en:

1. Addressing review comment `discussion_r3361813521` regarding the exposure of root cgroup controller files.
2. Splitting `kernel/src/filesystem/cgroup2/mod.rs` by responsibility to reduce single-file volume and align object/function responsibilities more closely with Linux cgroup v2 organization.

This implementation will not introduce non-P1 controllers, fabricate legacy v1 controllers, or disguise unimplemented resource statistics as real statistics. P1 files that can be no-op must explicitly maintain the semantics of "preserving configuration/returning zero statistics."

## Linux 6.6 Reference Findings

Referencing Linux 6.6.139:

- `kernel/cgroup/cgroup.c` uses `struct cftype` to describe cgroup files and determines their appearance in the root cgroup through `cgroup_addrm_files()` based on flags such as `CFTYPE_NOT_ON_ROOT` and `CFTYPE_ONLY_ON_ROOT`.
- The base files of the default hierarchy are declared in `cgroup_base_files[]`. Root-visible base files include `cgroup.procs`, `cgroup.controllers`, `cgroup.subtree_control`, `cgroup.stat`, etc.; `cgroup.type`, `cgroup.events`, `cgroup.freeze`, `cgroup.kill` carry `CFTYPE_NOT_ON_ROOT`.
- In `kernel/sched/core.c`'s `cpu_files[]`, controller configuration files such as `cpu.weight` and `cpu.max` all carry `CFTYPE_NOT_ON_ROOT`. The root's `cpu.stat` comes from `cgroup_base_files[]`, not from `cpu_files[]`.
- In `mm/memcontrol.c`'s `memory_files[]`, files such as `memory.current`, `memory.min`, `memory.low`, `memory.high`, `memory.max`, and `memory.events` carry `CFTYPE_NOT_ON_ROOT`; `memory.stat` does not have this flag, hence it is visible to the root.
- Files such as `kernel/cgroup/pids.c`'s `pids.max`, `pids.current`, and `pids.events` all carry `CFTYPE_NOT_ON_ROOT` and are not visible to the root.
- The actual files in the host's cgroup v2 root directory also align with the above source code: the root has `cpu.stat` and `memory.stat`, but not `cpu.max`, `cpu.weight`, `memory.max`, `memory.current`, and `pids.max`.

Therefore, the review comment's assertion that "Linux cgroup v2 root also exposes interfaces such as `cpu.max`, `cpu.weight`, `memory.max`, and `pids.max`" does not align with Linux 6.6. The correct approach is not to add these files to the root but to explicitly model the visibility of root/non-root in DragonOS and supplement tests to prevent accidental changes.

## Sub-agent Review Adoption Items

After the solution was output, a sub-agent was initiated for independent review. The points confirmed as real issues and adopted from the review comments are as follows:

- `cpu.stat` should be modeled as a cgroup base file according to Linux 6.6, not a cpu controller file; otherwise, disabling the cpu controller could mistakenly delete the non-root `cpu.stat`.
- File synchronization should be based on the complete desired file set, not just the controller file table; this way, root/non-root core files, controller files, and always-visible stat files are maintained by unified rules.
- `cgroup2_init()` is more appropriately retained in `mod.rs` to avoid the mount module taking on the responsibility of sysfs default mounting.
- `IndexNode::resize(0)` must continue to function as a no-op cgroup2 pseudo-file, maintaining compatibility with shell redirection and `O_TRUNC` write paths.
- Regression tests need to cover the visibility of root files before and after enabling root `subtree_control`, as well as the continued existence of non-root `cpu.stat` before enabling and after disabling the cpu controller.

## Current Issues in DragonOS

`kernel/src/filesystem/cgroup2/mod.rs` currently has approximately 1500 lines, mixing the following responsibilities:

- cgroup2 filesystem mount/metadata.
- inode types, directory cache, lookup/list/create/rmdir/unlink.
- cgroup file specification table, controller availability, and file visibility.
- Text format encoding/parsing.
- `cgroup.procs` migration permissions and task movement.
- `cgroup.subtree_control` parsing, validation, submission, and subdirectory file synchronization.
- Read/write operations for P1 files of cpu/memory/pids/freezer.

This leads to two issues:

- `desired_controller_specs()` uses a special case for `ROOT_CONTROLLER_FILE_SPECS` for the root, which is semantically correct but lacks a Linux-style visibility flag, making it easy to misinterpret as missing controller files.
- Unclear file responsibilities will cause `mod.rs` to continue expanding when additional cgroup v2 files are added later.

## Target Architecture

Split `kernel/src/filesystem/cgroup2/mod.rs` into directory modules:

```text
kernel/src/filesystem/cgroup2/
├── mod.rs
├── files.rs
├── inode.rs
└── mount.rs
```

### `mod.rs`

Responsibilities:

- Declare submodules.
- Store cgroup2 global constants, such as max name length, block size, available/domain controllers.
- Re-export types internally shared by the current module.
- Provide external functions:
  - `cgroup2_check_attach_permissions()`
  - `cgroup2_inode_to_node()`
- Call `register_mountable_fs!`.

### `mount.rs`

Responsibilities:

- `Cgroup2Fs`
- `Cgroup2MountData`
- `FileSystemMakerData`
- `FileSystem`
- `MountableFileSystem`
- `Cgroup2Fs::new()` and `nsdelegate()`.

Mount-related code should not know the specific controller file table and should not directly handle file read/write.

### `cgroup2_init()`

`cgroup2_init()` remains in `mod.rs`, responsible for sysfs mount point and default mounting. `mod.rs` provides `pub(super) use mount::Cgroup2Fs;` for use by `register_mountable_fs!`; internal types use `pub(super)`, without exposing `Cgroup2Fs`, `Cgroup2Inode`, `CgroupCoreFile`, `CgroupFileSpec` outside of `filesystem`.

### `files.rs`

Responsibilities:

- `CgroupCoreFile`
- `CgroupFileSpec`
- Linux-style root visibility:
  - `CgroupFileVisibility::All`
  - `CgroupFileVisibility::NotOnRoot`
- P1 file specification table:
  - base files
  - non-root core files
  - cpu files
  - memory files
  - pids files
- `available_controllers_for()`
- `desired_file_specs()` / `desired_controller_specs()`.
- Encoding/parsing tools:
  - controller list
  - `max`/integer
  - `cpu.max`
  - pids max
  - zero-stat text
- Controller P1 file read/write:
  - Configuration read/write for cpu/memory/pids/freezer.
  - Read-only statistical files return 0/default.

Design principles:

- Root file visibility must be derived from the same `CgroupFileSpec` through visibility calculation to avoid independent tables like `ROOT_CONTROLLER_FILE_SPECS` that can be easily misunderstood.
- `cpu.stat` is modeled as a base/all-cgroup file according to Linux 6.6 and not included in the cpu controller file table; non-root `cpu.stat` should still exist after disabling the cpu controller.
- P1 no-op files only save configurations or return zero statistics; comments indicate that scheduling/memory statistics are not yet integrated, and private paths, network addresses, or test machine information are not written.
- `files.rs` does not handle task migration, does not access inode parent, and does not manipulate directory cache.

### `inode.rs`

Responsibilities:

- `Cgroup2Inode`
- `Cgroup2InodeInner`
- `Cgroup2InodeKind`
- inode metadata construction.
- Directory cache maintenance:
  - stale dir prune
  - controller file sync
  - cached child sync
  - lookup/list/create/rmdir/unlink
- Implementation of `IndexNode`.
- Writes requiring inode/fs context:
  - `cgroup.procs`
  - `cgroup.subtree_control`
- Delegates read/write operations for ordinary controller files such as cpu/memory/pids/freezer to `files.rs`.

`write_at()` continues to avoid holding the inode inner lock across task migration or permission checks, retaining the current self-deadlock prevention constraints.
`IndexNode::resize(0)` must remain a no-op cgroup2 pseudo-file; the DragonOS open path will execute `resize(0)` before shell redirection or the true `write()` of Rust `File::create()`.

## Review Comment Handling Plan

1. Do not add `cpu.max`, `cpu.weight`, `memory.current`, `memory.max`, or `pids.max` to the root cgroup.
2. Remove the `ROOT_CONTROLLER_FILE_SPECS` special case and replace it with a complete desired file set:
   - Retain `cpu.stat` as a base/root-visible P1 stat file, keeping it visible to the root.
   - `memory.stat` is a memory controller file but with visibility set to `All`, making it visible to both the root and non-root with memory enabled.
   - Visibility for `cpu.weight`, `cpu.max`, memory limit/current/event, swap, and pids files is set to `NotOnRoot`.
   - `inode.rs` uses `files::desired_file_specs(cgroup)` to calculate the complete desired file set; pruning only removes files "managed by the cgroup2 file table but not in the desired set," rather than deleting solely based on `controller()`.
3. Add root visibility assertions in dunitest:
   - The root has `cpu.stat` and `memory.stat`.
   - The root does not have `cpu.max`, `cpu.weight`, `memory.current`, `memory.max`, `pids.current`, or `pids.max`.
4. Respond to the GitHub review comment by explaining the Linux 6.6 evidence and noting that the code has fixed this semantics through visibility flags and tests.

## Implementation Sequence

1. Create `files.rs`, migrating `CgroupCoreFile`, `CgroupFileSpec`, controller lists, file specifications, parsing/encoding, and read/write operations for ordinary controller files.
2. Create `mount.rs`, migrating `Cgroup2Fs`, mount data, and fs trait implementations.
3. Create `inode.rs`, migrating inode data structures, directory cache, `IndexNode` implementation, and writes for `cgroup.procs` and `subtree_control`.
4. Reduce `mod.rs` to module organization, constants, public entry functions, and registration.
5. Use visibility to uniformly select root/non-root files and remove `ROOT_CONTROLLER_FILE_SPECS`.
6. Expand dunitest root file visibility assertions.
7. `make fmt`, `make kernel`, and `make -C user/apps/tests/dunitest build-suites`.
8. Launch a DragonOS guest, run the new version of `sysfs_cgroup2_mount_test`, and manually confirm key items in the root file list.
9. Submit to the PR branch, push, and respond to review comments.

## Regression Risks and Mitigations

- Risk: Private visibility errors after module splitting causing compilation failures.
  - Mitigation: Only expose necessary internal types using `pub(super)`; do not make cgroup2 inode types public outside the filesystem.
- Risk: Accidental modification of root file visibility.
  - Mitigation: dunitest explicitly asserts that the root does not have Linux `CFTYPE_NOT_ON_ROOT` files such as `cpu.max`, `memory.max`, or `pids.max`.
- Risk: Cached subdirectory controller files are not synchronized after updating `subtree_control`.
  - Mitigation: `inode.rs` retains `sync_cached_child_controller_files()`, and tests continue to cover the disappearance of child files after disabling controllers.
- Risk: Lock order inversion.
  - Mitigation: Prohibit any path from holding an inode inner lock and then acquiring `cgroup_accounting_lock()`; `cgroup.procs` continues to enter the accounting lock after releasing the inode lock; `subtree_control` only performs submissions and subdirectory synchronizations under the accounting lock, without introducing reverse lock paths.
- Risk: Regression of the pseudo-file write path for `O_TRUNC`.
  - Mitigation: `inode.rs` retains `resize(0) -> Ok(())`, and the write helper in dunitest maintains `O_WRONLY | O_TRUNC`.
- Risk: Deadlocks caused by mixed use of locks for ordinary controller writes and `cgroup.procs` migration.
  - Mitigation: Only `cgroup.procs` and `subtree_control` in `inode.rs` perform operations requiring fs/inode context; ordinary files are delegated to `files.rs`, without holding the inode inner lock.

## Verification Plan

- Static:
  - `git diff --check`
  - Search code and PR content to ensure no private environment paths, private network addresses, or temporary test machine information are included.
- Build:
  - `make fmt`
  - `make kernel`
  - `make -C user/apps/tests/dunitest build-suites`
- Guest:
  - After refreshing the image, run `/opt/tests/dunitest/bin/normal/sysfs_cgroup2_mount_test`.
  - If the dunitest in the image is not updated, temporarily transfer and run the newly built test binary from the host, clearly documenting this.
  - Manual confirmation:
    - root: `cgroup.controllers` includes `cpu memory pids`
    - root: `cpu.stat` and `memory.stat` both exist before and after enabling `subtree_control`
    - root: `cpu.max`, `cpu.weight`, `memory.current`, `memory.max`, `pids.current`, and `pids.max` are all absent before and after enabling `subtree_control`
    - non-root shows P1 controller files after the parent-level `subtree_control` is enabled
    - non-root has `cpu.stat` both before enabling cpu and after disabling cpu, but `cpu.max` is absent
