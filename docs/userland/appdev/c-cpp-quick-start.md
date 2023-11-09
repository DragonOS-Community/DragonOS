# 为DragonOS开发C/C++应用

## 编译环境

&emsp;&emsp;DragonOS与Linux具有部分二进制兼容性，因此可以使用Linux的musl-gcc进行编译。但是由于DragonOS还不支持动态链接，
因此要增加编译参数`-static`

比如，您可以使用
```shell
musl-gcc -static -o hello hello.c
```
来编译一个hello.c文件。

在移植现有程序时，可能需要配置`CFLAGS`和`LDFLAGS`，以及`CPPFLAGS`，以便正确地编译，具体请以实际为准。

