# Developing C/C++ Applications for DragonOS

## Compilation Environment

DragonOS has partial binary compatibility with Linux, so you can use the musl-gcc compiler from Linux. However, since DragonOS does not currently support dynamic linking, you need to add the compilation parameter `-static`.

For example, you can use the following command:
```shell
musl-gcc -static -o hello hello.c
```
to compile a hello.c file.

When porting existing programs, you may need to configure `CFLAGS`, `LDFLAGS`, and `CPPFLAGS` to ensure correct compilation. Please refer to the actual requirements.

## Configuring DADK

Please refer to: [Quick Start | DADK](https://docs.dragonos.org.cn/p/dadk/user-manual/quickstart.html)
