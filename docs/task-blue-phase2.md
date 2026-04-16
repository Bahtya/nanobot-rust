# 🔵 Blue Hat — 迁移架构评估

## 任务

你之前已经完成了 Hermes Agent 自我进化系统的架构分析，输出在 `/tmp/hats/01-blue-hat-architecture.md`。

现在你需要：

### 第一步：回顾你的分析
读取 `/tmp/hats/01-blue-hat-architecture.md`，回忆你对 Hermes 自我进化架构的理解。

### 第二步：深入阅读 kestrel 源码
kestrel 源码在 `/opt/kestrel/kestrel/`。重点阅读以下文件，理解现有的架构和数据流：

1. `crates/kestrel-agent/src/context.rs` — 当前的 system prompt 组装（只有简单的身份+工具列表）
2. `crates/kestrel-agent/src/lib.rs` — agent loop 主循环
3. `crates/kestrel-session/` — session 存储和消息历史
4. `crates/kestrel-config/src/schema.rs` — Config 结构体，理解现有配置体系
5. `crates/kestrel-tools/` — tool registry 和工具系统
6. `src/commands/gateway.rs` — gateway 主循环，理解消息流
7. `src/main.rs` — 入口点
8. `crates/kestrel-core/src/` — 核心类型和错误定义
9. `Cargo.toml` + 各 crate 的 `Cargo.toml` — 依赖关系
10. `CLAUDE.md` — 项目整体架构说明

### 第三步：架构迁移评估
对比 Hermes 的自我进化架构和 kestrel 的现有架构，回答：

1. **架构差距**：kestrel 当前缺少哪些核心组件？每个组件的缺失程度如何？
2. **模块映射**：Hermes 的每个模块对应 kestrel 的什么位置？是扩展现有 crate 还是需要新 crate？
3. **数据流适配**：Hermes 的 feedback loop（用户交互→记录→评估→学习→改进）如何适配 kestrel 的消息流（InboundMessage → Bus → AgentLoop → OutboundMessage）？
4. **集成点分析**：自我进化系统需要 hook 进 kestrel 的哪些现有流程？每个 hook 点的侵入性如何？
5. **crate 边界**：新功能应该放在哪些 crate 里？是否需要新建 crate（如 kestrel-skills, kestrel-memory）？
6. **依赖链**：新 crate 的依赖关系如何？会不会引入循环依赖？

### 输出
在 `/tmp/hats/01-blue-hat-architecture.md` 的基础上**追加**以下章节（用 `## 迁移架构评估` 标题）：

```markdown
## 迁移架构评估

### 1. 架构差距矩阵
（表格：组件 × 缺失程度 × 预估工作量）

### 2. 模块映射表
（Hermes 模块 → kestrel 对应位置 → 新建/扩展）

### 3. 数据流适配方案
（ASCII 图：kestrel 消息流 + 自我进化反馈环的集成点）

### 4. Crate 边界设计
（新 crate 的职责划分和依赖关系图）

### 5. 集成点清单
（每个 hook 点：文件、函数、侵入性评级）

### 6. 迁移架构路线图
（Phase 1/2/3 的架构演进图）
```

用中文写。保持客观和系统性。直接修改原文件追加内容，不要创建新文件。
