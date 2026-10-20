# 构建用户程序

::: tip
在阅读本文之前，请确保你已经读过了[Quick Start](./quickstart.md)
:::

## DADK的工作原理

DADK使用`(任务名，任务版本）`二元组来标识每个构建目标。

当使用DADK构建DragonOS应用时，DADK会根据用户的配置文件，自动完成以下工作：

- 解析配置文件，生成DADK任务列表
- 根据DADK任务列表，进行拓扑排序。这一步会自动处理任务的依赖关系。
- 收集环境变量信息，并根据DADK任务列表，设置全局环境变量、任务环境变量。
- 根据拓扑排序后的DADK任务列表，自动执行任务。
- 从各个任务的输出缓存目录中，收集构建结果，拷贝到`bin/sysroot`目录下。

## 我该如何编写我的构建脚本？

你可以参考这个示例：

- [http server示例程序](https://code.dragonos.org.cn/xref/DragonOS-0.1.10/user/apps/http_server/Makefile)

原理就是，在构建阶段时，把程序拷贝到`DADK_CURRENT_BUILD_DIR`目录下。

## 我该如何编写dadk用户程序编译配置文件？

DADK用户程序编译配置文件的模版里面，有详细的注释，你可以参考这个：

- [userapp_config.toml](https://github.com/DragonOS-Community/DADK/blob/main/dadk-config/templates/config/userapp_config.toml)
