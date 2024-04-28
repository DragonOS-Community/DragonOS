# 代码提交规范

&emsp;&emsp;这份文档将会简要的介绍DragonOS Github仓库的代码提交规范，主要是给出了基于Conventional Commit的命名规范，以及对DragonOS Bot的简要介绍

## Conventional Commit(约定式提交)

&emsp;&emsp;关于约定式提交的详细规范，在[Conventional Commit/约定式提交](https://www.conventionalcommits.org/zh-hans/v1.0.0/)网站中有详细介绍，在本节末尾将给出示例(摘自[Conventional Commit/约定式提交](https://www.conventionalcommits.org/zh-hans/v1.0.0/)网站)，可选择性阅读。我们做出以下特别说明：
1. 由于DragonOS内核原则上仅通过系统调用接口保证对外可用性，而迄今为止(2024/04/22)，出于对软件生态的考量，DragonOS选择实现与Linux一致的系统调用，因此不会对`破坏性变更(BREAKING CHANGES)`做特殊说明，或者说，在当前开发环境中不会产生对用户产生显著影响的破坏性变更，因此无特殊需要，DragonOS内核不应使用诸如`feat!`来表示破坏性变更。(内核之外，例如dadk，仍需遵循规范)
2. DragonOS社区严格遵循基于squash的工作流，因此我们并不强制要求PR中的每一个单独的commit都符合[Conventional Commit/约定式提交](https://www.conventionalcommits.org/zh-hans/v1.0.0/)，但是我们仍强烈建议使用。
3. 关于scope: 如无特殊说明，以子模块/系统/目录名作为范围，例如代码变动是发生在`kernel/src/driver/net`中的特性追加，那么应当命名为`feat(driver/net):`，如果是发生在`kernel/src/mm/allocator`中，应当命名为`feat(mm)`，简而言之就是要尽可能简短的表现出所属模块，大多数情况下，不应使用超过两级的范围标识，例如`fix(x86_64/driver/apic)`是错误的，应当命名为`fix(x86_64/apic)`
4. 在DragonOS内核代码仓库中的`issue checker`会对标题格式进行简单审查，如果不符合格式的将会被标记为`ambiguous`，贡献者们请按需修改
5. 使用小写

### 示例

#### 包含了描述并且脚注中有破坏性变更的提交说明
```
feat: allow provided config object to extend other configs

BREAKING CHANGE: `extends` key in config file is now used for extending other config files
```
#### 包含了 ! 字符以提醒注意破坏性变更的提交说明
```
feat!: send an email to the customer when a product is shipped
```
#### 包含了范围和破坏性变更 ! 的提交說明
```
feat(api)!: send an email to the customer when a product is shipped
```
#### 包含了 ! 和 BREAKING CHANGE 脚注的提交说明
```
chore!: drop support for Node 6

BREAKING CHANGE: use JavaScript features not available in Node 6.
```
#### 不包含正文的提交说明
```
docs: correct spelling of CHANGELOG
```
#### 包含范围的提交说明
```
feat(lang): add polish language
```

## DragonOS Bot

&emsp;&emsp; DragonOS使用triagebot来实现自动标签功能以及分配reviewer，贡献者也可以通过部分命令与triagebot交互，详见[triagebot](https://forge.rust-lang.org/triagebot/index.html)
