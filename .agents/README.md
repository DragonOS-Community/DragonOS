.agents 目录说明
=================

本目录用于集中存放 Agent 工具的通用资源与说明，避免分散在多个配置目录中造成维护成本增加与同步困难的问题。

## 作用
- 统一维护：集中存放各类 Agent 工具共用的配置方便管理与更新。
- 可复用：配合软链接将本目录挂载到其他 Agent 工具配置中，实现一次修改、多处同步。

## 与其他目录的关系
本目录可被其他 Agent 工具的配置目录通过软链接引用，以达到共享资源的目的。
例如将 `.agents/skills` 链接到外部 Agent 工具的技能目录中（示例命令，按实际路径调整）：
```
ln -s /path/to/DragonOS/.agents/skills /path/to/other-agent/skills
```
    
## 目录结构示例
```
.agents/
├── README.md           # 本说明文件
└── skills/             # 存放各类 Agent Skills
    ├── dragonos-gvisor-test-analysis/  # DragonOS gVisor 测试失败分析 Skill
    │   ├── SKILL.md     # Skill 说明文件
    │   └── references/  # 存放参考文档与格式说明
    │       └── FORMAT.md # 分析报告格式说明
    └── ...              # 其他 Skills
```
