# Quick Start

本指南旨在教您快速上手DADK。

DADK是一个用于管理DragonOS的应用编译打包的工具。您可以通过声明配置文件的方式，
把您的应用程序打包到DragonOS中。

## 安装

目前，DADK版本与DragonOS的版本是绑定的，一般来说，DragonOS在编译的时候就会帮您安装正确版本的dadk。

如果您需要手动安装，可以在dragonos的`user/Makefile`中找到匹配的dadk版本。


```
MIN_DADK_VERSION = 0.6.1
```

::: warning 注意版本兼容性
请确保您使用的 dadk 版本与 DragonOS 当前分支要求一致（当前推荐 `v0.6.1`）。
:::

您可以通过以下命令安装dadk:
```shell
cargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag <版本号> --locked
```

比如，对于0.6.1版本，您可以使用以下命令安装: `(注意版本号前面有个v)`
```shell
cargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v0.6.1 --locked
```

## RootFS（ext4 + Docker base）

从 `v0.6.0` 开始，DADK 支持：

- `rootfs` 使用 `ext4`
- 从 Docker 镜像导入 base rootfs
- base 变更自动重建镜像

详细设计与用法请参考：[RootFS 基础镜像](./rootfs-base.md)

## 打包你的第一个应用

在安装完成后，您可以使用dadk来打包您的第一个应用。

使用以下命令进入到DragonOS仓库的`user/apps/`目录：

```shell
cd DragonOS/user/apps/
```

### 编写代码

然后，为你的应用创建一个目录：
```shell
mkdir myapp
cd myapp
```

接下来，在该目录下创建一个`main.c`文件：

```shell
touch main.c
```

并向`main.c`写入以下内容：

```c

#include <stdio.h>

int main()
{
    printf("Hello World!\n");
    return 0;
}

```

然后，在该目录下创建一个`Makefile`文件：

```shell
touch Makefile
```

并向`Makefile`写入以下内容：
```Makefile
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
else ifeq ($(ARCH), loongarch64)
	CROSS_COMPILE=loongarch64-unknown-linux-gnu-
endif

CC=$(CROSS_COMPILE)gcc

.PHONY: all
all: main.c
	$(CC) -static -o helloworld main.c

.PHONY: install clean
install: all
	mv helloworld $(DADK_CURRENT_BUILD_DIR)/helloworld

clean:
	rm helloworld *.o

```

### 编写dadk用户程序配置文件

最后，在DragonOS仓库的`user/dadk/config`目录下创建一个`myapp.toml`文件：

```shell
cd DragonOS/user/dadk/config
touch myapp.toml
```

在`myapp.toml`文件中写入以下内容，用于描述你的应用的构建方式：

```toml
# 用户程序名称
name = "helloworld"
# 版本号
version = "0.1.0"
# 用户程序描述信息
description = "一个用来测试helloworld的app"

# （可选）默认: false 是否只构建一次，如果为true，DADK会在构建成功后，将构建结果缓存起来，下次构建时，直接使用缓存的构建结果
build-once = false
#  (可选) 默认: false 是否只安装一次，如果为true，DADK会在安装成功后，不再重复安装
install-once = false
# 目标架构
# 可选值："x86_64", "aarch64", "riscv64", "loongarch64"
target-arch = ["x86_64"]

# 任务源
[task-source]
# 构建类型
# 可选值："build-from_source", "install-from-prebuilt"
type = "build-from-source"
# 构建来源
# "build_from_source" 可选值："git", "local", "archive"
# "install_from_prebuilt" 可选值："local", "archive"
source = "local"
# 路径或URL
source-path = "user/apps/helloworld"

# 把压缩包中的哪个目录作为根目录（可选）,仅当 source = "archive" 时生效
# archive-rootdir = ""

# 构建相关信息
[build]
# （可选）构建命令
build-command = "make install"

# 安装相关信息
[install]
# （可选）安装到DragonOS的路径
in-dragonos-path = "/bin"

# clean相关信息
[clean]
# （可选）清除命令
clean-command = "make clean"
```
上面这就是一个简单的dadk应用的配置文件。完整的模版请见：[userapp_config.toml](https://github.com/DragonOS-Community/DADK/blob/main/dadk-config/templates/config/userapp_config.toml)

### 运行DragonOS

在完成上述步骤后，您可以使用以下命令来运行DragonOS：

```shell
make run
```

更详细的运行命令请参考：[构建DragonOS](https://docs.dragonos.org.cn/introduction/build_system.html#build-system-command)

### 测试你的应用

在DragonOS启动后，您可以在终端中输入以下命令来测试您的应用：

```shell
cd /bin
./helloworld
```

如果一切正常，您应该会看到以下输出：

```text
Hello World!
```
