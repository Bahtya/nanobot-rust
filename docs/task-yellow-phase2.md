# 🟡 Yellow Hat — 迁移价值与优先级评估

## 任务

你之前已经完成了 Hermes Agent 的价值评估，输出在 `/tmp/hats/05-yellow-hat-value.md`。

现在你需要：

### 第一步：回顾你的价值评估
读取 `/tmp/hats/05-yellow-hat-value.md`。

### 第二步：阅读 kestrel 源码
kestrel 源码在 `/opt/kestrel/kestrel/`。重点阅读：

1. 整体目录结构和所有 crate
2. `crates/kestrel-agent/src/` — 当前 agent 能力
3. `crates/kestrel-tools/src/` — 当前 tool 能力
4. `crates/kestrel-session/src/` — session 持久化能力
5. `crates/kestrel-config/src/` — 配置灵活性
6. `crates/kestrel-channels/src/` — 多平台支持
7. `README.md` — 当前功能描述

### 第三步：迁移价值与优先级
基于 kestrel 的现状，重新评估每个功能的迁移价值：

1. **现状差距评估**：kestrel 现在能做什么？缺什么？哪些"缺失"其实是 feature（简洁性）？
2. **用户价值排序**：对 kestrel 的实际用户来说，哪个功能最有价值？（考虑 kestrel 的定位是轻量级 Rust agent）
3. **迁移成本精确估算**：每个功能需要改动多少文件？新增多少行？依赖多少新 crate？
4. **增量价值曲线**：Phase 1 完成后用户体验提升多少？Phase 2 在 Phase 1 基础上又提升多少？
5. **kestrel 特色机会**：有什么是 Hermes 没做好但 kestrel 因为 Rust 语言优势可以做得更好的？
6. **最小可行产品定义**：如果只有 2 周开发时间，应该实现什么？为什么？

### 输出
在 `/tmp/hats/05-yellow-hat-value.md` 的基础上**追加**以下章节（用 `## 迁移价值与优先级` 标题）：

```markdown
## 迁移价值与优先级

### 1. 现状差距热力图
（功能 × kestrel 现状 × 差距程度 × 用户影响）

### 2. 用户价值排序（重新评估）
（基于 kestrel 定位重新排序，不同于对 Hermes 的排序）

### 3. 迁移成本矩阵
（功能 × 文件数 × 新增行数 × 新依赖 × 工作量（人天））

### 4. 增量价值曲线
（Phase 1 → Phase 2 → Phase 3 的累积用户价值）

### 5. Rust 特色优势机会
（只有 Rust 才能做到的差异化功能）

### 6. 2 周 MVP 方案
（精确到文件级别的实施计划）

### 7. 推荐迁移顺序（最终版）
（考虑价值、成本、依赖关系后的最优排序）
```

用中文写。务实、具体、可执行。
