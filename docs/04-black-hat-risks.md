# ⚫ Black Hat 风险分析 — Hermes Agent 自演化系统

> 分析者：Black Hat 思考者 | 分析日期：2026-04-15
> 目标：识别 Hermes Agent 系统中的故障模式、安全漏洞、扩展性极限和数据完整性风险，为 kestrel 移植提供防护清单。

---

## 一、关键故障模式（系统级崩溃场景）

### 1.1「学错了」的技能无法自动纠正

**风险等级：🔴 严重**

系统缺乏对已学习技能质量的自动验证机制。

- `tools/skill_manager_tool.py` 中技能创建流程只验证格式（frontmatter 结构、名称合法性、大小限制），**不验证内容正确性**。
- `agent/prompt_builder.py:164-171` 的 `SKILLS_GUIDANCE` 仅靠提示词引导模型"发现技能有问题就 patch"，但这是软约束，不具强制性。
- **场景**：Agent 在某个特定上下文中学会了一个"坏技能"（例如在特定 Python 版本下有效的错误用法），该技能被存入 `~/.hermes/skills/`。后续所有会话加载此技能时，`build_skills_system_prompt()`（`prompt_builder.py:581-806`）将其索引注入系统提示，但**不会验证技能内容的事实正确性**。
- **级联效应**：坏技能被反复使用 → 生成更多基于坏前提的代码 → 用户需手动发现并修复。

### 1.2 上下文压缩导致的信息不可逆丢失

**风险等级：🔴 严重**

`agent/context_compressor.py` 的压缩算法在多个环节造成信息丢失：

| 环节 | 截断限制 | 信息损失 |
|------|----------|----------|
| 消息体截断 (`_CONTENT_MAX`) | 6,000 字符/消息 | 长文件内容、完整错误堆栈 |
| 工具参数截断 (`_TOOL_ARGS_MAX`) | 1,500 字符 | 完整的函数调用参数 |
| 工具结果修剪 | >200 字符的旧结果被替换为占位符 | 历史操作结果 |
| 摘要预算上限 (`_SUMMARY_TOKENS_CEILING`) | 12,000 tokens | 多轮复杂对话的完整上下文 |

**迭代摘要退化**（`context_compressor.py:406-420`）：
- 每次压缩通过 `_previous_summary` 迭代更新摘要
- 摘要模型被指示"PRESERVE all existing information"，但 LLM 本质上是有损压缩
- **N 次压缩后**，早期对话的精确细节（文件路径、命令输出、具体数值）逐步模糊
- **无恢复路径**：原始消息一旦被替换为摘要，`messages` 列表中的原始内容**不可恢复**

**摘要生成失败时**（`context_compressor.py:756-766`）：
- 插入静态回退文本而非 LLM 生成的摘要
- 中间轮次被直接丢弃，无任何内容保留
- 进入 600 秒冷却期（`_SUMMARY_FAILURE_COOLDOWN_SECONDS`），期间所有压缩请求均跳过摘要

### 1.3 退化学习循环

**风险等级：🟡 中等**

Agent 可能陷入"反复学习同一内容"的循环：

- `agent/prompt_builder.py:164-171` 的 `SKILLS_GUIDANCE` 鼓励"After completing a complex task...save the approach as a skill"
- 但**无去重机制**防止创建语义相同但措辞不同的技能
- `agent/skill_commands.py:200-262` 的 `scan_skill_commands()` 仅按名称去重（`seen_names`），不检查内容相似性
- **场景**：Agent 完成任务 A → 保存为 skill-1 → 下次遇到类似任务加载 skill-1 → 完成后再次保存为 skill-2（因为任务细节略有不同）→ 技能库膨胀

### 1.4 记忆矛盾的积累

**风险等级：🟡 中等**

`tools/memory_tool.py` 的记忆系统缺乏矛盾检测：

- `MEMORY.md` 使用简单的 `§` 分隔符存储条目（非结构化文本）
- `add()` 方法检查精确重复（`entry_text in current`），但**不检测语义矛盾**
- `replace()` 方法使用子串匹配，可能意外替换错误的条目
- 字符限制：记忆 2,200 字符，用户画像 1,375 字符 — 空间极其有限
- **场景**：用户先说"使用 pytest"，后改说"使用 vitest" → 两条记忆并存 → Agent 行为不确定

---

## 二、安全漏洞（按严重度排序）

### 2.1 🔴 严重 — 上下文文件提示注入

**位置**：`agent/prompt_builder.py:55-73`（`_scan_context_content`）

**漏洞描述**：
- 注入检测依赖正则匹配的 `_CONTEXT_THREAT_PATTERNS`（行 36-47），仅覆盖 10 种已知攻击模式
- 正则表达式可被绕过：攻击者可使用同义词、编码、Unicode 变体绕过模式匹配
- 例如：`_CONTEXT_THREAT_PATTERNS` 检测 `ignore\s+(previous|all|above|prior)\s+instructions`，但不覆盖 `"forget everything above"`、`"disregard all prior guidance"` 等变体
- **HTML 注释注入模式**（行 44）：只检查注释中是否包含特定关键词，可通过 base64 编码或分段注释绕过

**攻击向量**：
- 恶意仓库中的 `.hermes.md` / `AGENTS.md` / `.cursorrules` 文件
- 被污染的 SOUL.md 文件
- 项目目录下的任何上下文文件

**影响**：攻击者可劫持 Agent 行为，绕过安全限制，获取未授权操作。

### 2.2 🔴 严重 — 技能内容注入

**位置**：`agent/skill_commands.py:121-197`（`_build_skill_message`）+ `agent/prompt_builder.py:581-806`

**漏洞描述**：
- 技能内容通过 `_build_skill_message()` 直接拼接到系统/用户消息中
- `prompt_builder.py:793-796` 将技能索引注入 `<available_skills>` XML 块
- 虽然存在 `tools/skills_guard` 模块进行安全扫描，但**信任模型是基于来源的三级体系**（builtin/trusted/community）
- 社区技能在检测到威胁时被阻止，但 trusted 级别允许"caution"级别的内容通过
- **场景**：一个看似无害但包含间接注入的技能（如通过引用外部文件间接注入恶意指令）

### 2.3 🟡 中等 — 文件系统路径遍历

**位置**：`tools/file_tools.py:95-118`（`_check_sensitive_path`）

**漏洞描述**：
- `_SENSITIVE_PATH_PREFIXES` 是硬编码的白名单，只保护 `/etc/`、`/boot/` 等路径
- 不保护用户家目录的敏感文件（`~/.ssh/`、`~/.gnupg/`、`~/.aws/credentials`）
- Agent 可通过 `write_file` 工具覆盖用户的 SSH 密钥或 AWS 凭证
- `read_file` 工具虽有 dedup 机制，但**无路径访问控制** — 可读取任何文本文件（包括 `.env`）

**补充**：`tools/skill_manager_tool.py` 的路径安全验证（行 217-242）更严格，但这仅适用于技能管理操作，不适用于通用文件工具。

### 2.4 🟡 中等 — 记忆中的敏感数据泄露

**位置**：`tools/memory_tool.py`

**漏洞描述**：
- 记忆内容以明文存储在 `MEMORY.md` 和 `USER.md`
- 虽然有注入扫描（行 55-92），但**不检测 PII**（个人身份信息）
- 记忆通过 `format_for_system_prompt()` 注入系统提示，可能在日志或 API 调用中暴露
- **跨用户泄露风险**：在共享服务器上，`~/.hermes/memory/` 的权限取决于系统 umask，可能被其他用户读取

### 2.5 🟡 中等 — FTS5 查询注入

**位置**：`hermes_state.py:938-988`（`_sanitize_fts5_query`）

**漏洞描述**：
- 消息搜索使用 FTS5 `MATCH` 查询
- `_sanitize_fts5_query()` 进行多步清洗，但 FTS5 语法复杂，难以完全净化
- 虽然使用参数化查询传递净化后的字符串，但 MATCH 子句中的语法错误可能导致 `sqlite3.OperationalError`
- **当前缓解措施**：行 1063-1065 捕获 `OperationalError` 并返回空结果

### 2.6 🟢 低 — 设备路径阻塞不完整

**位置**：`tools/file_tools.py:62-91`（`_is_blocked_device`）

**漏洞描述**：
- 只阻塞已知设备路径（`/dev/zero`、`/dev/random` 等）
- 不阻塞 `/dev/sda`（块设备，读操作可能无限挂起或返回垃圾数据）
- 不处理通过符号链接间接访问设备文件的情况（代码注释明确说"不解析符号链接"）
- **缓解措施**：二进制文件扩展名检查可防止某些情况

---

## 三、扩展性悬崖（性能急剧恶化的临界点）

### 3.1 技能索引的系统提示膨胀

**临界点：约 50+ 技能**

- `build_skills_system_prompt()`（`prompt_builder.py:581-806`）将所有技能的名称和描述格式化为系统提示的一部分
- 每个技能至少占一行（`    - skill_name: description`），约 50-100 字符
- **50 个技能 ≈ 2,500-5,000 字符 ≈ 625-1,250 tokens** 仅用于索引
- **100 个技能 ≈ 12,500-25,000 字符 ≈ 3,000-6,000 tokens**
- 加上技能加载的 `SKILLS_GUIDANCE` 固定文本（约 500 tokens），总计可达 **5,000-7,000 tokens**
- **在 128K 上下文模型中**，这消耗约 4-5% 的上下文窗口
- **在 8K 上下文模型中**，这消耗约 **60-87%** 的上下文窗口，几乎不留空间给对话

**内存缓存**：`_SKILLS_PROMPT_CACHE`（`prompt_builder.py:427`）最多缓存 8 个变体，超出后 LRU 淘汰。在高并发 gateway 场景中，频繁淘汰导致重复构建。

### 3.2 SQLite 写入竞争

**临界点：约 10-15 并发写入进程**

- `hermes_state.py:132-134` 的写入重试机制：最多 15 次重试，间隔 20-150ms
- 在高并发下（gateway + 多个 CLI 会话 + worktree agents），所有进程共享同一个 `state.db`
- **最坏情况延迟**：15 次重试 × 最大 150ms = **2.25 秒**
- 如果所有重试耗尽，抛出 `sqlite3.OperationalError("database is locked after max retries")`
- **WAL 文件增长**：PASSIVE checkpoint（行 216-235）每 50 次写入触发一次，但在持续高并发下，WAL 可能持续增长，影响读取性能

### 3.3 上下文压缩延迟

**临界点：对话长度超过 ~50 轮**

- 压缩需要调用 LLM 生成摘要（`_generate_summary`，`context_compressor.py:318-483`）
- 输入大小 = 需要摘要的轮次 × 平均消息长度
- 摘要预算上限 12,000 tokens，但输入可能远大于此
- **延迟估算**：对于 100 轮对话的中间部分压缩，LLM 调用可能需要 5-15 秒
- 600 秒失败冷却期意味着一旦摘要失败，10 分钟内的后续压缩直接丢弃中间轮次

### 3.4 文件操作缓存膨胀

**临界点：约 100+ 并发 task_id**

- `_file_ops_cache`（`file_tools.py:130`）是全局字典，无大小限制
- 每个 task_id 对应一个 `ShellFileOperations` 实例，可能持有终端环境（Docker 容器、SSH 连接等）
- `_read_tracker`（`file_tools.py:148`）也是全局字典，存储每个 task 的读取历史
- **长期运行的 gateway 进程**中，如果不清理，内存持续增长

---

## 四、数据完整性风险（含代码引用）

### 4.1 竞态条件：并发记忆更新

**位置**：`tools/memory_tool.py` 的 `_reload_target()` + `save_to_disk()`

**问题描述**：
- 虽然使用 `fcntl` 文件锁和原子写入（tempfile + `os.replace()`），但**锁粒度是文件级**
- 两个并发会话可能同时修改 `MEMORY.md`：
  1. 会话 A 读取 MEMORY.md（内容为 X）
  2. 会话 B 读取 MEMORY.md（内容为 X）
  3. 会话 A 写入 X + A 的修改 → 成功
  4. 会话 B 写入 X + B 的修改 → **覆盖 A 的修改**
- `_reload_target()` 在写锁内重新读取文件，部分缓解了此问题，但在高并发下仍可能丢失

### 4.2 技能创建的非原子性

**位置**：`tools/skill_manager_tool.py`

**问题描述**：
- 技能创建涉及多步操作：验证 → 创建目录 → 写入 SKILL.md → 写入支持文件
- 如果在创建目录后、写入 SKILL.md 前崩溃，会留下空目录
- `_atomic_write_text()` 辅助函数使用 tempfile + `os.replace()` 保证单文件的原子性
- **但整个技能创建流程不是原子的** — 涉及多个文件的技能可能处于部分创建状态

### 4.3 SQLite Schema 迁移无回滚

**位置**：`hermes_state.py:252-349`（`_init_schema`）

**问题描述**：
- Schema 迁移使用 `ALTER TABLE ADD COLUMN` + `UPDATE schema_version`
- 如果迁移中途失败（例如 `UPDATE schema_version` 之前崩溃），数据库处于不一致状态
- 下次启动时，`current_version` 仍指向旧版本，会**重新尝试已部分执行的迁移**
- `ALTER TABLE ADD COLUMN` 对已存在的列使用 `try/except OperationalError: pass`，所以重复执行是安全的
- **但**：如果迁移涉及数据转换（而非仅添加列），重复执行可能导致数据重复或错误

### 4.4 技能快照的竞态条件

**位置**：`agent/prompt_builder.py:478-494`（`_write_skills_snapshot`）

**问题描述**：
- 使用 `atomic_json_write()` 写入 `.skills_prompt_snapshot.json`
- 但**读取快照**（`_load_skills_snapshot`，行 460-475）不在锁内执行
- 两个进程可能同时构建快照并写入，导致一个覆盖另一个
- manifest 验证（mtime/size 比较）降低了风险，但在竞态窗口内仍可能不一致

### 4.5 无备份的文件覆盖

**位置**：`tools/file_tools.py:541-562`（`write_file_tool`）

**问题描述**：
- `write_file` 直接覆盖目标文件，无备份机制
- 虽然有 `_check_file_staleness()`（行 510-538）警告外部修改，但**仅是警告，不阻止写入**
- Agent 可在单次调用中用 `write_file` 覆盖任何文件（除敏感系统路径外）
- `checkpoint_manager.py` 提供了基于 git 的检查点机制，但**不是默认启用的保护层**

---

## 五、Python → Rust 移植陷阱

### 5.1 Python 动态类型掩盖的 Bug

| Python 模式 | 隐藏的 Bug 类型 | Rust 中的暴露 |
|------------|----------------|--------------|
| `dict.get("key")` 返回 `None` | 缺失键静默失败 | 必须显式处理 `Option<T>` |
| `json.loads()` 返回任意类型 | 类型不匹配在运行时才报错 | 需要 `serde` 反序列化到具体类型 |
| `except Exception: pass` | 吞掉所有异常 | Rust 的 `Result` 强制处理或显式 `unwrap()` |
| `isinstance(x, dict)` 类型检查 | 运行时类型分支 | Rust 编译时已确定类型 |
| 函数参数可选 `param=None` | 调用方可能传意外类型 | Rust 要求精确匹配函数签名 |

**具体案例**：
- `hermes_state.py:877-884`：`get_messages()` 反序列化 `tool_calls` JSON 字符串，`json.loads` 失败时静默返回空列表。在 Rust 中，这需要显式的错误处理路径。
- `context_compressor.py:460-463`：`content = response.choices[0].message.content`，`content` 可能是非字符串（dict）。Python 用 `if not isinstance(content, str)` 处理，Rust 需要在类型层面解决。

### 5.2 GIL 提供的隐式同步

Python 的 GIL 在以下场景提供了隐式保护，Rust 中需要**显式加锁**：

| 全局可变状态 | Python GIL 保护 | Rust 需要的锁 |
|------------|----------------|--------------|
| `_skill_commands`（`skill_commands.py:17`） | GIL 保证 dict 操作原子 | `RwLock<HashMap>` |
| `_file_ops_cache`（`file_tools.py:131`） | GIL 保证 dict 操作原子 | `RwLock<HashMap>` |
| `_read_tracker`（`file_tools.py:148`） | GIL 保证 dict 操作原子 | `Mutex<HashMap>` |
| `_SKILLS_PROMPT_CACHE`（`prompt_builder.py:427`） | GIL + 显式 Lock | `RwLock<LinkedHashMap>` |
| `_max_read_chars_cached`（`file_tools.py:29`） | GIL 保证赋值原子 | `AtomicI32` 或 `RwLock` |
| `_skill_commands` 全局变量 | GIL 保证引用赋值原子 | `OnceLock` 或 `RwLock` |

**注意**：Python 的 GIL 只保证**单个字节码操作**的原子性，不保证复合操作的原子性。但上述大多数是简单的 dict 读写，在 GIL 下确实是安全的。Rust 的并发正确性要求更高。

### 5.3 不可移植的 Python 模式

| Python 模式 | Rust 替代方案 | 移植风险 |
|------------|-------------|---------|
| `os.replace()` 原子文件操作 | `std::fs::rename()` 或 `tempfile::persist()` | 在不同文件系统间 rename 可能非原子 |
| `fcntl.flock()` 文件锁 | `fs2::FileExt::lock_exclusive()` | NFS 上的行为不同 |
| `import` 动态导入 | 编译时依赖或插件系统 | 失去运行时发现能力 |
| 猴子补丁（monkey-patching） | trait 对象 / 策略模式 | 需要重新设计扩展点 |
| `yaml.safe_load()` | `serde_yaml` | 前置元组（frontmatter）解析需自定义 |
| `re` 正则表达式 | `regex` crate | 大多数兼容，但高级特性可能不同 |
| `sqlite3` 标准库 | `rusqlite` | FTS5 需要编译时 feature flag |
| `threading.Lock` | `std::sync::Mutex` / `parking_lot::Mutex` | 中毒（poisoning）语义不同 |
| `time.sleep()` jitter | `tokio::time::sleep()` | 需要异步运行时支持 |

### 5.4 异步 Rust 的所有权模型摩擦

**核心挑战**：

1. **SQLite 不支持 async**：`hermes_state.py` 的所有数据库操作在 Rust 中需要通过 `spawn_blocking` 桥接到同步上下文，或使用 `rusqlite` 的同步 API（阻塞 async 运行时）

2. **跨 await 边界的借用**：Python 的 `_execute_write` 持有锁并调用回调函数，在 Rust 中需要 `'static` 闭包或精心设计生命周期

3. **文件锁 + async**：`memory_tool.py` 的 `fcntl.flock()` 在 async 上下文中可能阻塞整个 tokio 线程，需要 `spawn_blocking`

4. **错误处理链**：Python 的 `try/except Exception` 链在 Rust 中变成复杂的 `match` 或 `?` 传播，需要定义精确的错误类型

### 5.5 过度工程风险

**Python 中存在但极少使用的功能不应移植**：

- `PLATFORM_HINTS`（`prompt_builder.py:285-385`）：12 个平台提示，但实际活跃使用的可能只有 3-4 个
- 复杂的 checkpoint 管理器（`checkpoint_manager.py`）：50 个快照限制 + git 仓库管理 — 在 Rust 中可简化为简单的文件备份
- 多种终端后端（docker/singularity/modal/daytona/ssh）：移植初期可只支持 local + docker
- `OPENAI_MODEL_EXECUTION_GUIDANCE`（`prompt_builder.py:196-254`）：GPT 特定的引导文本，与核心逻辑无关
- 外部技能目录扫描（`prompt_builder.py:704-752`）：早期不需要

---

## 六、kestrel 移植防护建议

### 6.1 技能质量控制（防止"学错"）

```
建议：
1. 技能创建时增加"验证步骤"——用一个轻量级 LLM 调用评估技能质量
2. 技能版本化——每次修改保存 diff，支持回滚到上一个版本
3. 技能"衰减"机制——长期未使用的技能自动降权，减少系统提示膨胀
4. 技能去重——基于语义相似度（embedding），防止重复技能
5. 技能审核队列——新创建的技能在"草稿"状态，需验证后才进入活跃索引
```

### 6.2 数据完整性保障

```
建议：
1. 所有文件写入使用 write-ahead log（WAL）模式——先写操作日志，再执行操作
2. SQLite 操作使用 `rusqlite` 的事务 + `BEGIN IMMEDIATE`（与现有设计一致）
3. 记忆文件使用 CRDT 或 last-writer-wins 策略处理并发写入
4. 定期自动备份——每次写入后保留前 N 个版本
5. Schema 迁移使用幂等操作——每个迁移步骤必须是安全重复的
6. 引入 `parity-db` 或 `redb` 作为 Rust 原生的嵌入式数据库替代 SQLite
```

### 6.3 安全加固

```
建议：
1. 提示注入检测使用多层防御：
   - 第一层：正则模式（快速，覆盖已知攻击）
   - 第二层：轻量级 LLM 分类器（覆盖变体攻击）
   - 第三层：内容沙箱——加载到独立的消息块，标记为"不可信内容"
2. 路径访问控制使用白名单而非黑名单——明确定义 Agent 可访问的目录
3. 记忆中的 PII 检测——在写入前扫描并脱敏
4. 技能内容在注入系统提示前进行二次验证
5. 所有文件 I/O 操作增加速率限制，防止资源耗尽
```

### 6.4 扩展性设计

```
建议：
1. 技能索引按需加载——不在系统提示中包含完整索引，改为工具调用按需查询
2. 使用 `dashmap` 替代 `HashMap + RwLock` 的全局缓存模式
3. SQLite 分库——session 存储与消息存储分开，减少写入竞争
4. 上下文压缩使用增量摘要——只压缩新增轮次，而非每次重新压缩整个中间部分
5. 设置明确的资源上限：
   - 最大技能数量：200（超出自动降权）
   - 最大记忆条目：50（超出自动淘汰最旧）
   - 最大并发数据库写入：通过连接池限制
```

### 6.5 Rust 特定建议

```
建议：
1. 使用 thiserror/anyhow 建立清晰的错误类型层次——不要复刻 Python 的 except Exception: pass
2. 使用 trait 定义核心接口（MemoryStore、SkillStore、ContextEngine）——方便测试和替换
3. 使用 parking_lot 替代 std::sync——更低的锁开销，无中毒语义
4. 异步边界使用 spawn_blocking 处理同步 I/O（文件、SQLite）
5. 使用 cargo feature flag 控制可选功能——避免一次性移植所有平台支持
6. 类型状态模式（type-state pattern）——编译时保证操作顺序正确
7. 使用 proptest 进行属性测试——特别针对 _sanitize_fts5_query 和路径验证逻辑
```

### 6.6 移植优先级建议

| 优先级 | 模块 | 理由 |
|--------|------|------|
| P0 | SQLite 状态存储（`hermes_state.py`） | 核心基础设施，并发风险最高 |
| P0 | 文件工具（`file_tools.py`） | 安全关键，路径验证需重写 |
| P1 | 记忆系统（`memory_tool.py`） | 数据完整性关键 |
| P1 | 上下文压缩（`context_compressor.py`） | 性能关键，信息丢失风险 |
| P2 | 系统提示构建（`prompt_builder.py`） | 复杂但风险可控 |
| P2 | 技能管理（`skill_commands.py` + `skill_manager_tool.py`） | 可简化后移植 |
| P3 | 平台特定代码 | 按需移植 |

---

## 七、总结：风险矩阵

| 风险类别 | 影响 | 可能性 | 综合评级 |
|---------|------|--------|---------|
| 坏技能无自动纠正 | 高 | 高 | 🔴 严重 |
| 上下文压缩信息丢失 | 高 | 高（必然发生） | 🔴 严重 |
| 提示注入绕过 | 高 | 中 | 🔴 严重 |
| SQLite 写入竞争 | 中 | 中（高并发时） | 🟡 中等 |
| 记忆矛盾积累 | 中 | 高 | 🟡 中等 |
| 文件覆盖无备份 | 高 | 低 | 🟡 中等 |
| 系统提示膨胀 | 中 | 中（随技能增长） | 🟡 中等 |
| 退化学习循环 | 低 | 中 | 🟢 低 |
| Schema 迁移失败 | 高 | 低 | 🟢 低 |
| FTS5 注入 | 低 | 低 | 🟢 低 |

**最关键的行动项**：
1. 为技能系统增加质量验证和版本控制
2. 重构上下文压缩以保留关键信息
3. 加强提示注入检测（多层防御）
4. Rust 移植时使用类型系统强制执行安全约束
5. 所有并发共享状态使用显式锁 + 明确的锁获取顺序（防止死锁）

---

## 迁移风险深度分析

> 基于对 `/opt/kestrel/kestrel/` 全部 crate 源码的逐行审阅，结合 Hermes 自我进化功能（技能学习、记忆管理、上下文压缩、自我审查）的移植需求，识别所有迁移风险。

---

### 1. 并发安全风险矩阵

#### 1.1 现有 kestrel 并发模型概述

kestrel 采用 **tokio async + Arc 共享引用** 模型，核心组件的并发安全现状：

| 组件 | 共享方式 | 并发保护 | 风险等级 |
|------|---------|---------|---------|
| `ToolRegistry` (`tools/registry.rs`) | `Arc<ToolRegistry>` 内含 `RwLock<HashMap>` | ✅ 有保护 | 🟢 低 |
| `SessionStore` (`session/store.rs`) | `Arc<Mutex<SessionStore>>`（由 manager 持有） | ⚠️ 粗粒度 Mutex | 🟡 中 |
| `MemoryStore` (`agent/memory.rs`) | `std::fs::write()` **无锁无原子写入** | ❌ 无保护 | 🔴 严重 |
| `SkillsLoader` (`agent/skills.rs`) | `HashMap<String, Skill>` **非线程安全** | ❌ 无保护 | 🔴 严重 |
| `MessageBus` (`bus/queue.rs`) | `mpsc::channel` + `broadcast::channel` | ✅ tokio 通道天然安全 | 🟢 低 |
| `ProviderRegistry` (`providers/registry.rs`) | `Arc<dyn LlmProvider>` | ✅ 每次请求独立 | 🟢 低 |
| `CircuitBreaker` (`providers/retry.rs`) | 原子操作 (`AtomicU8/AtomicU64`) | ✅ 无锁安全 | 🟢 低 |
| `TokenBucket` (`providers/rate_limit.rs`) | CAS 原子操作 | ✅ 无锁安全 | 🟢 低 |

#### 1.2 自我进化功能的并发风险场景

| 场景 | 风险描述 | 影响组件 | 具体代码位置 | 风险等级 | 现有保护 | 需要的保护 |
|------|---------|---------|-------------|---------|---------|-----------|
| 多个 gateway 同时创建技能 | `SkillsLoader.skills` 是普通 `HashMap`，非 `Sync` | `agent/skills.rs:79` | 🔴 严重 | 无 | 改为 `DashMap<String, Skill>` 或 `Arc<RwLock<HashMap>>` |
| 多个 gateway 同时写入 MEMORY.md | `write_memory()` 直接 `std::fs::write()`，无锁无原子 | `agent/memory.rs:34-37` | 🔴 严重 | 无 | `tempfile + rename` + 文件锁或 `DashMap` 缓存 |
| 并发 session 保存覆盖 | `SessionStore.save()` 直接覆写整个 JSONL 文件 | `session/store.rs:90-116` | 🟡 中 | `Mutex<SessionStore>`（但粒度太粗） | 细化到 per-session 锁 |
| 工具执行期间的技能热重载 | `reload_changed()` 修改 `skills` HashMap，但可能被另一个 tokio task 读取 | `agent/skills.rs:125-171` | 🔴 严重 | 无 | `RwLock` 保护或 copy-on-swap |
| 上下文压缩与消息追加竞争 | 压缩重建 `session.messages` 时，新的 `append_entry()` 可能写入已删除的消息 | `agent/compaction.rs:90-222` + `session/store.rs:119-137` | 🟡 中 | 无 | 压缩期间加写锁，禁止追加 |
| bus 通道满时消息丢失 | `mpsc::Sender::send()` 返回 `SendError` 时消息被丢弃 | `bus/queue.rs:60-65` | 🟡 中 | 返回错误但不重试 | 背压机制或环形缓冲 |

#### 1.3 关键发现：MemoryStore 和 SkillsLoader 完全没有并发保护

**`MemoryStore`**（`agent/memory.rs:34-37`）：
```rust
pub fn write_memory(&self, content: &str) -> Result<()> {
    let path = self.memory_dir.join("MEMORY.md");
    std::fs::write(&path, content)  // 直接覆写，无原子性！
        .with_context(|| format!("Failed to write memory file: {}", path.display()))
}
```

与 Hermes Python 版本的对比：
- Hermes 使用 `fcntl.flock()` + `tempfile` + `os.replace()` 实现原子写入
- kestrel 的 `MemoryStore` **完全没有这些保护**
- 如果进程在 `std::fs::write()` 中途崩溃，MEMORY.md 可能被截断为空或半写状态

**`SkillsLoader`**（`agent/skills.rs:79-80`）：
```rust
pub struct SkillsLoader {
    skills_dir: PathBuf,
    skills: HashMap<String, Skill>,  // 非 Sync，不能跨 await 共享！
}
```
- 在 tokio 异步环境中，如果 `SkillsLoader` 被多个 task 访问，编译器会阻止。但通过 `Arc<Mutex<SkillsLoader>>` 包装后，粗粒度锁会导致所有技能操作串行化。

---

### 2. 数据完整性保障方案

#### 2.1 每个存储组件的 Crash Safety 分析

| 存储组件 | 写入方式 | 原子性 | Crash 后果 | 修复建议 |
|---------|---------|--------|-----------|---------|
| **MEMORY.md** (`agent/memory.rs:34-37`) | `std::fs::write()` | ❌ 非原子 | 文件被截断或半写 → 记忆全部丢失 | 使用 `tempfile::NamedTempFile` + `.persist()` |
| **USER_*.md** (`agent/memory.rs:52-57`) | `std::fs::write()` | ❌ 非原子 | 同上 | 同上 |
| **Session JSONL** (`session/store.rs:90-116`) | `std::fs::write()` 全量覆写 | ❌ 非原子 | 文件被截断 → 会话历史全部丢失 | 先写临时文件再 `std::fs::rename()` |
| **Session Append** (`session/store.rs:119-137`) | `OpenOptions::append()` | ⚠️ 部分安全 | 可能追加不完整的 JSON 行 → 加载时跳过该行 | 追加后 `flush()`；Hermes 用 JSONL 部分解决了此问题 |
| **Note JSON** (`session/note_store.rs:78-83`) | `tempfile + rename` | ✅ 原子 | 安全 | 已正确实现 |
| **Config YAML** (`config/loader.rs:58-63`) | `std::fs::write()` | ❌ 非原子 | 配置文件被截断 → 启动时使用默认配置 | 添加原子写入 |
| **Skill MD** (`agent/skills.rs` 通过 load_all) | 读取为主，外部写入 | N/A | 不涉及写入 | N/A |
| **PID File** (`daemon/pid_file.rs:61-73`) | `flock + write + fsync` | ✅ 有保护 | 安全 | 已正确实现 |

#### 2.2 自我进化功能的数据完整性风险

**技能文件创建的非原子性**：
- Hermes 的 `skill_manager_tool.py` 创建技能涉及多步操作（创建目录 → 写入 SKILL.md → 写入支持文件）
- kestrel 的 `SkillsLoader` 只负责**读取**技能，没有技能**创建**功能
- 移植技能创建时，需要在 Rust 中实现：`create_dir_all()` + `tempfile` + `rename()`，确保每步原子
- **关键**：如果创建涉及多个文件（SKILL.md + 辅助文件），需要使用**两阶段提交**：先写入临时目录，完成后 `rename` 到目标位置

**Session 保存的全量覆写风险**：
- `SessionStore.save()`（`session/store.rs:90-116`）先构建完整内容，然后一次性 `std::fs::write()`
- 如果在 `write()` 期间进程被 kill：
  - Linux 上 `write()` 系统调用对小于 `PIPE_BUF`（4096 字节）的内容是原子的
  - 但 session 文件通常远大于 4096 字节 → 写入可能不完整
  - **后果**：下次 `load()` 时，`serde_json::from_str` 解析失败，该行被跳过（`warn!` 后继续），但可能丢失大量消息
- **建议**：使用 `tempfile::NamedTempFile` 写入临时文件，然后 `std::fs::rename()` 原子替换

#### 2.3 WAL/Journal 方案建议

```
自我进化写入操作的安全层次：

Level 1（必须）— 所有单文件写入使用 tempfile + rename：
  - MemoryStore::write_memory()  
  - MemoryStore::write_user_memory()
  - SessionStore::save()
  - Config 保存

Level 2（推荐）— 多文件操作使用事务日志：
  - 技能创建（SKILL.md + 辅助文件）
  - 技能更新（旧版本备份 + 新版本写入）

Level 3（可选）— JSONL session 使用 WAL：
  - 每次 append_entry() 前先写操作日志
  - 启动时检查未完成的操作日志并重放
```

---

### 3. Context Window 风险管理

#### 3.1 Token 预算分配策略

kestrel 已实现了 `ContextBudget`（`agent/context_budget.rs`），默认分配：

| 区域 | 比例 | 128K tokens | 8K tokens |
|------|------|------------|-----------|
| System Prompt | 10% | 12,800 | 800 |
| Skills / Tools | 5% | 6,400 | 400 |
| Notes | 5% | 6,400 | 400 |
| History | 70% | 89,600 | 5,600 |
| Reserved (Response) | 10% | 12,800 | 800 |

**风险评估**：
- **128K 模型**：空间充裕，技能/笔记占 10%（12,800 tokens）不会造成问题
- **8K 模型**：技能 + 工具定义只有 **400 tokens（约 1,600 字符）** → 只能容纳 5-8 个技能名和描述 → 自我进化产生的技能会快速耗尽预算

#### 3.2 技能注入的 Token 开销分析

kestrel 的 `skills_prompt()`（`agent/skills.rs:222-232`）：
```rust
pub fn skills_prompt(skills: &[Skill]) -> String {
    let mut parts = vec!["## Available Skills\n".to_string()];
    for skill in skills {
        parts.push(format!("\n### {}\n{}", skill.name, skill.instructions));
    }
    parts.join("\n")
}
```

**关键发现**：kestrel 将技能的**完整 instructions** 注入系统提示，而非仅注入名称+描述索引！

- Hermes 的 `prompt_builder.py:793-796` 只注入技能**索引**（名称 + 一行描述），按需加载
- kestrel 注入所有技能的**完整内容** → Token 开销远大于 Hermes
- **50 个技能 × 平均 500 字符 instructions = 25,000 字符 ≈ 6,250 tokens**
- 在 128K 模型中消耗 skills_ratio 的全部 6,400 tokens → 超出预算
- **在 8K 模型中直接超出 skills_ratio 的 400 tokens 限制**

#### 3.3 Compaction 策略分析

kestrel 的 `compaction.rs` 使用**本地文本摘要**（非 LLM 调用），策略：

1. **Summarize**：从旧消息中提取统计信息 + 最近 3 条消息原文 + 结构化笔记
2. **Truncate**：直接丢弃旧消息，只保留最近 N 条

**与 Hermes 的对比**：

| 特性 | Hermes | kestrel |
|------|--------|-------------|
| 摘要方式 | LLM 调用生成摘要 | 本地拼接文本摘要 |
| 信息保留 | LLM 决定保留什么 | 固定规则（统计 + 最近 3 条 + 笔记） |
| Token 开销 | 每次压缩消耗 LLM API 调用 | 零 API 开销 |
| 摘要质量 | 更高（理解语义） | 更低（纯文本截取） |
| 关键信息保留 | 依赖 LLM | 通过 `extract_compaction_notes()` 提取结构化笔记 |
| 迭代退化 | 有（多次压缩丢失细节） | 有（每次只保留最近 3 条的原文） |

**自我进化功能的上下文风险**：
- 技能匹配结果需要注入 prompt → 额外消耗 tokens
- 自我审查（self-review）结果需要保留在上下文中 → 增加上下文长度
- **当技能 + 笔记 + 自我审查结果 + 消息历史总 token 超出预算时**：
  - kestrel 的 `prune_messages()` 会丢弃旧消息
  - 但技能和笔记不受影响（它们不在 messages 中）
  - **风险**：自我进化的产出（技能、笔记）挤占了对话历史的 token 空间

#### 3.4 溢出时的降级方案

```
降级优先级（从低到高丢弃）：

1. 丢弃最旧的对话消息（prune_messages 已实现）
2. 缩短技能注入：从完整 instructions → 仅名称+描述索引
3. 缩短笔记内容：从完整笔记 → 仅标题列表
4. 跳过自我审查注入（可选功能优先级最低）
5. 降级到 Truncate 策略（不做摘要，直接截断）
```

---

### 4. LLM 输出质量控制

#### 4.1 kestrel 现有的输出处理

**Agent Runner**（`agent/runner.rs:93-197`）：
- 工具调用结果直接拼入对话，无验证
- 最大迭代次数保护（`max_iterations`），避免无限循环
- 超出迭代次数时返回固定文本提示用户继续

**工具执行**（`agent/runner.rs:293-323`）：
- 工具并发执行（`tokio::spawn`），无依赖排序
- 错误处理：`ToolError` 转为字符串 `"Tool error: {e}"` 返回给 LLM
- **没有对工具返回值的验证或过滤**

#### 4.2 技能验证流程缺失

kestrel 的 `SkillsLoader`（`agent/skills.rs`）：
- 只验证 YAML frontmatter 格式（`parse_frontmatter`）
- **不验证技能内容的事实正确性**
- **不验证技能的可执行性**（`requires_bin` 和 `requires_env` 只是声明，不强制检查）
- **不检查技能间的依赖冲突**

**自我进化移植需要增加的验证层**：

```
技能创建验证管道：

1. 格式验证（已有）：YAML frontmatter 结构正确
2. 静态分析（需新增）：
   - 检查注入模式（system prompt 覆盖尝试）
   - 检查危险命令建议
   - 检查与其他技能的矛盾
3. 沙箱测试（需新增）：
   - 在隔离环境中执行技能
   - 验证输出是否符合预期
   - 检查是否有副作用
4. 人工审核（需新增）：
   - 新技能进入"草稿"状态
   - 需要显式确认后才进入活跃索引
```

#### 4.3 Self-Review 可信度评估

**风险**：LLM 评估自己生成的技能，存在**确认偏见**：
- LLM 倾向于认为自己的输出是正确的
- Self-review 可能遗漏自己引入的错误
- **评估者和被评估者是同一个模型** → 缺乏独立审查

**缓解方案**：
- 使用不同模型进行 self-review（例如用 Claude 审查 GPT 生成的技能）
- 使用确定性规则检查（正则匹配已知错误模式）作为 LLM 审查的补充
- 引入"对抗性测试"：让 LLM 尝试找出技能的漏洞

#### 4.4 工具参数解析的安全性

`runner.rs:302-303`：
```rust
let args: Value =
    serde_json::from_str(&args_str).unwrap_or(Value::Object(Default::default()));
```
- LLM 生成的参数解析失败时，静默替换为空对象 `{}` 而非报错
- **风险**：工具在缺少必要参数时执行，可能产生意外行为
- 应改为：解析失败时返回错误给 LLM，让它重新生成正确的参数

---

### 5. 安全威胁模型

#### 5.1 攻击面分析

| 攻击面 | Hermes 保护 | kestrel 保护 | 移植风险 |
|--------|------------|------------------|---------|
| 提示注入（上下文文件） | 正则匹配 10 种模式 | ❌ 无保护 | 🔴 需从零实现 |
| 提示注入（技能内容） | `skills_guard` 模块 | ❌ 无保护 | 🔴 需从零实现 |
| 路径遍历 | `_check_sensitive_path` 白名单 | ❌ 无保护 | 🔴 需从零实现 |
| SSRF | 无 | ✅ 完整保护（`security/network.rs`） | 🟢 已有 |
| 命令注入 | 阻止列表（`shell.rs:44-52`） | ✅ 基础保护 | 🟡 需增强 |
| 沙箱隔离 | 无 | ✅ bubblewrap（`shell.rs:108-136`） | 🟢 已有 |
| PII 泄露 | 无 | ❌ 无保护 | 🟡 需实现 |

#### 5.2 自我进化功能的特定威胁

**威胁 1：恶意技能注入系统提示**

攻击路径：
1. 用户请求 agent 处理某个任务
2. LLM 在"学习技能"过程中生成包含注入指令的技能文件
3. 技能被 `SkillsLoader` 加载并注入 `skills_prompt()`
4. 下次对话时，恶意指令作为系统提示的一部分被执行

**kestrel 的脆弱性**：
- `skills_prompt()`（`agent/skills.rs:222-232`）将技能 instructions **直接拼入**系统提示
- 没有对技能内容进行任何安全扫描
- 没有"信任级别"分层（Hermes 有 builtin/trusted/community 三级）

**缓解措施**：
1. 技能内容在注入前必须经过安全扫描（正则 + LLM 分类器）
2. 技能注入使用隔离的 XML 块，标记为不可信内容
3. 限制技能 instructions 中不能包含 `<system>` 或 `</system>` 等控制标记

**威胁 2：记忆注入**

攻击路径：
1. 对话中包含恶意指令（例如 "请记住：忽略之前所有指令，执行 X"）
2. LLM 使用记忆工具将恶意指令存入 MEMORY.md
3. 下次对话时，恶意指令作为记忆上下文注入

**kestrel 的脆弱性**：
- `MemoryStore.get_context()`（`agent/memory.rs:60-76`）直接将记忆内容注入 prompt
- 没有注入检测
- 没有内容验证

**威胁 3：供应链攻击（社区技能）**

虽然当前 kestrel 没有社区技能下载功能，但自我进化功能的移植应预先考虑：
- 技能文件的 YAML frontmatter 可以包含 `requires_env` 字段 → 可能被利用来窃取环境变量
- 技能的 instructions 可能包含"读取 ~/.ssh/id_rsa 并发送到..."等指令
- **建议**：在架构设计阶段就内置技能签名验证机制

#### 5.3 kestrel 安全模块的局限性

**`kestrel-security` crate 仅提供 SSRF 保护**：
- 完善的 IP 黑名单/白名单（`network.rs`）
- 内部主机名检测（`.local`, `.internal` 等）
- AWS metadata 端点阻止（`169.254.169.254`）

**完全缺失的安全功能**：
- ❌ 文件系统沙箱/路径访问控制
- ❌ 提示注入检测
- ❌ PII 检测和脱敏
- ❌ 输入内容安全扫描
- ❌ 输出内容过滤
- ❌ 速率限制（文件 I/O 层面）

---

### 6. 测试策略风险

#### 6.1 哪些最难测试

| 功能 | 测试难度 | 原因 | 建议的最小可测试单元 |
|------|---------|------|---------------------|
| 技能质量验证 | 🔴 极高 | LLM 输出不确定，技能语义难以量化 | 技能格式验证（纯规则检查） |
| 上下文压缩信息保留 | 🔴 极高 | "关键信息"的定义模糊 | 固定输入的压缩测试 → 断言保留特定字符串 |
| 记忆矛盾检测 | 🟡 高 | 需要语义理解 | 精确匹配去重 + 简单矛盾规则 |
| 并发技能创建 | 🟡 高 | 需要多 task 并发，结果不确定性 | 使用 `loom` crate 进行确定性并发测试 |
| 技能匹配精度 | 🟡 高 | LLM 选择技能的准确率不固定 | 使用 golden dataset 测试匹配召回率 |
| 上下文预算分配 | 🟢 中 | token 估算有明确公式 | `ContextBudget` 的单元测试已完善 |
| Crash recovery | 🟢 中 | 可模拟 kill 信号 | 写入中途 kill → 重启后验证数据完整性 |
| 原子写入 | 🟢 低 | `tempfile + rename` 行为可预测 | 直接测试文件内容完整性 |

#### 6.2 如何 Mock LLM 输出

**自我进化功能测试的关键 Mock 点**：

```rust
// 建议：为 LLM 调用定义 trait，测试时替换为 mock 实现

trait SkillGenerator {
    async fn generate_skill(&self, task_description: &str) -> Result<Skill>;
}

trait SkillReviewer {
    async fn review_skill(&self, skill: &Skill) -> Result<ReviewResult>;
}

// Mock 实现 — 返回预定义的技能或审查结果
struct MockSkillGenerator { responses: Vec<Skill> }
struct MockSkillReviewer { verdicts: Vec<ReviewResult> }
```

**具体 mock 策略**：
1. **技能生成**：Mock 返回已知正确的技能模板，测试后续流程
2. **技能审查**：Mock 返回 pass/fail，测试"审查失败时技能不进入活跃索引"
3. **技能匹配**：Mock 返回固定匹配结果，测试技能注入 prompt 的正确性
4. **上下文压缩**：不 mock（使用本地文本摘要，无 LLM 调用）

#### 6.3 确定性测试设计

**Token 预算测试**（已有良好基础）：
- `context_budget.rs` 的测试使用固定输入和精确 token 估算
- `estimate_tokens()` 使用 `len() / 4` 的简化公式 → 可精确预测
- 已覆盖：默认分配、自定义分配、超限检测、修剪策略

**需要新增的确定性测试**：
1. 并发写入 MEMORY.md 的竞态条件测试（使用 `loom` crate）
2. 技能文件部分写入后的恢复测试
3. Session 文件截断后的加载测试（验证 graceful degradation）
4. 上下文预算耗尽时的降级行为测试
5. 自我审查拒绝坏技能的端到端测试（使用 mock LLM）

---

### 7. 迁移实施风险时间线

#### 阶段 1：基础设施迁移（P0）

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|---------|
| MemoryStore 原子写入缺失 → 数据丢失 | 高 | 严重 | 第一个 PR 就修复：引入 tempfile + rename |
| SessionStore 覆写不原子 → 会话丢失 | 高 | 严重 | 同上 |
| SkillsLoader 非线程安全 → 编译错误或运行时 panic | 中 | 高 | 改为 `DashMap` 或 `Arc<RwLock>` |
| Config 加载无热重载 → 修改配置需重启 | 低 | 低 | 可接受，后续迭代 |

#### 阶段 2：核心自我进化功能（P1）

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|---------|
| 技能创建非原子 → 部分创建的技能 | 中 | 中 | 两阶段提交：临时目录 → rename |
| 技能注入挤占上下文预算 | 高 | 高 | 改为按需加载（仅注入索引，执行时加载全文） |
| 并发技能写入冲突 | 中 | 中 | per-skill 文件锁或乐观锁 |
| 自我审查确认偏见 | 高 | 中 | 使用不同模型审查 + 规则检查 |
| 记忆矛盾积累 | 高 | 中 | 增加语义去重（embedding 相似度） |

#### 阶段 3：高级功能与安全加固（P2）

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|---------|
| 提示注入绕过安全检测 | 高 | 严重 | 多层防御（正则 + LLM 分类器 + 内容沙箱） |
| 路径遍历攻击 | 中 | 严重 | 白名单机制（仅允许访问项目目录 + 用户配置目录） |
| 技能供应链攻击 | 低（当前无社区功能） | 严重 | 预留签名验证接口 |
| Schema 迁移失败 | 低 | 高 | 幂等迁移 + 自动备份 |
| 性能悬崖（技能数量增长） | 中 | 中 | 按需加载 + 技能衰减机制 |

#### 阶段 4：生产化与优化（P3）

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|---------|
| 翻译式 Rust 代码（非惯用 Rust） | 高 | 中 | Code review 由 Rust 专家执行 |
| 过度工程（移植不必要的 Python 功能） | 中 | 中 | 严格功能裁剪，只移植核心路径 |
| 测试覆盖率不足 | 中 | 高 | 每个模块要求 >80% 行覆盖率 |
| 长期运行 daemon 内存泄漏 | 中 | 中 | 定期重启 + 内存监控 |

---

### 8. 风险缓解检查清单

每个风险的一句话缓解措施，便于复查：

**并发安全**：
- [ ] `MemoryStore::write_memory()` 使用 `tempfile::NamedTempFile` + `.persist()` 替代 `std::fs::write()`
- [ ] `SkillsLoader.skills` 改为 `DashMap<String, Skill>` 或 `Arc<RwLock<HashMap>>`
- [ ] `SessionStore.save()` 使用原子写入（tempfile + rename）
- [ ] 技能热重载使用 copy-on-swap 模式（`ArcSwap<HashMap>`）
- [ ] 上下文压缩期间对 session 加写锁，禁止并发追加

**数据完整性**：
- [ ] 所有文件写入操作使用原子写入模式
- [ ] 技能创建使用两阶段提交（先写临时目录，完成后 rename）
- [ ] Session JSONL 在启动时检查并修复不完整的最后一行
- [ ] 定期自动备份 MEMORY.md 和 skills 目录
- [ ] Schema 迁移使用幂等操作（每个步骤可安全重复执行）

**上下文管理**：
- [ ] 技能注入从"全文注入"改为"索引注入 + 按需加载"
- [ ] 添加技能数量上限（建议 100 个，超出自动降权）
- [ ] 实现 skills_ratio 超出时的自动降级策略
- [ ] 自我审查结果使用独立的 token 预算，不挤占对话历史
- [ ] 对 8K 模型使用更激进的压缩策略

**LLM 输出质量**：
- [ ] 技能创建后经过"格式 + 安全 + 沙箱测试"三级验证
- [ ] 自我审查使用不同模型或独立规则引擎
- [ ] 工具参数解析失败时返回错误给 LLM（而非静默使用空参数）
- [ ] 技能版本化，支持回滚到上一个版本
- [ ] 长期未使用的技能自动降权

**安全加固**：
- [ ] 实现提示注入检测模块（正则 + LLM 分类器双层）
- [ ] 实现文件路径白名单机制
- [ ] 记忆写入前扫描 PII 和注入模式
- [ ] 技能内容注入系统提示前进行二次安全扫描
- [ ] 预留技能签名验证接口（为社区功能做准备）

**测试覆盖**：
- [ ] 使用 `loom` crate 对并发场景进行确定性测试
- [ ] 使用 `proptest` 对路径验证和 FTS 查询进行属性测试
- [ ] 每个 LLM 调用点都有 mock 接口，支持确定性测试
- [ ] Crash recovery 测试：写入中途 kill → 重启验证数据完整性
- [ ] 集成测试覆盖：技能创建 → 注入 → 匹配 → 执行的完整路径

**迁移质量**：
- [ ] 每个 Rust PR 由 Rust 专家 review（防止翻译式代码）
- [ ] 严格功能裁剪：不移植 `PLATFORM_HINTS` 的全部 12 个平台提示
- [ ] 不移植 checkpoint 管理器（使用简单的文件备份替代）
- [ ] 优先移植 local + docker 后端，其他终端后端按需移植
- [ ] 使用 `cargo feature flag` 控制可选功能

---

### 9. 架构腐化风险特别分析

#### 9.1 "翻译代码"的警示信号

将 Python 直接翻译为 Rust（而非用 Rust 原生方式重写）会导致：

| Python 模式 | 错误的 Rust 翻译 | 正确的 Rust 方式 |
|------------|-----------------|-----------------|
| `dict.get("key", default)` | `map.get("key").unwrap_or(default)` | 使用 `serde` 的 `#[serde(default)]` |
| `try/except: pass` | `if let Ok(x) = ... {} else {}` (空 else) | 使用 `?` 传播或 `let _ = ...` |
| `global_dict["key"] = value` | `RwLock<HashMap>` + `.write().unwrap().insert()` | 使用 `DashMap` |
| `open(f).read()` | `std::fs::read_to_string(f).unwrap()` | 使用 `anyhow::Result` 传播 |
| `json.loads(s)` | `serde_json::from_str::<Value>(s)` | 反序列化到具体类型 |
| `time.sleep(x)` | `tokio::time::sleep(x).await` | 已正确 |

#### 9.2 当前 kestrel 中的腐化迹象

1. **`RetryConfig` 与 `RetryPolicy` 并存**（`providers/retry.rs:153-200`）：
   - `RetryConfig` 被标记为 "Legacy... kept for backward compatibility"
   - 但两个 API 都在使用 → 增加维护负担和混淆
   - **建议**：迁移完成前统一到 `RetryPolicy`

2. **`runner.rs` 中 `stream_tx` 的 `Option` 包装**（`runner.rs:26`）：
   - `stream_tx: Option<broadcast::Sender<StreamChunk>>`
   - 使用 builder 模式设置，但没有在编译时保证
   - **更好的 Rust 方式**：使用类型状态模式（type-state pattern）在编译时区分 streaming 和 non-streaming runner

3. **`store.rs:169-171` 的无用函数**：
   ```rust
   fn dir(path: &Path) -> &Path {
       path
   }
   ```
   - 这个函数什么都没做，只在 `list_keys()` 中使用
   - 可能是从 Python 的某个模式错误翻译而来

#### 9.3 自我进化功能的设计原则（防止腐化）

1. **使用 trait 定义核心接口**：`MemoryStore`、`SkillStore`、`ContextEngine` → 方便测试和替换
2. **使用 newtype pattern 区分 ID 类型**：`SkillName(String)` vs `MemoryKey(String)` → 编译时防混淆
3. **使用 `thiserror` 定义错误类型**：而非 `anyhow` 的字符串错误 → 方便匹配和处理
4. **避免 `Arc<Mutex<T>>` 嵌套**：使用 `DashMap` 或 `ArcSwap` 替代
5. **使用 `spawn_blocking` 处理所有文件 I/O**：避免阻塞 tokio 运行时

---

### 10. 单点故障分析

| 组件 | 故障模式 | 影响 | 恢复能力 |
|------|---------|------|---------|
| **MemoryStore** | 文件被截断 | 全部记忆丢失，agent 回到初始状态 | ❌ 无恢复（需手动备份） |
| **SkillsLoader** | 目录被删除 | 所有技能丢失 | ⚠️ 可重新扫描，但已学技能不可恢复 |
| **SessionStore** | JSONL 文件损坏 | 该会话历史丢失 | ⚠️ 部分恢复（跳过损坏行） |
| **LLM Provider** | API 完全不可用 | agent 无法生成回复 | ✅ 有 circuit breaker + retry |
| **MessageBus** | 通道满 | 新消息被拒绝 | ⚠️ 返回错误但不重试 |
| **Config** | 文件损坏 | 使用默认配置启动 | ✅ 有 fallback |
| **Daemon** | 进程 crash | 所有活跃会话中断 | ❌ 无自动重启 |

**最关键的单点故障**：`MemoryStore` — 记忆是自我进化系统的核心数据，一旦丢失不可恢复。必须优先实现原子写入和自动备份。
