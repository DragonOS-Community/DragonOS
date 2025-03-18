# 👋 欢迎来到 2025 OSCOMP DragonOS ～
欢迎大家使用 DragonOS 作为内核实现赛道的基座系统！

## OSCOMP Roadmap
目前 DragonOS 正在紧锣密鼓适配 OSCOMP 的基座需求，同时也欢迎大家围绕下列部分添加实现～
- [ ] RISC-V
  - [ ] VirtIO base TTY
  - [x] Bootable Kernel
- [ ] LoongArch
- [ ] ext4 (正在实现)
- [ ] 优化启动内存序，允许qemu -kernel启动
- [ ] busybox (DragonOS使用的是NovaShell)

## 在 OSCOMP 下构建 riscv64
本分支已将默认 Target 设为 riscv64

### Quick Start
因为使用 CI 镜像工具链，在不额外配置环境的情况下先不构建用户程序

打开 VS Code ，安装 devcontainer 插件并进入 devcontainer 环境
```sh
make ci-run # or specify "ARCH=x86_64" to run x86 target
```

更多选项请运行 `make help`

> [!TIP]
> 如果没有看到提示进入 devcontainer 环境，可以 `ctrl+shift+p` 找到 `Dev Containers: Reopen in Container`。
> 第一次构建可能时间会有些久，尤其是拉取 CI 镜像时，请耐心一些～

---

</br>
</br>
<div align="center">
  <img width="40%" src="docs/_static/dragonos-logo.svg" alt="dragonos-logo"></br>
  <h2>打造完全自主可控的数字化未来！</h2>

<a href="https://dragonos.org"><img alt="官网" src="https://img.shields.io/badge/%E5%AE%98%E7%BD%91-DragonOS.org-4c69e4?link=https%3A%2F%2Fbbs.dragonos.org.cn" ></a>
<a href="https://bbs.dragonos.org.cn"><img alt="bbs" src="https://img.shields.io/badge/BBS-bbs.dragonos.org.cn-purple?link=https%3A%2F%2Fbbs.dragonos.org.cn" ></a>



--- 

</div>

# DragonOS

**Languages** 中文|[English](README_EN.md)

&nbsp;

&emsp;&emsp;DragonOS龙操作系统是一个面向云计算轻量化场景的，完全自主内核的，提供Linux二进制兼容性的64位操作系统。它使用Rust语言进行开发，以提供更好的可靠性。目前在Rust操作系统领域，DragonOS在Github排行全国稳居前三位。

&emsp;&emsp;DragonOS开源社区成立于2022年7月，它完全商业中立。我们的目标是，构建一个完全独立自主的、开源的、高性能及高可靠性的服务器操作系统，打造完全自主可控的数字化未来！

&emsp;&emsp;DragonOS具有优秀的、完善的架构设计。相比于同体量的其他系统，DragonOS支持虚拟化，并在设备模型、调度子系统等方面具有一定优势。当前正在大力推进云平台支持、riscv支持等工作，以及编译器、应用软件的移植。力求在5年内实现生产环境大规模应用。

&emsp;&emsp;DragonOS目前在社区驱动下正在快速发展中，目前DragonOS已经实现了约1/4的Linux接口，在未来我们将提供对Linux的100%兼容性，并且提供新特性。


## 参与开发？

仔细阅读 [DragonOS社区介绍文档] ，能够帮助你了解社区的运作方式，以及如何参与贡献！

- **了解开发动态、开发任务，请访问DragonOS社区论坛**： [https://bbs.dragonos.org.cn](https://bbs.dragonos.org.cn)
- 您也可以从项目的issue里面了解相关的开发内容。


&emsp;&emsp;如果你愿意加入我们，你可以查看issue，并在issue下发表讨论、想法，或者访问DragonOS的论坛，了解开发动态、开发任务： [https://bbs.dragonos.org.cn](https://bbs.dragonos.org.cn)

&emsp;&emsp;你也可以带着你的创意与想法，和社区的小伙伴一起讨论，为DragonOS创造一些新的功能。

## 网站


- 项目官网  **[DragonOS.org](https://dragonos.org)**
- 文档：**[docs.dragonos.org](https://docs.dragonos.org)**
- 社区介绍文档： **[community.dragonos.org](https://community.dragonos.org)**


## 如何运行？

&emsp;&emsp;运行DragonOS的步骤非常简单，您可以参考以下几个资料，在最短15分钟内运行DragonOS！

- [构建DragonOS — DragonOS dev 文档](https://docs.dragonos.org/zh_CN/latest/introduction/build_system.html)



## 如何与社区建立联系？

请阅读[贡献者指南](https://community.dragonos.org/contributors/#%E7%A4%BE%E5%8C%BA)~

- 您可以通过[社区管理团队]信息，与各委员会的成员们建立联系~
- 同时，您可以通过[SIGs]和[WGs]页面，找到对应的社区团体负责人的联系方式~

## 贡献者名单

[Contributors to DragonOS-Community/DragonOS · GitHub](https://github.com/DragonOS-Community/DragonOS/graphs/contributors)



## 赞助

&emsp;&emsp;DragonOS是一个公益性质的开源项目，但是它的发展离不开资金的支持，如果您愿意的话，可以通过 **[赞助 - DragonOS](https://dragonos.org/?page_id=37)** ，从而促进这个项目的发展。所有的赞助者的名单都会被公示。您的每一分赞助，都会为DragonOS的发展作出贡献！

### 赞助的资金都会被用到哪里？

我们保证，所有赞助的资金及物品，将会用于：

- 为活跃的社区开发者发放补贴或设备支持

- DragonOS的云服务开支

- 设备购置

- 任何有助于DragonOS发展建设的用途

### 赞助商列表

- **[中国雅云](https://yacloud.net)** 雅安数字经济运营有限公司为DragonOS提供了云服务器支持。

### 个人赞赏者列表

- 万晓兰
- David Wen
- [YJwu2023](https://github.com/YJwu2023)
- [longjin](https://github.com/fslongjin)
- [黄铭涛](https://github.com/1037827920)
- [许梓毫](https://github.com/Jomocool)
- [谢润霖](https://github.com/xiaolin2004)
- [蔡俊源](https://github.com/SMALLC04)
- Kelly
- [Samuka007](https://github.com/Samuka007)
- [杨璐玮](https://github.com/val213)
- [何懿聪](https://github.com/GnoCiYeH)
- [周凯韬](https://github.com/laokengwt)
- [Seele.Clover](https://github.com/seeleclover)
- [FindWangHao](https://github.com/FindWangHao)
- [ferchiel](https://github.com/ferchiel)
- 叶锦毅
- 林
- Albert
- [TerryLeeSCUT · GitHub](https://github.com/TerryLeeSCUT)
- slientbard
- 悟

## 开放源代码声明

本项目采用GPLv2协议进行开源，欢迎您在遵守开源协议的基础之上，使用本项目的代码！

**我们支持**：遵守协议的情况下，利用此项目，创造更大的价值，并为本项目贡献代码。

**我们谴责**：任何不遵守开源协议的行为。包括但不限于：剽窃该项目的代码作为你的毕业设计等学术不端行为以及商业闭源使用而不付费。

若您发现了任何违背开源协议的使用行为，我们欢迎您发邮件到 pmc@dragonos.org 反馈！让我们共同建设诚信的开源社区。


[DragonOS社区介绍文档]: https://community.dragonos.org/
[社区管理团队]: https://community.dragonos.org/governance/staff-info.html
[SIGs]: https://community.dragonos.org/sigs/
[WGs]: https://community.dragonos.org/wgs/
