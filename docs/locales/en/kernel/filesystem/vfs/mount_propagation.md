:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/filesystem/vfs/mount_propagation.md

- Translation time: 2025-11-26 17:09:56

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Mount Propagation Mechanism

## 1. Overview

Mount Propagation is an important feature introduced in Linux kernel version 2.6.15, which has been fully implemented in DragonOS. This mechanism controls whether and how mount/unmount events occurring on one mount point propagate to other related mount points.

### 1.1 Why Mount Propagation is Needed?

In containerized and namespace-isolated scenarios, different processes may have different mount namespaces (Mount Namespace). Traditional mount behavior cannot meet the following requirements:

1. **Shared Storage**: Multiple containers need to see the same storage changes
2. **Isolation**: Mount changes in some containers should not affect other containers
3. **Flexible Configuration**: Different directory trees require different propagation policies

### 1.2 Core Concepts

Mount Propagation introduces the following core concepts:

| Concept | Description |
|---------|-------------|
| **Peer Group** | A collection of mount points that share mount events |
| **Propagation Type** | Defines how a mount point participates in event propagation |
| **Bind Mount** | Binds a directory tree to another location |
| **Namespace** | The isolation boundary of mount points |

## 2. Propagation Types

DragonOS supports four propagation types, each defining different event propagation behaviors:

### 2.1 Shared

```
┌─────────────────────────────────────────────────────────┐
│                     Peer Group                          │
│  ┌─────────┐         ┌─────────┐         ┌─────────┐   │
│  │ Mount A │ ◄─────► │ Mount B │ ◄─────► │ Mount C │   │
│  │ shared  │         │ shared  │         │ shared  │   │
│  └─────────┘         └─────────┘         └─────────┘   │
│       │                   │                   │         │
│       └───────────────────┴───────────────────┘         │
│              双向传播：mount/umount 事件                 │
└─────────────────────────────────────────────────────────┘
```

**Characteristics**:
- Mount points in the same Peer Group bidirectionally propagate events
- mount/umount operations on any Peer propagate to all other Peers
- Set via `MS_SHARED` flag

**Typical Use Cases**:
- Multiple containers needing to share the same storage view
- Real-time synchronization across namespaces

### 2.2 Private

```
┌─────────────┐         ┌─────────────┐
│   Mount A   │         │   Mount B   │
│   private   │    ✗    │   private   │
│             │◄───────►│             │
└─────────────┘         └─────────────┘
      不传播任何事件
```

**Characteristics**:
- Mount events are neither sent nor received
- Each mount point is completely independent
- This is the default type for newly created mounts

**Typical Use Cases**:
- Containers requiring complete isolation
- Temporary mount points

### 2.3 Slave

```
┌─────────────┐         ┌─────────────┐
│   Master    │ ───────►│    Slave    │
│   shared    │         │    slave    │
│             │◄─ ✗ ────│             │
└─────────────┘         └─────────────┘
    单向传播：Master → Slave
```

**Characteristics**:
- Only receives events from the Master, does not propagate outward
- Can have its own local mount changes, but does not affect the Master
- The Master must be of Shared type

**Typical Use Cases**:
- Read-only shared views
- Containers needing to see host mount changes without affecting the host

### 2.4 Unbindable

```
┌─────────────┐         ┌─────────────┐
│   Mount A   │    ✗    │   Mount B   │
│ unbindable  │◄───────►│     any     │
│             │ 禁止bind │             │
└─────────────┘         └─────────────┘
```

**Characteristics**:
- Cannot be bind mounted
- Does not participate in any propagation
- The strongest level of isolation

**Typical Use Cases**:
- Sensitive data directories
- System directories that need to prevent accidental exposure

## 3. Peer Group Mechanism

### 3.1 What is a Peer Group?

A Peer Group is a collection of mount points that share mount propagation relationships. All Shared mount points within the same Peer Group bidirectionally propagate mount events.

```
                    ┌──────────────────────────────────┐
                    │        Peer Group (ID=42)        │
                    │                                  │
  Namespace A       │   ┌─────────┐                    │
  ┌─────────────────┼───│ /mnt/a  │                    │
  │                 │   │ shared  │                    │
  │                 │   └────┬────┘                    │
  │                 │        │                         │
  └─────────────────┼────────┼─────────────────────────┤
                    │        │                         │
  Namespace B       │        │                         │
  ┌─────────────────┼────────┼─────────────────────────┤
  │                 │        │                         │
  │                 │   ┌────▼────┐                    │
  │                 │   │ /mnt/b  │                    │
  │                 │   │ shared  │                    │
  │                 │   └─────────┘                    │
  └─────────────────┼──────────────────────────────────┤
                    └──────────────────────────────────┘
```

### 3.2 Formation of Peer Groups

Peer Groups are formed or expanded in the following situations:

1. **Setting Shared Type**: When a mount point is first set to Shared, a new Group ID is assigned
2. **Bind Mount**: Performing a bind mount on a Shared mount causes the new mount to join the same Peer Group
3. **Namespace Copy**: During `unshare(CLONE_NEWNS)`, Shared mounts are copied and join the same Peer Group

### 3.3 Group ID Assignment

Each Peer Group is identified by a unique Group ID:

```
Group ID 分配器
┌─────────────────────────────────────┐
│  ID Pool: [1, 2, 3, 4, 5, ...]      │
│                                     │
│  已分配: {1 → Group A, 3 → Group B} │
│  可用: {2, 4, 5, ...}               │
└─────────────────────────────────────┘
```

- Group IDs are assigned starting from 1
- 0 indicates invalid/not part of any group
- When a Peer Group is empty, the ID can be recycled

## 4. Event Propagation Process

### 4.1 Mount Event Propagation

When creating a new mount on a Shared mount point:

```
步骤 1: 在源挂载点创建子挂载
┌──────────────┐
│   /mnt/a     │ ← mount("", "/mnt/a/sub", "ramfs", ...)
│   shared     │
│      │       │
│   ┌──▼───┐   │
│   │ sub  │   │
│   └──────┘   │
└──────────────┘

步骤 2: 获取 Peer Group 成员
┌──────────────────────────────────────┐
│ Peer Group 42:                       │
│   - /mnt/a (源)                      │
│   - /mnt/b (Peer)                    │
│   - /mnt/c (Peer)                    │
└──────────────────────────────────────┘

步骤 3: 向每个 Peer 传播
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│   /mnt/a     │  │   /mnt/b     │  │   /mnt/c     │
│   shared     │  │   shared     │  │   shared     │
│      │       │  │      │       │  │      │       │
│   ┌──▼───┐   │  │   ┌──▼───┐   │  │   ┌──▼───┐   │
│   │ sub  │   │  │   │ sub' │   │  │   │ sub''│   │
│   └──────┘   │  │   └──────┘   │  │   └──────┘   │
└──────────────┘  └──────────────┘  └──────────────┘
      源               复制               复制
```

### 4.2 Umount Event Propagation

When unmounting a sub-mount on a Shared mount point:

```
步骤 1: umount("/mnt/a/sub")
┌──────────────┐
│   /mnt/a     │
│   shared     │
│      │       │
│   ┌──▼───┐   │ ← umount
│   │ sub  │   │
│   └──────┘   │
└──────────────┘

步骤 2: 传播到所有 Peer
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│   /mnt/a     │  │   /mnt/b     │  │   /mnt/c     │
│   shared     │  │   shared     │  │   shared     │
│              │  │              │  │              │
│   (empty)    │  │   (empty)    │  │   (empty)    │
│              │  │              │  │              │
└──────────────┘  └──────────────┘  └──────────────┘
  已卸载            传播卸载          传播卸载
```

### 4.3 Propagation to Slave

Slave mount points unidirectionally receive events:

```
┌──────────────┐         ┌──────────────┐
│   Master     │         │    Slave     │
│   shared     │         │    slave     │
│      │       │   ───►  │      │       │
│   ┌──▼───┐   │         │   ┌──▼───┐   │
│   │ sub  │   │         │   │ sub' │   │
│   └──────┘   │         │   └──────┘   │
└──────────────┘         └──────────────┘
                               │
                               ▼
                         Slave 上的本地挂载
                         不会传播回 Master
```

## 5. Namespace Interaction

### 5.1 Namespace Copy

When calling `unshare(CLONE_NEWNS)` to create a new mount namespace:

```
复制前（父进程的命名空间）:
┌─────────────────────────────────────┐
│ Mount Namespace (Parent)            │
│                                     │
│  /              ┌──────────┐        │
│  └── mnt/       │ shared   │        │
│      └── data   │ Group 1  │        │
│                 └──────────┘        │
└─────────────────────────────────────┘

unshare(CLONE_NEWNS) 后:
┌─────────────────────────────────────┐
│ Mount Namespace (Parent)            │
│                                     │
│  /              ┌──────────┐        │
│  └── mnt/       │ shared   │◄───────┼─┐
│      └── data   │ Group 1  │        │ │
│                 └──────────┘        │ │ Peer
└─────────────────────────────────────┘ │ 关系
                                        │
┌─────────────────────────────────────┐ │
│ Mount Namespace (Child)             │ │
│                                     │ │
│  /              ┌──────────┐        │ │
│  └── mnt/       │ shared   │◄───────┼─┘
│      └── data   │ Group 1  │        │
│                 └──────────┘        │
└─────────────────────────────────────┘
```

**Key Behaviors**:
- Private Mounts: Simple copy, no Peer relationships
- Shared Mounts: Copied and joined to the same Peer Group, establishing cross-namespace propagation
- Slave Mounts: Maintain Slave relationships
- Unbindable Mounts: Cannot be copied to new namespaces

### 5.2 Cross-Namespace Propagation Example

```
时间线:
───────────────────────────────────────────────────►

T1: 父进程创建 shared 挂载
    Parent NS: /mnt/shared (Group 1)

T2: 子进程 unshare(CLONE_NEWNS)
    Parent NS: /mnt/shared (Group 1) ◄──┐
                                        │ Peer
    Child NS:  /mnt/shared (Group 1) ◄──┘

T3: 父进程在 /mnt/shared/sub 挂载
    Parent NS: /mnt/shared/sub ←── 新挂载
              │
              ▼ 传播
    Child NS:  /mnt/shared/sub ←── 自动出现

T4: 子进程也能看到 /mnt/shared/sub
```

## 6. Propagation Type Conversion

### 6.1 State Transition Diagram

```
                    ┌───────────────┐
        MS_SHARED   │               │  MS_PRIVATE
       ┌───────────►│    SHARED     │◄───────────┐
       │            │               │            │
       │            └───────┬───────┘            │
       │                    │                    │
       │                    │ MS_SLAVE           │
       │                    ▼                    │
┌──────┴──────┐      ┌─────────────┐      ┌─────┴───────┐
│             │      │             │      │             │
│   PRIVATE   │◄─────│    SLAVE    │─────►│ UNBINDABLE  │
│             │      │             │      │             │
└─────────────┘      └─────────────┘      └─────────────┘
                     MS_PRIVATE            MS_UNBINDABLE
```

### 6.2 Conversion Rules

| Source Type | Target Type | Operation | Side Effects |
|-------------|-------------|-----------|--------------|
| Private | Shared | `mount --make-shared` | Assigns new Group ID |
| Shared | Private | `mount --make-private` | Removes from Peer Group |
| Shared | Slave | `mount --make-slave` | Becomes a receiver of the Peer Group |
| Slave | Shared | `mount --make-shared` | Disconnects from the Master |
| * | Unbindable | `mount --make-unbindable` | Clears all relationships |

## 7. Design Principles

### 7.1 Principle of Least Surprise

- New mounts default to Private, avoiding unexpected side effects
- Only explicitly setting Shared participates in propagation
- Propagation behavior is clear and predictable

### 7.2 Performance Considerations

- Peer Groups use a global registry for management, with O(1) lookup
- Propagation operations use deferred execution to avoid blocking mount operations
- Weak references prevent circular references and memory leaks

### 7.3 Consistency Guarantees

- Atomic operations and locks protect propagation states
- Propagation failures do not affect the original operation
- Supports state recovery after partial propagation

## 8. Compatibility with Linux

DragonOS's mount propagation implementation follows Linux semantics:

| Feature | Linux | DragonOS |
|---------|-------|----------|
| Shared Propagation | ✓ | ✓ |
| Private Isolation | ✓ | ✓ |
| Slave Unidirectional Propagation | ✓ | ✓ |
| Unbindable | ✓ | ✓ |
| Cross-Namespace Propagation | ✓ | ✓ |
| Recursive Propagation (MS_REC) | ✓ | ✓ |
| /proc/self/mountinfo | ✓ | Partial |

## 9. References

1. [Linux Kernel Documentation: Shared Subtrees](https://www.kernel.org/doc/Documentation/filesystems/sharedsubtree.txt)
2. [LWN.net: Shared subtrees](https://lwn.net/Articles/159077/)
