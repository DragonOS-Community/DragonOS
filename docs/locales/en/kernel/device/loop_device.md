:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/device/loop_device.md

- Translation time: 2025-12-24 06:30:50

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Loop Device Architecture Design

This document outlines the architectural design rationale of the loop device subsystem in DragonOS, serving as guidance for development and future evolution.

## Problem Background

In operating system development, we frequently encounter these requirements:
- How to use an image file as a block device?
- How to dynamically create/delete virtual block devices without system reboot?

The loop device is the key component addressing these needs.

## System Architecture Positioning

The loop device plays the role of a "virtualization bridge" in DragonOS architecture:

```
用户态应用
     ↓
loop-control 接口 (字符设备)
     ↓
LoopManager (设备生命周期管理)
     ↓
LoopDevice[] (虚拟块设备数组)
     ↓
块设备层 ←→ 后端文件系统
```

The core concept of this layered design is: **separating control plane from data plane**.

## Core Design Philosophy

### 1. State-driven Device Management

We adopt a state machine approach for device lifecycle management, similar to Linux design:

```
Unbound → Bound → Rundown → Deleting
    ↓       ↓         ↓
Deleting  Unbound  Deleting
```

**Design Considerations**:
- Prevent illegal state transitions (e.g., deleting a device directly in Bound state)
- Provide clear device lifecycle semantics
- Lay foundation for future extensions (e.g., hot-plugging)

### 2. Dual Interface Strategy

Our design deliberately distinguishes between two interfaces:

**Character Control Interface** (`/dev/loop-control`):
- Manages device lifecycle
- Provides user-friendly device allocation/recycling mechanisms
- Maintains compatibility with Linux standard interfaces

**Block Device Interface** (`/dev/loopX`):
- Focuses on data read/write functionality
- Provides standard block device semantics
- Supports advanced features like offset and size limits

**Design Value**: This separation ensures control logic and data paths don't interfere, improving system maintainability.

### 3. Security First

When interacting with user space, we implement multiple security checks:

- **Parameter Boundary Checks**: All offsets and sizes must be LBA-aligned
- **Memory Safety**: Uses `UserBufferReader/Writer` for user-space data copying
- **Permission Validation**: Read-only devices reject write operations
- **State Validation**: Each operation checks if current device state permits it

## Module Collaboration Architecture

### Role of LoopManager
LoopManager isn't just a device array manager, but the subsystem's "dispatch center":

- **Device Allocation Policy**: Adopts "nearest allocation" principle, prioritizing reuse of idle devices
- **Resource Pool Management**: Pre-registers 8 devices to avoid runtime allocation overhead
- **Concurrency Safety**: All device operations are protected by locks

### Abstraction Design of LoopDevice
The core abstraction of LoopDevice is "**block device view of backend files**":

```
用户视角          内部实现
/dev/loop0  ←→   文件偏移 + 大小限制
  块0-100           文件偏移0-51200
  块101-200         文件偏移51200-102400
```

This design allows mapping any part of a file as a block device, providing great flexibility for applications like containers.

## Key Designs

### Why Choose 256 as Device Limit?
- Sufficient for most application scenarios
- Avoids resource exhaustion from unlimited growth
- Maintains compatibility with Linux's default limit

### Why Pre-register 8 Devices?
- Covers common testing scenarios (typically ≤4-5 devices)
- Reduces wait time for first-time usage
- Provides a reasonable initial working set

### Why Use SpinLock Instead of Other Locks?
- Loop device operations are mostly short-lived
- Avoids complex lock hierarchies and deadlocks
- Simplifies implementation and improves performance

## Compatibility Considerations

Our design heavily references Linux loop driver interfaces by intentional choice:

1. **User-space Software Compatibility**: Existing loop tools work without modification
2. **API Contract Consistency**: Avoids potential issues from interface differences
3. **Community Knowledge Reuse**: Developers can leverage existing loop device knowledge

## Summary

DragonOS's loop device design follows these core principles:
1. **Clear Architecture**: Separation of control and data planes
2. **State Safety**: State-machine-based device lifecycle management
3. **Interface Compatibility**: Alignment with Linux standard interfaces
4. **Extension Friendly**: Architectural space reserved for future features
5. **Comprehensive Testing**: Quality ensured through multi-level testing
