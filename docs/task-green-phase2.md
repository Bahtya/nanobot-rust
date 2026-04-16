# 🟢 Green Hat — Rust 原生自我进化设计

## 任务

你之前已经完成了创新设计方案，输出在 `/tmp/hats/06-green-hat-design.md`。

现在你需要：

### 第一步：回顾你的设计方案
读取 `/tmp/hats/06-green-hat-design.md`。

### 第二步：深入阅读 kestrel 源码
kestrel 源码在 `/opt/kestrel/kestrel/`。逐个阅读所有 crate：

1. `crates/kestrel-agent/src/` — agent loop、context builder、subagent
2. `crates/kestrel-session/src/` — Session struct、SQLite store
3. `crates/kestrel-tools/src/` — Tool trait、ToolRegistry、所有内置 tool
4. `crates/kestrel-config/src/` — Config、各 *Config struct
5. `crates/kestrel-bus/src/` — EventBus、events 定义
6. `crates/kestrel-core/src/` — Platform、MessageType、error types
7. `crates/kestrel-providers/src/` — Provider trait、LLM 集成
8. `crates/kestrel-cron/src/` — 调度器（可复用于 self-review？）
9. `crates/kestrel-channels/src/` — ChannelManager、Telegram adapter
10. `crates/kestrel-security/src/` — SSRF 保护
11. `src/commands/` — CLI 命令实现

### 第三步：基于真实代码的设计
基于你对 kestrel 真实代码的理解，修正和完善你的 Rust 原生设计方案：

1. **Trait 设计精修**：之前的伪代码是概念性的。现在基于 kestrel 的实际 trait 风格（Tool trait、Provider trait），设计一致的 Skill trait 和 MemoryStore trait
2. **模块集成方案**：新 crate 如何与现有 crate 交互？具体的 use 路径、pub 接口、依赖方向
3. **消息流集成**：自我进化的 feedback 数据如何通过 kestrel-bus 传递？新的事件类型？EventBus 订阅模式？
4. **Session 扩展方案**：kestrel-session 已有 SQLite。如何扩展它来支持 memory 存储？新表？还是独立的存储？
5. **Context Builder 扩展**：kestrel-agent 的 ContextBuilder 只有 70 行。如何优雅地扩展它来支持 skill/memory 注入，同时不变成 Hermes 式的 God File？
6. **Self-Review 调度**：kestrel-cron 已有 tick-based scheduler。如何用它来触发 periodic self-review？
7. **Tool 扩展**：需要新增哪些 tool？它们的 execute() 方法如何实现？

### 输出
在 `/tmp/hats/06-green-hat-design.md` 的基础上**追加**以下章节（用 `## 基于 kestrel 的精修设计` 标题）：

```markdown
## 基于 kestrel 的精修设计

### 1. Trait 定义（完整 Rust 代码）
（Skill trait, MemoryStore trait, ReviewScheduler trait — 与现有 Tool trait 风格一致）

### 2. 新 Crate 结构
（kestrel-skills/ 和 kestrel-memory/ 的完整文件列表和职责）

### 3. 消息流集成方案
（Bus event 新类型定义、订阅模式、数据流图）

### 4. Session/Memory 存储设计
（SQLite 表结构、迁移脚本、CRUD 实现）

### 5. ContextBuilder 扩展方案
（现有 70 行代码如何优雅扩展到支持 skill/memory 的完整实现）

### 6. Self-Review 集成方案
（基于 kestrel-cron 的 review 调度实现）

### 7. 新 Tool 实现
（memory, skill, session_search 三个 tool 的完整 Rust 代码）

### 8. 集成测试方案
（如何用 kestrel 现有测试模式写自我进化的测试）

### 9. 完整的 Cargo.toml 变更
（workspace 和各 crate 的新依赖）
```

用中文写。所有代码必须是可编译的 Rust（不是伪代码）。与现有 kestrel 代码风格完全一致。
