# 🔴 Red Hat — 迁移设计批判

## 任务

你之前已经完成了对 Hermes Agent 的设计批判，输出在 `/tmp/hats/03-red-hat-critique.md`。

现在你需要：

### 第一步：回顾你的批判
读取 `/tmp/hats/03-red-hat-critique.md`。

### 第二步：阅读 kestrel 源码
kestrel 源码在 `/opt/kestrel/kestrel/`。重点阅读：

1. `crates/kestrel-agent/src/context.rs` — 当前的 prompt 组装（极简，70行）
2. `crates/kestrel-session/src/` — session 管理
3. `crates/kestrel-tools/src/` — Tool trait 和 registry
4. `crates/kestrel-config/src/schema.rs` — 配置体系
5. `crates/kestrel-bus/` — 消息总线
6. `src/commands/gateway.rs` — gateway 循环
7. 整体目录结构 — 理解 Rust 项目的组织方式

### 第三步：迁移设计批判
基于你对 Hermes 的批判，评估移植到 kestrel 时需要注意什么：

1. **不要重犯的错误**：Hermes 有哪些设计缺陷，kestrel 迁移时必须避免？列出具体的"反模式清单"
2. **Rust 强迫你做对的事**：Rust 的类型系统/所有权/生命周期会自动阻止哪些 Hermes 式的错误？
3. **过度设计警告**：哪些 Hermes 功能看起来炫酷但实际价值低？迁移时应该跳过？
4. **kestrel 的现有设计嗅觉**：当前 kestrel 代码有什么设计味道好的地方？什么值得扩展？什么应该重构？
5. **移植陷阱**：哪些"显而易见"的移植方案实际上是陷阱？表面相似但底层语义不同的情况？
6. **直觉排名**：如果要你直觉排序迁移优先级，你会怎么排？为什么？

### 输出
在 `/tmp/hats/03-red-hat-critique.md` 的基础上**追加**以下章节（用 `## 迁移设计批判` 标题）：

```markdown
## 迁移设计批判

### 1. 必须避免的反模式清单
（从 Hermes 批判推导出的"不要做"列表）

### 2. Rust 类型系统的保护
（Rust 自动阻止的 Hermes 式错误）

### 3. 过度设计警告
（应该跳过的 Hermes 功能 + 原因）

### 4. kestrel 现有设计评价
（什么好、什么需要重构、什么不要碰）

### 5. 移植陷阱 Top 10
（表面相似但语义不同的陷阱，按危险程度排序）

### 6. 直觉迁移优先级
（你的 gut feeling 排序 + 理由）
```

用中文写。保持你的锐利和诚实。
