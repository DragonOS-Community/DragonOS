# DADK - DragonOS Application Development Kit
# DragonOS 应用开发工具

[![codecov](https://codecov.io/gh/DragonOS-Community/DADK/graph/badge.svg?token=K3AYCACL8Z)](https://codecov.io/gh/DragonOS-Community/DADK)

## 简介

DADK是一个用于开发DragonOS应用的工具包，设计目的是为了让开发者能够更加方便的开发DragonOS应用。

## 文档

DADK的文档托管在[DADK Docs](https://docs.dragonos.org.cn/p/dadk/)上。

## DADK有什么用？

- 管理DragonOS应用的编译与安装
- 管理DragonOS rootfs镜像（支持 ext4、Docker base 导入、base 变更自动重建）
- 对DragonOS内核进行profiling
- 管理DragonOS的镜像构建、虚拟机运行


## 快速开始

### 安装DADK

DADK是一个Rust程序，您可以通过Cargo来安装DADK。

```shell
# 从GitHub安装最新版
cargo install --git https://github.com/DragonOS-Community/DADK.git

# 或安装指定版本（推荐与DragonOS分支匹配）
cargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v0.6.1 --locked


```

然后，转到[Quick Start](https://docs.dragonos.org.cn/p/dadk/user-manual/quickstart.html)以开始使用DADK。

## License

DADK is licensed under the [GPLv2 License](LICENSE).

## Contributing

欢迎贡献代码！请参阅[开发者指南](https://docs.dragonos.org.cn/p/dadk/dev-guide/)以了解如何贡献代码。
