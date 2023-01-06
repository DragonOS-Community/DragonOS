# port目录
---

本目录移植到DragonOS的应用程序。

可以包含以下类型的文件：

- 移植的patch，以及编译脚本、编译用的Dockerfile等
- 把子目录作为git仓库的submodule

## 注意

编译好libc之后，要把sysroot/usr/lib的文件，复制到$HOME/opt/dragonos-host-userspace/x86_64-dragonos/lib. 因为ld会从这里面找链接的东西。

目前由于DragonOS的libc还不完善，所以尚未能使用用户态交叉编译器来编译flex。
