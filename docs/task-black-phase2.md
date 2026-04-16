# ⚫ Black Hat — 迁移风险深度分析

## 任务

你之前已经完成了 Hermes Agent 的风险分析，输出在 `/tmp/hats/04-black-hat-risks.md`。

现在你需要：

### 第一步：回顾你的风险分析
读取 `/tmp/hats/04-black-hat-risks.md`。

### 第二步：阅读 kestrel 源码
kestrel 源码在 `/opt/kestrel/kestrel/`。重点阅读：

1. `crates/kestrel-agent/src/` — agent 实现
2. `crates/kestrel-session/src/` — SQLite session 存储（并发模式？锁策略？）
3. `crates/kestrel-tools/src/` — Tool trait（错误处理模式？）
4. `crates/kestrel-bus/` — tokio broadcast（消息丢失风险？）
5. `crates/kestrel-config/src/` — 配置加载（文件 I/O 错误处理？）
6. `crates/kestrel-providers/src/` — LLM provider（重试逻辑？超时？）
7. `crates/kestrel-daemon/src/` — daemon 模式（多进程并发风险？）
8. `crates/kestrel-security/src/` — 安全模块（现有保护措施？）

### 第三步：迁移风险深度分析
针对将 Hermes 自我进化功能移植到 kestrel，识别所有风险：

1. **并发安全**：kestrel 是 async tokio。多个 gateway 同时运行时，memory 和 skill 的并发读写如何保证安全？SQLite WAL 模式？文件锁？
2. **数据损坏**：daemon 进程 crash 时，正在写入的 skill 文件会怎样？需要 WAL/journal 吗？
3. **Context Window 溢出**：skill + memory 注入 prompt 后，token 超限怎么办？kestrel 有 context compressor 吗？
4. **LLM 幻觉写入**：self-review 生成的"技能"可能是错的。如何验证？sandbox 测试？人工审核？
5. **供应链攻击**：如果未来支持社区 skill 下载，恶意 skill 注入 system prompt 的风险？
6. **迁移中的架构腐化**：Python → Rust 移植容易产生"翻译代码"而非"Rust 原生代码"的腐化风险
7. **测试挑战**：如何测试自我进化功能？LLM 输出不确定，skill 匹配不精确，如何写确定性测试？
8. **性能悬崖**：skill 数量增长后的匹配延迟？memory 查询的性能特征？
9. **向后兼容**：Config schema 变更时的迁移风险？skill 格式版本升级？
10. **单点故障**：哪个组件如果出问题，整个自我进化系统就瘫痪？

### 输出
在 `/tmp/hats/04-black-hat-risks.md` 的基础上**追加**以下章节（用 `## 迁移风险深度分析` 标题）：

```markdown
## 迁移风险深度分析

### 1. 并发安全风险矩阵
（场景 × 风险等级 × 保护措施 × 具体代码位置）

### 2. 数据完整性保障方案
（每个存储组件的 crash safety 分析）

### 3. Context Window 风险管理
（token 预算分配策略、溢出时的降级方案）

### 4. LLM 输出质量控制
（skill 验证流程、self-review 可信度评估）

### 5. 安全威胁模型
（攻击面、威胁场景、缓解措施）

### 6. 测试策略风险
（哪些难测、如何 mock、最小可测试单元）

### 7. 迁移实施风险时间线
（按迁移阶段排列的风险清单）

### 8. 风险缓解检查清单
（每个风险的一句话缓解措施，便于复查）
```

用中文写。比第一次更深入——结合 kestrel 的具体代码分析风险。
