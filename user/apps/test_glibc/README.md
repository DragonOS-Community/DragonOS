# DragonOS Rust-Application Template

您可以使用此模板来创建DragonOS应用程序。

## 使用方法

1. 使用DragonOS的tools目录下的`bootstrap.sh`脚本初始化环境
2. 在终端输入`cargo install cargo-generate`
3. 在终端输入`cargo generate --git https://github.com/DragonOS-Community/Rust-App-Template`即可创建项目
如果您的网络较慢，请使用镜像站`cargo generate --git https://git.mirrors.dragonos.org/DragonOS-Community/Rust-App-Template`
4. 使用`cargo run`来运行项目
5. 在DragonOS的`user/dadk/config`目录下，使用`dadk new`命令，创建编译配置,安装到DragonOS的`/`目录下。 
(在dadk的编译命令选项处，请使用Makefile里面的`make install`配置进行编译、安装)
6. 编译DragonOS即可安装
