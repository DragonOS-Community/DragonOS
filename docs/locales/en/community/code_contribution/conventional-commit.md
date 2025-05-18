:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: community/code_contribution/conventional-commit.md

- Translation time: 2025-05-19 01:41:57

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Code Commit Guidelines

&emsp;&emsp;This document will briefly introduce the code commit guidelines for the DragonOS GitHub repository, mainly providing the naming conventions based on Conventional Commit, as well as a brief introduction to the DragonOS Bot.

## Conventional Commit (Conventional Commit)

&emsp;&emsp;For detailed specifications on Conventional Commit, please refer to the website [Conventional Commit/Conventional Commit](https://www.conventionalcommits.org/zh-hans/v1.0.0/). At the end of this section, we will provide examples (taken from the [Conventional Commit/Conventional Commit](https://www.conventionalcommits.org/zh-hans/v1.0.0/) website), which are optional to read. We make the following special notes:
1. Since the DragonOS kernel ensures external usability primarily through system call interfaces, and up to now (April 22, 2024), considering the software ecosystem, DragonOS has chosen to implement system calls consistent with Linux. Therefore, there is no special explanation for `破坏性变更(BREAKING CHANGES)`, or in the current development environment, there will not be any destructive changes that significantly affect users. Therefore, unless there is a special need, the DragonOS kernel should not use `feat!` to indicate destructive changes. (Outside the kernel, such as dadk, the guidelines still apply.)
2. The DragonOS community strictly follows a squash-based workflow, so we do not require each individual commit in a PR to conform to [Conventional Commit/Conventional Commit](https://www.conventionalcommits.org/zh-hans/v1.0.0/). However, we still strongly recommend using it.
3. Regarding scope: If not specified otherwise, the scope should be the name of the submodule/system/directory. For example, if the code change is adding a feature in `kernel/src/driver/net`, it should be named as `feat(driver/net):`; if it is in `kernel/src/mm/allocator`, it should be named as `feat(mm)`. In short, the scope should be as short as possible to indicate the module it belongs to. Most of the time, it should not use more than two levels of scope identifiers. For example, `fix(x86_64/driver/apic)` is incorrect and should be named as `fix(x86_64/apic)`.
4. In the DragonOS kernel code repository, `issue checker` will perform a simple review of the title format. If it does not conform to the format, it will be marked as `ambiguous`. Contributors are advised to modify it as needed.
5. Use lowercase.

### Examples

#### Commit message with a description and a footnote indicating a breaking change
```
feat: allow provided config object to extend other configs

BREAKING CHANGE: `extends` key in config file is now used for extending other config files
```
#### Commit message with the ! character to alert about a breaking change
```
feat!: send an email to the customer when a product is shipped
```
#### Commit message with scope and a breaking change !
```
feat(api)!: send an email to the customer when a product is shipped
```
#### Commit message with ! and BREAKING CHANGE footnote
```
chore!: drop support for Node 6

BREAKING CHANGE: use JavaScript features not available in Node 6.
```
#### Commit message without a body
```
docs: correct spelling of CHANGELOG
```
#### Commit message with scope
```
feat(lang): add polish language
```

## DragonOS Bot

&emsp;&emsp;DragonOS uses triagebot to implement automatic labeling and reviewer assignment. Contributors can also interact with triagebot through some commands. For more details, see [triagebot](https://forge.rust-lang.org/triagebot/index.html)
