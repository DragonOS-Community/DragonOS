:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/filesystem/unionfs/overlayfs.md

- Translation time: 2025-05-19 01:41:18

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# overlayfs

OverlayFS is currently the most widely used union file system, with a simple principle and convenient usage, mainly used in containers.

In Docker, OverlayFS is one of the default storage drivers. Docker creates an independent upper directory for each container, while all containers share the same lower image file. This design makes resource sharing between containers more efficient and reduces storage requirements.

## Architecture Design

OverlayFS has two layers and a virtual merged layer.

- **Lower Layer (Lower Layer)**: Usually a read-only file system. It can contain multiple layers.
- **Upper Layer (Upper Layer)**: A writable layer. All write operations are performed on this layer.
- **Merged Layer (Merged Layer)**: The logical view of the upper and lower layers is merged, and the final file system presented to the user is shown.

## Working Principle

- **Read Operation**:
    - OverlayFS will first read the file from the Upper Layer. If the file does not exist in the upper layer, it will read the content from the Lower Layer.
- **Write Operation**:
    - If a file is located in the Lower Layer and an attempt is made to write to it, the system will copy it up to the Upper Layer and then write to it in the upper layer. If the file already exists in the Upper Layer, it will be directly written to that layer.
- **Delete Operation**:
    - When deleting a file, OverlayFS creates a whiteout entry in the upper layer, which hides the file in the lower layer.

## Copy-up

- **Copy-on-Write (Write-time Copy)**
When a file in the lower layer is modified, it is copied to the upper layer (called copy-up). All subsequent modifications will be performed on the copied file in the upper layer.

## Implementation Logic

The implementation is achieved by building `ovlInode` to implement the `indexnode` trait to represent the inode of the upper or lower layer. Specific operations related to files and directories are handled accordingly.
