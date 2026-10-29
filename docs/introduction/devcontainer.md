# Developing DragonOS with devcontainer

This tutorial uses VSCode as an example and requires a Linux system with Docker installed.

## Clone the Repository

```shell
git clone https://github.com/DragonOS-Community/DragonOS.git
code DragonOS
```

## Enter the devcontainer Environment

A popup will appear in the bottom right corner of VSCode. Select `Reopen in Container`. If it's not visible, follow these steps to enter:
- Download the devcontainer plugin
- `ctrl+shift+p` Open the VSCode command palette
- Type `devcontainer`, and you'll see an option for `Reopen in Container`. Click it to build the devcontainer environment

The build may take some time, especially as the msr plugin is prone to installation failures under poor network conditions.

## Build DragonOS!

Simply enter

```shell
make run-nographic
```

Wait for the build to complete, and you will automatically enter the DragonOS QEMU environment.

To exit the QEMU environment, type `ctrl+a` and then press `x`.
