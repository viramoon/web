# 本仓库专属 Codex skill

仅与本仓库有关的 skill 放在 `.agents/skills/<名字>/SKILL.md`。名字使用小写英文与连字符，表达单一任务；SKILL.md 的 YAML 元数据必须包含非空 `name` 和 `description`，描述适用任务与触发条件。可按需带 `scripts/`、`references/`、`assets/`，只引用实际存在的文件。

通用 skill 在 [agent/skills](https://github.com/viramoon/agent/tree/main/skills) 维护，同步到 Codex 用户级 `~/.agents/skills`（支持软链接）及各 bot 的 skill 目录，不复制到本仓库。本目录当前只有组织约定，没有 skill 实现。后续新增具体 skill 需对应独立 issue，并遵守根目录 [AGENTS.md](../../AGENTS.md)。
