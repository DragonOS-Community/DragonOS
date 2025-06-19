:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: questions/build_errors.md

- Translation time: 2025-05-22 09:22:00

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Common Issues During Build

Common issues encountered during the build of DragonOS and their solutions.

## 1. Error Indicates Missing Toolchain

### Question Detail

During the build, if you encounter an error like ``xxxx not found``, it is typically due to the absence of the necessary compiler toolchain.

### Answer

If you were previously able to compile the code, but after pulling the latest code, you encounter this error, it is likely because the upstream code has updated its requirements for the toolchain. You can try the following steps to resolve this issue:

```shell
cd tools
bash bootstrap.sh
```

Then, restart the terminal and re-run the build command.

*Note:* The ``bootstrap.sh`` script is designed to be "re-runnable". It can be executed at any time to install the latest required toolchain on your system.

## 2. Disk Image Write Failure

### Question Detail

- During the build of user programs, the content inside the disk image is inconsistent with the actual content.
- Errors related to symbolic links are reported.
- Some applications are not correctly installed into the image.

### Answer

If you encounter a disk image write failure during the build process, it could be due to insufficient disk space or permission issues. It could also be due to changes in directory attributes.

A typical example is when a folder under ``bin/sysroot/xxx`` is actually a directory, but the new version of the application expects the directory ``xxx`` to be treated as a symbolic link.

In such a case, you can first check whether there is an issue with the script used to compile your application. If you confirm that there is no problem, you can try the following steps:

- Delete the ``bin/`` directory and rebuild. This can resolve most of the issues.
