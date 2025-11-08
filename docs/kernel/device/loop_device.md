# Loop Device 模块与测试详解

本文档总结 `feat/loop-device` 分支中 loop 设备子系统与配套用户态测试的现状，用于 PR 审阅和后续开发。内容包括模块目标、组件设计、已完成功能、未完项、测试覆盖及下一步建议。

## 模块定位与目标

Loop 设备用于将普通文件映射为内核可识别的块设备，实现与 Linux `loop` 驱动类似的功能，适用于：

- 将镜像文件挂载到 DragonOS 的虚拟块层，便于文件系统测试或容器环境。
- 通过 `IOCTL` 动态绑定/解绑后端文件，无需内核重新加载驱动。
- 支持只读标记、偏移映射、大小限制等基础特性，为后续扩展（加密、分片）打基础。

## 组件结构概览

| 组件                | 角色           | 关键字段/方法                                                | 说明                                                         |
| :------------------ | :------------- | :----------------------------------------------------------- | :----------------------------------------------------------- |
| `LoopDevice`        | 块设备实现     | `inner: SpinLock<LoopDeviceInner>`、`block_dev_meta`、`bind_file`、`read_at_sync`、`write_at_sync` | 针对单个 loopX 设备的核心逻辑，提供块读写接口，并维护设备状态和元数据。 |
| `LoopDeviceInner`   | 设备内部状态   | `device_number`、`state`、`file_inode`、`file_size`、`offset`、`size_limit`、`flags`、`read_only` | 受自旋锁保护；记录绑定文件、状态机、映射参数和只读标志。     |
| `LoopDeviceDriver`  | 驱动入口       | `new_loop_device`                                            | 负责创建空设备、向块设备管理器注册，并作为内核对象自身具备 KObject 和 Driver 特性。 |
| `LoopManager`       | 管理器         | `devices: [Option<Arc<LoopDevice>>; 256]`、`loop_add`、`loop_remove`、`find_free_minor` | 管理最多 256 个 loop 设备，初始化时预注册 0-7，共 8 个占位设备，并提供动态分配和回收接口。 |
| `LoopControlDevice` | 字符控制接口   | `ioctl` (LOOP_CTL_ADD/REMOVE/GET_FREE)                       | 对应 `/dev/loop-control` 节点，供用户态分配、回收，撤销 loop 设备。 |
| `LoopStatus64`      | IOCTL 数据结构 | `lo_offset`、`lo_sizelimit`、`lo_flags`                      | 对标 Linux `LOOP_SET_STATUS64` 数据结构，用于用户态和内核态之间传递 loop 设备参数。 |

### 状态机 (`LoopState`)

Loop 设备以 `LoopState` 枚举管理生命周期：

| 当前状态   | 允许迁移              | 场景                                    |
| :--------- | :-------------------- | :-------------------------------------- |
| `Unbound`  | `Bound`、`Deleting`   | 空闲设备，可绑定或直接删除。            |
| `Bound`    | `Unbound`、`Rundown`  | 已关联后端文件；解绑或进入下线流程。    |
| `Rundown`  | `Deleting`、`Unbound` | 下线/清理阶段；可安全删除或回到未绑定。 |
| `Deleting` | —                     | 正在注销，拒绝再次绑定。                |

状态转换由 `LoopDeviceInner::set_state` 强制检查，防止非法跃迁，并由 `state_lock` 自旋锁保护。

### I/O 数据路径

1.  **设备绑定**：
    *   `LOOP_SET_FD` IOCTL 接收一个文件描述符，通过 `ProcessManager::current_pcb().fd_table()` 获取对应的 `File` 和 `IndexNode`。
    *   `LoopDevice::bind_file` 方法根据文件模式判断是否只读，并调用 `set_file` 将后端文件 inode 关联到 LoopDevice，同时设置 `flags` 和 `read_only` 状态。
    *   最终会调用 `recalc_effective_size` 重新计算设备可见大小。
2.  **设备参数配置**：
    *   `LOOP_SET_STATUS64` IOCTL 接收 `LoopStatus64` 结构体，用于配置 `offset`（偏移量）、`size_limit`（大小限制）和 `lo_flags`（只读等标志）。
    *   内核会进行参数校验（如偏移和限制是否 LBA_SIZE 对齐，flags 是否支持）。
    *   成功设置后，同样会调用 `recalc_effective_size` 更新设备可见容量。
3.  **块读写**：
    *   ioctl的读写操作会先传入`Gendisk`然后再通过`read`函数传递到`loopX`的`read`or `write`函数
    *   块设备的读写操作 (`BlockDevice::read_at_sync` / `write_at_sync`) 将 LBA 地址和长度转换为后端文件的字节偏移和长度。
    *   计算后的文件偏移量会加上 `LoopDeviceInner::offset`。
    *   读写操作通过后端文件 `IndexNode::read_at`/`write_at` 实现。
    *   如果设备被标记为只读 (`inner.read_only` 为 true)，`write_at_sync` 将返回 `SystemError::EROFS`。
    *   写入成功后，会再次调用 `recalc_effective_size` 确保块层容量与后端文件状态一致。

### 设备控制接口

-   **块设备 IOCTL** (针对 `/dev/loopX` 节点)：
    *   `LOOP_SET_FD`：绑定一个文件描述符到 loop 设备。
    *   `LOOP_CLR_FD`：解绑当前关联的文件，并将 loop 设备置于 `Unbound` 状态。
    *   `LOOP_SET_STATUS` / `LOOP_GET_STATUS`：设置/获取 32 位状态，目前直接委托给 `_STATUS64` 版本。
    *   `LOOP_SET_STATUS64` / `LOOP_GET_STATUS64`：设置/获取 64 位状态，包括文件偏移、大小限制和标志位。
    *   `LOOP_CHANGE_FD`： 更换后端文件描述符，同时更新只读状态。
    *   `LOOP_SET_CAPACITY`： 重新计算设备容量，通常在后端文件大小或参数改变后调用。
-   **控制字符设备 IOCTL** (针对 `/dev/loop-control` 节点)：
    *   `LOOP_CTL_ADD`：根据用户请求的 minor 号（或自动查找空闲 minor）分配一个新的 `LoopDevice`。
    *   `LOOP_CTL_REMOVE`：根据 minor 号移除一个 loop 设备，将其从块设备管理器中注销。
    *   `LOOP_CTL_GET_FREE`：查找并返回一个当前未绑定后端文件的空闲 loop 设备的 minor 号。

## 已完成的工作

### 内核部分

-   `LoopDevice` 作为虚拟块设备，实现了 `BlockDevice`、`Device`、`KObject` 和 `IndexNode` 接口，具备完整的块设备读写能力。
-   引入 `LoopState` 状态机，并通过 `SpinLock` 保护其转换，确保设备生命周期管理严谨。
-   `LoopDevice` 支持文件偏移 (`offset`) 和大小限制 (`size_limit`)，使得用户可以精确控制 loop 设备可见的后端文件区域。
-   实现了 `LO_FLAGS_READ_ONLY` 标志，在 `write_at_sync` 路径中正确阻止写入并返回 `EROFS`。
-   `LoopManager` 负责集中管理 `LoopDevice` 实例，初始化时预注册 8 个占位设备，并支持动态分配和回收多达 256 个设备。
-   `LoopControlDevice` 作为字符设备，提供了用户态与 `LoopManager` 交互的控制接口 (`/dev/loop-control`)，支持 `LOOP_CTL_ADD`、`LOOP_CTL_REMOVE` 和 `LOOP_CTL_GET_FREE` IOCTL。
-   `LOOP_SET_FD`, `LOOP_CLR_FD`, `LOOP_SET_STATUS64`, `LOOP_GET_STATUS64`, `LOOP_CHANGE_FD`, `LOOP_SET_CAPACITY` 等关键块设备 IOCTL 已实现，覆盖了 loop 设备的基本配置功能。
-   用户态和内核态之间的数据拷贝使用了 `UserBufferReader/Writer`，确保了内存访问的安全性。
-   `loop_init` 函数在系统启动时通过 `unified_init` 宏自动注册 `LoopControlDevice` 并初始化 `LoopManager`，预分配了初始的 loop 设备。
-   完善的错误处理机制：在文件绑定、状态转换、参数校验和读写操作中，会返回具体的 `SystemError`。
-   `LoopDevice` 的 `metadata()` 方法能够正确反映后端文件和 loop 设备自身的元数据，包括设备类型、大小、块数等。

### 用户态测试 (`user/apps/c_unitest/test_loop.c`)

-   **测试镜像生成**：自动创建两个测试文件 (`test_image.img`, `test_image_2.img`) 作为 loop 设备的后端存储，分别大小为 1 MiB 和 512 KiB，并在后续测试中通过 `ftruncate` 动态调整大小。
-   **设备分配与绑定**：
    *   通过 `/dev/loop-control` 接口调用 `LOOP_CTL_GET_FREE` 获取空闲 minor，然后使用 `LOOP_CTL_ADD` 分配并创建对应的 `/dev/loopX` 设备节点。
    *   对新分配的 `/dev/loopX` 设备执行 `LOOP_SET_FD`，将 `test_image.img` 文件绑定到该 loop 设备。
-   **参数配置与校验**：
    *   使用 `LOOP_SET_STATUS64` 配置 loop 设备，例如设置一个 512 字节的偏移量 (`lo_offset`) 和一个有效大小限制 (`lo_sizelimit`)。
    *   接着使用 `LOOP_GET_STATUS64` 读取设备状态，验证之前设置的参数是否正确回读。
-   **数据读写验证 (初始后端文件)**：
    *   写入 512 字节的数据到 loop 设备。
    *   验证写入的数据在原始文件对应偏移处是否一致。
    *   从 loop 设备读回数据，并验证其与写入内容的一致性。
-   **只读模式测试**：
    *   通过 `LOOP_SET_STATUS64` 设置 `LO_FLAGS_READ_ONLY` 标志，将 loop 设备切换到只读模式。
    *   尝试向只读设备写入数据，验证写入操作是否被 `EROFS` 错误拒绝。
    *   恢复设备的读写权限，确保功能正常。
-   **`LOOP_CHANGE_FD` 测试**：
    *   调用 `ioctl(loop_fd, LOOP_CHANGE_FD, backing_fd_2)` 将 loop 设备绑定的后端文件从 `test_image.img` 动态切换到 `test_image_2.img`，无需解绑和重新绑定。
    *   验证切换后，能够成功向新的后端文件 (`test_image_2.img`) 写入数据，并通过直接读取 `test_image_2.img` 校验数据一致性。
-   **`LOOP_SET_CAPACITY` 测试**：
    *   **后端文件大小调整**: 通过 `ftruncate` 动态增大 `test_image_2.img` 的大小。
    *   **触发容量重新计算**: 调用 `ioctl(loop_fd, LOOP_SET_CAPACITY, 0)` 触发 loop 设备重新评估其容量。
    *   **验证容量限制行为**:
        *   首先在 `lo_sizelimit` 非零的情况下进行测试，观察容量是否受限于 `lo_sizelimit`。
        *   随后将 `lo_sizelimit` 清零，再次调用 `LOOP_SET_CAPACITY`，验证 loop 设备能够正确反映后端文件的新增容量。
    *   **扩展区域读写验证**: 尝试向新扩展的区域写入数据，并通过直接读取后端文件校验数据是否成功写入。
-   **设备清理**：
    *   调用 `LOOP_CLR_FD` 解绑后端文件。
    *   通过 `LOOP_CTL_REMOVE` 移除 loop 设备，并验证设备节点是否不再可访问。
-   **资源清理**：测试结束后删除所有生成的测试镜像文件，确保环境干净。

## 未完成/待完善事项

### 内核侧限制

-   **文件系统连接 (`fs()`)**：`LoopDevice::fs()` 和 `LoopControlDevice::fs()` 方法目前仍为 `todo!()`。
-   **I/O 调度和工作队列**：当前所有读写操作都是同步直通的。缺少异步 I/O 队列或工作队列的实现，可能在高负载情况下影响系统响应和性能。
-   **加密类型实现**：代码中保留了加密类型常量 (`LO_CRYPT_NONE` 等)，但未有任何实际的加密/解密逻辑实现。
-   **`LoopDevice::sync()` 空实现**：`sync()` 方法目前仅返回 `Ok(())`，未实现对后端文件的实际 `flush` 或 `fsync` 操作，可能导致数据持久性问题。
-   **分区支持**：`BlockDevice::partitions()` 返回空集合。这意味着 loop 设备目前不支持解析或呈现后端镜像文件中的分区表。
-   **错误回滚不足**：在 `LoopManager::create_and_register_device_locked` 中，如果 `block_dev_manager().register` 失败，虽然返回错误，但未清除 `inner.devices[minor]` 中可能残留的 `Some`，可能导致状态不一致。
-   **内核侧单元测试**：目前缺乏针对 `LoopDevice`、`LoopManager` 和 `LoopControlDevice` 核心逻辑的独立内核单元测试或集成测试，只有一个`ctest`

### 用户态测试缺口

-   **设备节点依赖**：测试依赖 `/dev/loop-control` 与 `/dev/loopX` 节点预先存在且权限正确。若 `udev`/`devfs` 未创建节点或权限不符，测试会失败。
-   **并发与压力测试**：目前的测试集中于单个设备、单线程的场景。未验证并发绑定/解绑、多设备同时读写、极限容量或高 I/O 负载下的行为。
-   **负向测试**：缺乏对非法参数（如非 LBA_SIZE 对齐的偏移/大小限制）、读写越界、重复绑定、非文件类型后端文件等边界条件的测试。**（部分覆盖，但仍需更全面）**
-   **IOCTL 完整性测试**：虽然已覆盖 `LOOP_SET_FD`, `LOOP_CLR_FD`, `LOOP_SET_STATUS64`, `LOOP_GET_STATUS64`, `LOOP_CHANGE_FD`, `LOOP_SET_CAPACITY` 等，但针对这些 IOCTL 的所有参数组合和异常情况的测试仍可扩展。

## 运行测试的基本步骤

1. **启动 DragonOS**：确保系统启动并成功执行 `loop_init` 函数。您可以通过查看内核日志确认“Loop control device initialized.”等消息。

2. **进入用户态环境**：在 DragonOS 的 shell 中，导航到用户态测试目录。

3. **编译并运行 `test_loop`**：

   ```
   ./bin/test_loop
   ```

4. **观察输出**：如果所有测试步骤成功，您将看到类似“Read/write test PASSED.”、“只读模式下写入被正确阻止。”等成功的提示信息。任何错误都会以 `ERROR:` 标志并在日志中指明。

