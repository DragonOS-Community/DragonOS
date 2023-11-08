# Rust应用开发快速入门

## 编译环境

&emsp;&emsp;DragonOS与Linux具有部分二进制兼容性，因此可以使用Linux的Rust编译器进行编译，但是需要进行一些配置：

您可以参考DragonOS的`tools/bootstrap.sh`中，`initialize_userland_musl_toolchain()`函数的实现，进行配置。
或者，只要运行一下bootstrap.sh就可以了。

主要是因为DragonOS还不支持动态链接，但是默认的工具链里面，包含了动态链接解释器相关的代码，因此像脚本内那样，进行替换就能运行。

## 配置项目

### 从模板创建

:::{note}
该功能需要dadk 0.1.4及以上版本方能支持
:::

1. 使用DragonOS的tools目录下的`bootstrap.sh`脚本初始化环境
2. 在终端输入`cargo install cargo-generate`
3. 在终端输入

```shell
cargo generate --git https://github.com/DragonOS-Community/Rust-App-Template
```
即可创建项目。如果您的网络较慢，请使用镜像站
```shell
cargo generate --git https://git.mirrors.dragonos.org/DragonOS-Community/Rust-App-Template
```

4. 使用`cargo run`来运行项目
5. 在DragonOS的`user/dadk/config`目录下，使用`dadk new`命令，创建编译配置,安装到DragonOS的`/`目录下。 
(在dadk的编译命令选项处，请使用Makefile里面的`make install`配置进行编译、安装)
6. 编译DragonOS即可安装

### 手动配置

如果您需要移植别的库/程序到DragonOS，请参考模板内的配置。

由于DragonOS目前不支持动态链接，因此目前需要在RUSTFLAGS里面指定`-C target-feature=+crt-static -C link-arg=-no-pie`
并且需要使用上文提到的工具链`nightly-2023-08-15-x86_64-unknown-linux_dragonos-gnu`
