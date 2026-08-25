# DragonOS inotify: User Semantics and Architecture

> Related issue: [DragonOS-Community/DragonOS#2151](https://github.com/DragonOS-Community/DragonOS/issues/2151)
>
> Compatibility baseline: Linux 6.6.139
>
> Status: **Implemented and maintained**

### 0. Design goals

DragonOS inotify uses Linux 6.6.139 as its compatibility baseline and follows four principles:

1. **Linux-visible semantics come first.** Syscalls, event layout, masks, errno values, ordering, and read/poll behavior should match Linux.
2. **Notification must not change the original operation.** Delivery is best effort. Queue exhaustion or notification-side memory pressure cannot turn a successful filesystem operation into a failure.
3. **Hot paths stay cheap.** The kernel exits immediately when there are no watches and, when watches exist, checks the relevant object instead of contending on a system-wide index.
4. **Every state has an owner.** Watch publication, object deletion, unmount, and queue overflow use explicit transitions so cleanup is neither duplicated nor lost.

inotify is the userspace interface; fsnotify is the kernel event-routing layer. DragonOS currently provides an inotify backend without pre-implementing fanotify, mount marks, or Linux's full SRCU/connector machinery.

### 1. The userspace model

An inotify instance is a read-only file descriptor:

```text
inotify_init1()
       │
       ▼
  inotify fd ── inotify_add_watch(path, mask) ──► wd
       │
       ├── read() returns inotify_event records
       ├── poll()/epoll() waits for a readable queue
       ├── ioctl(FIONREAD) reports currently readable bytes
       └── close() removes all watches owned by the instance
```

- The `fd` represents one independent consumer with its own queue.
- A `wd` is meaningful only within that inotify instance.
- Adding the same object again to the same instance updates the existing watch and returns the existing `wd`.
- A watch follows a **filesystem object**, not a permanently fixed pathname string. Hard-link aliases share the same object watch; child events on a directory watch carry the name that was current when the operation happened.

#### 1.1 Directory watches and direct object watches

The most important routing rule is the distinction between child events in a parent directory and events on the object itself:

| Operation | Parent-directory watch | Direct object watch |
|---|---|---|
| Create, delete, move in, or move out a child | Delivered with the child name | Not delivered as a parent event |
| Open, read, write, close, or change attributes | Delivered with the current child name | Delivered without a name |
| Move the watched object itself | Not applicable | `IN_MOVE_SELF` |
| Finally delete the watched object | Parent deletion is delivered first | `IN_DELETE_SELF`, then `IN_IGNORED` |

A directory watch therefore observes activity below the directory, while a direct file watch continues to follow the same object across rename and hard-link aliases.

### 2. Architecture

The implementation has four layers, each with one responsibility:

```text
┌──────────────────────────────────────────────────────────────┐
│ VFS and file operations                                      │
│ Produce events at committed open/I/O/metadata/namespace      │
│ boundaries                                                   │
└─────────────────────────────┬────────────────────────────────┘
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ fsnotify routing                                             │
│ Resolve parent/object targets, snapshot marks, filter masks, │
│ and manage retirement                                       │
└─────────────────────────────┬────────────────────────────────┘
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ inotify backend                                              │
│ Convert events to wd/mask/cookie/name, merge, and enqueue    │
└─────────────────────────────┬────────────────────────────────┘
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ inotify fd                                                   │
│ read / poll / epoll / FIONREAD                              │
└──────────────────────────────────────────────────────────────┘
```

| Layer | Owns | Does not own |
|---|---|---|
| VFS/File | Commit point and event ordering | Watches or userspace queues |
| fsnotify | Object identity, parent/self routing, mark lifetime | Userspace-buffer handling |
| inotify backend | ABI, queueing, merging, overflow, wakeups | Filesystem-state revalidation |
| inotify fd | Blocking/nonblocking reads and poll/epoll | Filesystem mutation |

This separation is practical rather than speculative: filesystem operations do not depend on a particular consumer, and all inotify queue rules remain in one place.

### 3. Event production

#### 3.1 Committed facts are authoritative

Ordinary events are produced when an operation reaches a stable commit point. Notification code does not perform a later metadata read to reconstruct what happened.

Namespace operations are particularly sensitive to this boundary. A filesystem returns a typed outcome such as “other hard links remain” or “the last link was removed.” Mount/VFS consumes that authoritative result instead of guessing from a post-operation `nlink`. FUSE, OverlayFS, ext4, tmpfs, ramfs, and other filesystems produce the result where they own the real state and serialization.

This avoids stale FUSE attribute-cache decisions and prevents notification bookkeeping from changing the result of an already successful syscall.

#### 3.2 Event ordering

Ordering is part of compatibility, not an accidental logging detail:

| Operation | Main event order |
|---|---|
| Create a child | Commit creation → parent `IN_CREATE` |
| Unlink a regular file | file `IN_ATTRIB` (link-count change) → parent `IN_DELETE` → object `IN_DELETE_SELF` / `IN_IGNORED` at final detach |
| Remove a directory | parent `IN_DELETE|IN_ISDIR` → final object deletion |
| Ordinary rename | `IN_MOVED_FROM` → `IN_MOVED_TO` → displaced target `IN_ATTRIB` (if any) → source `IN_MOVE_SELF` |
| `RENAME_EXCHANGE` | Two independent FROM/TO pairs; both objects receive `IN_MOVE_SELF` |
| A write removes set-id bits | `IN_ATTRIB` → `IN_MODIFY` |

`IN_MOVED_FROM` and `IN_MOVED_TO` from one move share the same nonzero cookie. Exchange rename uses two cookie pairs.

Concurrent create, remove, and rename operations in the same parent publish notifications within the parent's namespace serialization boundary, so an event for a later commit cannot overtake an earlier commit.

#### 3.3 Data and attribute events

The current implementation covers common Linux data paths:

- open and exec: `IN_OPEN`
- read/pread/readv/preadv and transfer sources: `IN_ACCESS`
- write/pwrite/writev, truncate, fallocate, and transfer destinations: `IN_MODIFY`
- chmod/chown/timestamps/xattrs, link-count changes, and write-side privilege-bit removal: `IN_ATTRIB`
- final close of an open file description: `IN_CLOSE_WRITE` or `IN_CLOSE_NOWRITE`

Even when the kernel internally chunks a large I/O operation to bound memory use, one userspace syscall produces one corresponding ACCESS/MODIFY notification. Internal I/O and files marked `FMODE_NONOTIFY` do not recursively generate events.

### 4. Object identity and lifetime

#### 4.1 Stable object identity

A mounted object is identified by:

```text
(superblock identity, inode identity, inode generation)
```

This separates equal inode numbers in different mounts and prevents delivery to a reused inode identity. Hard-link aliases within one superblock resolve to the same object state, so:

- A direct watch added through any alias observes object events through the other aliases.
- Rename does not invalidate a direct object watch.
- Parent events still use the directory and name on which the operation happened.

The watch holds a strong reference to the watched object. Lookup indexes and dispatch snapshots contain weak references, avoiding ownership cycles.

#### 4.2 Final-link deletion state machine

`unlink()` removes one directory entry; it does not necessarily delete the object. DragonOS models deletion in three stages:

```text
       remove a non-final link
Linked ─────────────────────────► Linked
   │
   │ remove the final link
   ▼
Zero-link pending
   │  final disconnected dentry/open fd detaches
   ▼
Delete committed ──► IN_DELETE_SELF ──► IN_IGNORED

Zero-link pending ── successful relink ──► Linked
```

This explains an important behavior: if a file remains open after unlink, a direct watch does not immediately receive `IN_DELETE_SELF`. It arrives when the object crosses the final dentry/open-file lifetime boundary. A valid relink before that point cancels the pending deletion.

An object-local link-mutation coordinator orders link facts. Adding a watch does not hold that coordinator across filesystem or FUSE I/O, preserving both correctness and FUSE daemon reentrancy.

#### 4.3 Watch lifetime

Each watch follows an explicit lifecycle:

```text
Allocated (inactive)
      │  group/wd/index/quota are fully prepared
      ▼
Active
      │  rm_watch / one-shot / object delete / unmount / fd close
      ▼
Retiring ── unique cleanup token ──► Retired
```

`Active` is the final publication step. Dispatch can only observe a fully initialized active watch; failed publication unwinds in reverse order. Exactly one retirer owns cleanup, so the wd, quota, object counters, and indexes are released once.

- Explicit removal, one-shot retirement, object deletion, and unmount enqueue `IN_IGNORED`.
- Closing the inotify fd removes all of that instance's watches, but no consumer remains, so it does not enqueue `IN_IGNORED`.

#### 4.4 Unmount

The first watch publication must pass superblock unmount admission. Once final unmount begins, no new watch may become active. Existing object-local snapshots receive `IN_UNMOUNT`; each watch is then retired and receives `IN_IGNORED`.

`IN_UNMOUNT` is an implicit lifecycle interest on every watch; users do not need to request it explicitly.

### 5. Dispatch, queues, and reads

#### 5.1 Dispatch and filtering

fsnotify first takes an immutable mark snapshot, releases the index lock, and then dispatches. Backend queueing, wakeups, and mark cleanup never run under the object-index lock.

Dispatch applies these rules:

- Only subscribed event bits are retained.
- Directory-child events carry a name; direct object events do not.
- Applicable directory events carry `IN_ISDIR`. For Linux compatibility, `IN_DELETE_SELF` and `IN_MOVE_SELF` do not.
- `IN_EXCL_UNLINK` suppresses path-data ACCESS/MODIFY events on disconnected paths, but not dentry-data operations such as ftruncate.
- `IN_ONESHOT` retires after the first successfully matched event. Queue-full loss still consumes it; event-allocation failure only records overflow and, like Linux, does not consume it.

#### 5.2 Merging and overflow

Consecutive unread events with the same `wd`, mask, and name are merged. `IN_IGNORED` is never merged, and the move cookie is deliberately not part of the merge comparison, matching Linux inotify.

Each instance holds at most 16384 logical queued events. If the queue is full or event allocation fails:

1. The original filesystem operation remains successful.
2. The kernel records a logical `IN_Q_OVERFLOW` boundary.
3. Userspace reads an overflow event with `wd = -1` and `cookie = 0`.

The overflow state does not require allocating another queue node, so loss can still be reported under memory pressure. Events accepted before the boundary remain before it; later accepted events remain after it.

#### 5.3 read, poll, and epoll

`inotify_event` follows the Linux ABI: a 16-byte fixed header, a NUL-terminated name, and padding to a 16-byte boundary.

- A buffer smaller than 16 bytes, or one that cannot hold the first complete event, returns `EINVAL`.
- An empty nonblocking instance returns `EAGAIN`.
- An empty blocking instance waits for an event unless interrupted by a signal.
- A read returns only whole records and never splits an event.
- Multiple readers may compete one record at a time; user-memory copies happen without holding the queue lock.
- If a userspace copy faults, the dequeued event is consumed and the Linux-specific inotify rule returns `EFAULT`.
- Transitioning from empty to readable wakes read waiters and poll/epoll observers.
- `ioctl(FIONREAD)` reports the serialized byte count of the complete logical queue, including a pending overflow record.
- An inotify fd is stream-like and non-seekable; pread/pwrite/lseek do not apply.

### 6. Performance and failure isolation

#### 6.1 Layered fast paths

Event production sits on open/read/write/close hot paths. One unrelated watch must not force all I/O through a global lock. The implementation uses layered rejection:

```text
No watch in the system? ── yes ──► return
          │ no
No watch in this superblock? ── yes ──► return
          │ no
Valid negative-interest cache? ── yes ──► return
          │ no
No direct or parent watch? ── yes ──► update cache and return
          │ no
          ▼
Clone an object-local mark snapshot and dispatch without the index lock
```

The main mechanisms are:

- **Global presence count:** when the system has no watches, the hot path performs one atomic read.
- **Per-superblock counts:** unrelated mounts do not resolve object state.
- **Directory-watch count:** parent/name are not copied when no directory is watched.
- **Object-local immutable snapshots:** mounted-object hits do not scan the system-wide fallback index.
- **Epoch-validated negative cache:** after confirming that neither object nor parent is watched, later I/O performs only an atomic validation. Watch 0↔1 transitions and rename/unlink topology changes invalidate the cache.
- **Global fallback index:** used only for anonymous or special objects without mounted object state; inode-local presence hints can still reject early.

Watch addition/removal is a cold path and may build a new immutable mark list. Event dispatch clones an `Arc` snapshot and performs backend work outside the lock. RCU, sharded registries, and packed lock-free state machines are intentionally avoided because mutexes plus immutable snapshots already express the required invariants.

#### 6.2 Memory pressure and rollback

- add-watch completes all fallible allocation and quota reservation before becoming active; failure cannot leave a half-published watch.
- Retirement completes accounting even if a compact replacement snapshot cannot be allocated; dead weak entries are cleaned lazily.
- Event-name allocation failure and queue exhaustion both become `IN_Q_OVERFLOW`.
- Namespace mutation does not allocate notification state merely to proceed, so notification bookkeeping cannot turn an otherwise successful unlink/rename into `ENOMEM`.

### 7. Userspace API and supported scope

#### 7.1 Syscalls

| Interface | Behavior |
|---|---|
| `inotify_init()` | Create a blocking instance; this legacy ABI exists on x86_64 only |
| `inotify_init1(flags)` | Supports `IN_NONBLOCK` and `IN_CLOEXEC` |
| `inotify_add_watch(fd, path, mask)` | Add or update a watch; read permission is checked |
| `inotify_rm_watch(fd, wd)` | Explicitly remove a watch |

`IN_DONT_FOLLOW`, `IN_ONLYDIR`, `IN_EXCL_UNLINK`, `IN_MASK_ADD`, `IN_MASK_CREATE`, and `IN_ONESHOT` are supported. Unknown bits, an empty mask, or `IN_MASK_ADD|IN_MASK_CREATE` return Linux-compatible errors.

#### 7.2 Events

The standard event bits are supported:

```text
IN_ACCESS       IN_MODIFY        IN_ATTRIB
IN_CLOSE_WRITE  IN_CLOSE_NOWRITE IN_OPEN
IN_MOVED_FROM   IN_MOVED_TO      IN_CREATE
IN_DELETE       IN_DELETE_SELF   IN_MOVE_SELF
IN_UNMOUNT      IN_Q_OVERFLOW    IN_IGNORED
IN_ISDIR
```

#### 7.3 Resource limits

The current limits are kernel constants and are not yet exposed through `/proc/sys/fs/inotify/*`:

| Resource | Limit |
|---|---:|
| Instances per user | 128 |
| Watches per user | 8192 |
| Queued events per instance | 16384 |

Accounting applies to the current user namespace and effective UID and to ancestor user namespaces, preventing nested namespaces from resetting the limits. Instance exhaustion returns `EMFILE`; watch exhaustion returns `ENOSPC`.

### 8. Common examples

#### 8.1 Watching a directory

```text
watch(dir, IN_CREATE | IN_OPEN | IN_MODIFY)

create dir/a  → IN_CREATE(name="a")
open dir/a    → IN_OPEN(name="a")
write dir/a   → IN_MODIFY(name="a")
```

#### 8.2 Pairing rename events

```text
dir/a ──rename──► dir/b

IN_MOVED_FROM(name="a", cookie=42)
IN_MOVED_TO  (name="b", cookie=42)
```

Userspace should pair moves by cookie instead of assuming that any adjacent move records belong to the same operation.

#### 8.3 An unlinked file retained by an fd

```text
open(file) → watch(file) → unlink(file)

parent: IN_DELETE
object: no IN_DELETE_SELF yet

close(last fd)
object: IN_DELETE_SELF → IN_IGNORED
```

If another hard link remains, removing one name only produces link-count and parent events; it does not terminate the object watch.

### 9. Current boundaries and non-goals

- inotify is the current backend. fanotify, dnotify, mount marks, and superblock marks are outside this implementation.
- Resource limits are compile-time constants; Linux inotify sysctl tuning is not yet provided.
- Mounted filesystems use object-local mark snapshots. Anonymous and some special inodes use a global fallback identity. Both expose the same inotify ABI.
- fsnotify is an asynchronous observation mechanism, not a transaction log. After `IN_Q_OVERFLOW`, userspace must rescan and rebuild its state.

### 10. Implementation map and validation

The main implementation boundaries are:

| Location | Responsibility |
|---|---|
| `kernel/src/filesystem/fsnotify/` | Event routing, object state, mark/group lifetime |
| `kernel/src/filesystem/inotify.rs` | Syscalls, queue, ABI, read/poll/epoll, quota |
| `kernel/src/filesystem/vfs/mount/mod.rs` | Mounted identity, dentry snapshots, deletion/unmount lifetime, hot-path cache |
| `kernel/src/filesystem/vfs/file.rs` | Open/read/write/close event entry points |
| `kernel/src/filesystem/vfs/syscall/` | Rename, link, xattr, and I/O-transfer commit events |
| Concrete filesystems | Authoritative link/rename/fallocate outcomes, not inotify queues |

Compatibility behavior is checked against Linux 6.6.139 `fs/notify/`, `include/linux/fsnotify.h`, `include/linux/fsnotify_backend.h`, and `include/uapi/linux/inotify.h`. DragonOS regression coverage lives in:

- `user/apps/tests/dunitest/suites/normal/inotify_events.cc`
- `user/apps/tests/dunitest/suites/normal/inotify_dir_watch.cc`
- related FUSE, OverlayFS, fallocate, and concurrent-namespace dunitests
