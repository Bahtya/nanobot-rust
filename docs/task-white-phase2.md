# ⚪ White Hat — 迁移技术规格

## 任务

你之前已经完成了 Hermes Agent 自我进化系统的完整技术规格，输出在 `/tmp/hats/02-white-hat-specification.md`。

现在你需要：

### 第一步：回顾你的分析
读取 `/tmp/hats/02-white-hat-specification.md`。

### 第二步：深入阅读 kestrel 源码
kestrel 源码在 `/opt/kestrel/kestrel/`。重点阅读：

1. `crates/kestrel-agent/src/context.rs` — 当前 ContextBuilder（极简版 prompt 组装）
2. `crates/kestrel-agent/src/lib.rs` + `src/loop.rs` — agent loop
3. `crates/kestrel-session/` — Session 结构体、SQLite 持久化
4. `crates/kestrel-config/src/schema.rs` — Config 结构体所有字段
5. `crates/kestrel-tools/src/` — Tool trait、ToolRegistry
6. `crates/kestrel-bus/` — 消息总线（tokio broadcast）
7. `crates/kestrel-core/src/` — 核心类型（Platform, MessageType, InboundMessage, OutboundMessage）
8. `crates/kestrel-providers/` — LLM provider 实现
9. `src/commands/gateway.rs` — gateway 主循环
10. 所有 `Cargo.toml` — workspace 依赖

### 第三步：迁移技术规格
为每个 Hermes 自我进化组件，精确设计 Rust 移植方案：

1. **Memory 系统**：Hermes 用 `~/.hermes/memory.yaml` + Python dict。kestrel 用什么存储？SQLite？YAML？什么 schema？什么 trait 接口？
2. **Skill 系统**：Hermes 用 `~/.hermes/skills/*.md` 文件。kestrel 的 skill 格式？发现机制？加载时机？Skill trait 定义？
3. **Prompt Builder**：Hermes 的 `agent/prompt_builder.py` 非常复杂。kestrel 的 `context.rs` 只有 70 行。需要扩展多少？新增哪些 section？
4. **Self-Review**：Hermes 用 cron 触发。kestrel 已有 `kestrel-cron` crate。如何复用？
5. **Session 搜索**：Hermes 用 SQLite FTS5。kestrel 已有 `kestrel-session`。需要扩展什么？
6. **Tool 系统**：需要新增哪些 tool？（memory_tool, skill_tool, session_search_tool）

### 输出
在 `/tmp/hats/02-white-hat-specification.md` 的基础上**追加**以下章节（用 `## 迁移技术规格` 标题）：

```markdown
## 迁移技术规格

### 1. Memory 系统迁移规格
（Rust trait 定义、存储格式 schema、SQLite 表结构、CRUD API）

### 2. Skill 系统迁移规格
（Skill trait 定义、文件格式 frontmatter schema、发现和匹配算法）

### 3. Prompt Builder 扩展规格
（新增 section 列表、每个 section 的数据源、注入优先级、token 预算分配）

### 4. Self-Review 集成规格
（触发条件、数据收集、review prompt template、输出处理）

### 5. 需要新增的 Tool 列表
（每个 tool 的 schema、参数、行为描述）

### 6. 需要修改的现有文件清单
（文件路径 × 修改类型 × 具体变更描述）

### 7. 数据结构 Rust 定义
（所有新 struct/enum 的完整 Rust 代码）
```

用中文写。要极其精确——包含完整的 Rust struct/enum/trait 定义代码，具体的 SQL 表结构，精确的 YAML frontmatter 格式。直接修改原文件追加。
