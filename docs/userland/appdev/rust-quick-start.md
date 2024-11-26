# Rust应用开发快速入门

## 编译环境

&emsp;&emsp;DragonOS与Linux具有部分二进制兼容性，因此可以使用Linux的Rust编译器进行编译。

## 配置项目

### 从模板创建

:::{note}
该功能需要dadk 0.2.0及以上版本方能支持。旧版的请参考历史版本的DragonOS文档。
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
5. 在DragonOS的`user/dadk/config`目录下，参考模版[userapp_config.toml](https://github.com/DragonOS-Community/DADK/blob/main/dadk-config/templates/config/userapp_config.toml)，创建编译配置,安装到DragonOS的`/`目录下。 
(在dadk的编译命令选项处，请使用Makefile里面的`make install`配置进行编译、安装)
6. 编译DragonOS即可安装

### 手动配置

如果您需要移植别的库/程序到DragonOS，请参考模板内的配置。

由于DragonOS目前不支持动态链接，因此目前需要在RUSTFLAGS里面指定`-C target-feature=+crt-static -C link-arg=-no-pie`
