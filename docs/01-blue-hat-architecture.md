# 🔵 Blue Hat — Hermes Agent 自演化架构全景分析

## 一、系统总览

Hermes Agent 是一个多平台 AI Agent 系统，核心特征是 **经验驱动的渐进式自演化**：通过记忆持久化、技能自动生成/维护、后台反思审查三个核心机制，使 Agent 在使用过程中持续积累知识和能力，而非仅依赖固定的 prompt 和工具集。

### 1.1 系统架构图（ASCII）

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Hermes Agent 系统架构                            │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ┌──────────────────── 用户交互层 ──────────────────────────┐           │
│  │  CLI (cli.py)    │ Gateway (gateway/run.py)   │ Cron    │           │
│  │  终端交互         │ 多平台消息路由               │ 定时任务  │           │
│  └────────┬─────────┴──────────┬──────────────────┴────┬───┘           │
│           │                    │                       │                │
│           ▼                    ▼                       ▼                │
│  ┌────────────────────────────────────────────────────────────┐         │
│  │                   AIAgent (run_agent.py)                    │         │
│  │  ┌──────────────────────────────────────────────────────┐  │         │
│  │  │            主循环: run_conversation()                  │  │         │
│  │  │                                                       │  │         │
│  │  │  1. 构建系统提示 (_build_system_prompt)                │  │         │
│  │  │     ├─ Identity (SOUL.md / DEFAULT)                    │  │         │
│  │  │     ├─ Memory Snapshot (MEMORY.md / USER.md)           │  │         │
│  │  │     ├─ Skills Index (skills manifest)                  │  │         │
│  │  │     ├─ Context Files (AGENTS.md, .hermes.md, etc.)     │  │         │
│  │  │     ├─ Platform Hints + Environment Hints              │  │         │
│  │  │     └─ Timestamp + Model Info                          │  │         │
│  │  │                                                       │  │         │
│  │  │  2. LLM API 调用 → 工具调用 → 执行 → 循环              │  │         │
│  │  │                                                       │  │         │
│  │  │  3. 后台审查 (_spawn_background_review)  ◄── 核心自演化  │  │         │
│  │  │     ├─ Memory Review (每 N 轮对话)                     │  │         │
│  │  │     └─ Skill Review (每 M 次工具迭代)                   │  │         │
│  │  └──────────────────────────────────────────────────────┘  │         │
│  └────────────────────────────────────────────────────────────┘         │
│           │                    │                       │                 │
│           ▼                    ▼                       ▼                 │
│  ┌──────────────┐  ┌───────────────────┐  ┌─────────────────┐          │
│  │  Memory 系统  │  │   Skills 系统      │  │ Context 压缩    │          │
│  │              │  │                    │  │                 │          │
│  │ MEMORY.md   │  │ SKILL.md 文件       │  │ ContextCompressor│          │
│  │ USER.md     │  │ ~/.hermes/skills/   │  │ 结构化摘要      │          │
│  │ § 分隔符     │  │ references/        │  │ 迭代式更新      │          │
│  │ 冻结快照     │  │ templates/         │  │ 前缀缓存保护    │          │
│  │              │  │ scripts/           │  │                 │          │
│  │ MemoryManager│  │ skill_manage 工具   │  │                 │          │
│  │ 插件化后端   │  │ skill_view 工具     │  │                 │          │
│  └──────────────┘  └───────────────────┘  └─────────────────┘          │
│                                                                         │
│  ┌──────────────────────────────────────────────────────────────┐       │
│  │                     工具层 (tools/)                           │       │
│  │  memory │ skills_list │ skill_view │ skill_manage │ terminal │       │
│  │  session_search │ delegate_task │ cronjob_tools │ web_tools │       │
│  │  browser │ execute_code │ file_tools │ todo │ clarify │ ...  │       │
│  └──────────────────────────────────────────────────────────────┘       │
│                                                                         │
│  ┌──────────────────────────────────────────────────────────────┐       │
│  │                     调度层 (cron/)                            │       │
│  │  scheduler.py (tick 循环, 60s 间隔)                          │       │
│  │  jobs.py (CRUD, 调度解析, 输出存储)                           │       │
│  │  交付: Telegram/Discord/Slack/Signal/Matrix/...              │       │
│  └──────────────────────────────────────────────────────────────┘       │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 二、自演化闭环架构

Hermes Agent 的自演化遵循 **"记录 → 审查 → 结晶 → 增强"** 的闭环：

```
    ┌──────────────────────────────────────────────────────────┐
    │                    自演化闭环                              │
    │                                                          │
    │  用户交互 ──→ 行为记录 ──→ 定期审查 ──→ 策略优化           │
    │      ▲                                    │              │
    │      │              ┌── 技能结晶 ◄─────────┘              │
    │      │              │                                    │
    │      └──────────────┴── 能力增强（下一轮对话加载）          │
    │                                                          │
    └──────────────────────────────────────────────────────────┘
```

### 2.1 各阶段在代码中的实现

| 阶段 | 实现位置 | 关键函数/类 | 机制 |
|------|---------|------------|------|
| **用户交互** | `run_agent.py:7745` | `run_conversation()` | 主对话循环 |
| **行为记录** | `run_agent.py:10580-10614` | 后台审查触发逻辑 | 基于 turn 计数器和 iteration 计数器 |
| **定期审查** | `run_agent.py:2134-2268` | `_spawn_background_review()` + 审查 prompt 模板 | 后台线程中创建 forked AIAgent |
| **策略优化** | `tools/memory_tool.py` | `MemoryStore.add/replace/remove()` | 写入 MEMORY.md / USER.md |
| **技能结晶** | `tools/skill_manager_tool.py` | `skill_manage()` → `create/patch/edit` | 写入 SKILL.md 文件 |
| **能力增强** | `agent/prompt_builder.py` | `_build_system_prompt()` + `build_skills_system_prompt()` | 系统提示组装，注入记忆和技能索引 |

---

## 三、KEPA/GEPA 引擎 —— "反向传播"机制

### 3.1 核心概念

Hermes 没有传统意义上的"KEPA/GEPA"引擎，但存在一个功能等价的 **后台审查系统 (Background Review System)**，其作用类似于神经网络中的反向传播：在任务执行完毕后，回顾执行过程，提取可复用的知识和经验，更新内部表示。

### 3.2 触发机制

```
审查触发器在 run_agent.py 的 run_conversation() 中：

1. Memory Review 触发（基于对话轮次）:
   - _memory_nudge_interval: 默认 10 轮对话（可通过 config.yaml memory.nudge_interval 配置）
   - 计数器: _turns_since_memory（每次 run_conversation 递增）
   - 触发条件: _turns_since_memory >= _memory_nudge_interval
   - 位置: run_agent.py:7871-7878

2. Skill Review 触发（基于工具迭代次数）:
   - _skill_nudge_interval: 默认 10 次工具调用迭代（可通过 config.yaml skills.creation_nudge_interval 配置）
   - 计数器: _iters_since_skill（每次工具调用迭代递增）
   - 触发条件: _iters_since_skill >= _skill_nudge_interval
   - 位置: run_agent.py:8121-8125, 10587-10592

3. 审查执行:
   - 时机: 在主对话返回响应之后（后台线程），不阻塞用户交互
   - 位置: run_agent.py:10606-10614
   - 方式: _spawn_background_review() 在后台线程中运行
```

### 3.3 审查过程

`_spawn_background_review()` (run_agent.py:2169-2268) 的工作流程：

1. **创建 forked AIAgent**：复制当前会话的模型、工具和上下文
2. **注入审查 prompt**：根据触发类型选择不同的 prompt 模板
   - `_MEMORY_REVIEW_PROMPT` (run_agent.py:2134-2143)：关注用户偏好和个人信息
   - `_SKILL_REVIEW_PROMPT` (run_agent.py:2145-2153)：关注可复用的任务方法论
   - `_COMBINED_REVIEW_PROMPT` (run_agent.py:2155-2167)：两者兼有
3. **在后台 Agent 中执行**：forked Agent 审查对话历史，自主决定是否保存
4. **写入持久存储**：通过 memory/skill_manage 工具直接写入磁盘
5. **反馈摘要**：输出保存动作的摘要给用户

### 3.4 输出生成

审查 Agent 的输出是 **直接的工具调用动作**：
- 使用 `memory` 工具写入 MEMORY.md 或 USER.md
- 使用 `skill_manage` 工具创建或更新 SKILL.md
- 如果没有值得保存的内容，输出 "Nothing to save." 并停止

---

## 四、自动技能生成管线

### 4.1 技能格式

技能是 Markdown 文件，使用 YAML frontmatter + Markdown body 格式：

```yaml
---
name: skill-name              # 必需，最大 64 字符
description: Brief description # 必需，最大 1024 字符
version: 1.0.0                # 可选
license: MIT                  # 可选
platforms: [macos, linux]     # 可选，平台限制
prerequisites:                # 可选，运行前提
  commands: [curl, jq]
metadata:                     # 可选，agentskills.io 标准
  hermes:
    tags: [fine-tuning, llm]
    related_skills: [peft, lora]
    config:
      - key: config.key
        description: Description
        default: value
---

# Skill Title

Full instructions and content here...
```

### 4.2 技能存储结构

```
~/.hermes/skills/                    # 本地技能目录（单一数据源）
├── my-skill/
│   ├── SKILL.md                     # 主指令文件（必需）
│   ├── references/                  # 参考文档
│   │   └── api-guide.md
│   ├── templates/                   # 模板文件
│   │   └── config.yaml
│   ├── scripts/                     # 脚本文件
│   │   └── deploy.sh
│   └── assets/                      # 资源文件
├── category-name/
│   ├── DESCRIPTION.md               # 类别描述
│   └── another-skill/
│       └── SKILL.md
└── index-cache/                     # 技能索引缓存
    └── *.json

/opt/hermes-research/skills/         # 内置技能（随代码分发）
/opt/hermes-research/optional-skills/ # 可选技能包
```

### 4.3 技能生命周期

```
创建 → 使用 → 维护 → 进化

1. 创建 (skill_manage action='create'):
   - 由后台审查自动生成，或 Agent 在完成复杂任务后主动创建
   - 由用户通过 /skill-name 命令触发
   - 验证：名称、frontmatter 必需字段、内容大小限制、安全扫描

2. 使用 (skill_view):
   - 通过系统提示中的 Skills Index 发现
   - 通过 skill_view(name) 加载完整内容
   - 通过 skill_view(name, file_path) 加载参考/模板文件

3. 维护 (skill_manage action='patch'):
   - 后台审查中自动更新过时的技能
   - Agent 在使用技能发现问题后立即 patch
   - 系统提示中引导："If a skill you loaded was missing steps, had wrong commands,
     or needed pitfalls you discovered, update it before finishing."

4. 进化:
   - 技能内容随经验积累持续优化
   - 新的参考文件、模板可通过 write_file 添加
   - 旧技能可被删除或替换
```

### 4.4 技能匹配与加载

技能加载采用 **渐进式披露 (Progressive Disclosure)** 架构：

| 层级 | 内容 | 工具 | Token 开销 |
|------|------|------|-----------|
| Tier 1 | 名称 + 描述 | `skills_list()` | 最小（元数据） |
| Tier 2 | SKILL.md 完整内容 | `skill_view(name)` | 中等 |
| Tier 3 | 参考文件/模板/脚本 | `skill_view(name, file_path)` | 按需加载 |

**系统提示中的技能索引**（`prompt_builder.py:build_skills_system_prompt()`）：
- 在系统提示中注入所有技能的名称和描述列表
- 带有两层缓存：进程内 LRU + 磁盘快照
- 本地技能优先于外部技能目录

---

## 五、记忆系统架构

### 5.1 双层记忆架构

```
┌─────────────────────────────────────────────────┐
│              MemoryManager (编排层)               │
│  agent/memory_manager.py                        │
│                                                 │
│  ┌──────────────────┐  ┌──────────────────┐     │
│  │ BuiltinProvider   │  │ ExternalProvider  │     │
│  │ (始终活跃)         │  │ (最多1个)         │     │
│  │                  │  │                  │     │
│  │ MEMORY.md        │  │ Honcho / Mem0    │     │
│  │ USER.md          │  │ Hindsight / ...  │     │
│  │ MemoryStore      │  │ (plugins/memory/) │     │
│  └──────────────────┘  └──────────────────┘     │
│                                                 │
│  生命周期:                                       │
│  initialize → system_prompt_block → prefetch →  │
│  sync_turn → queue_prefetch → on_session_end →  │
│  on_pre_compress → shutdown                     │
└─────────────────────────────────────────────────┘
```

### 5.2 内置记忆 (MemoryStore)

**存储格式**（`tools/memory_tool.py`）：
- `~/.hermes/memories/MEMORY.md`：Agent 的个人笔记（环境事实、项目约定、工具特性）
- `~/.hermes/memories/USER.md`：用户档案（偏好、沟通风格、工作习惯）
- 条目使用 `§` (section sign) 分隔
- 字符限制：MEMORY 2200 字符，USER 1375 字符

**冻结快照模式**：
- 系统提示中的记忆是 **会话开始时的冻结快照**（`_system_prompt_snapshot`）
- 会话中通过工具写入的内容 **立即持久化到磁盘**，但 **不更新系统提示**
- 这样保护了前缀缓存（prefix cache），确保所有轮次使用相同的系统提示
- 下一次会话开始时加载最新的磁盘内容

**安全扫描**：
- 写入前检测注入模式（prompt_injection, role_hijack, exfil_curl 等）
- 检测不可见 Unicode 字符
- 原子写入（tempfile + os.replace）防止竞态条件

### 5.3 记忆注入机制

记忆在系统提示中的位置（`_build_system_prompt()`, run_agent.py:3121-3286）：

```
系统提示组装顺序:
1. Agent Identity (SOUL.md 或 DEFAULT_AGENT_IDENTITY)
2. 用户/网关系统提示
3. 工具行为指导 (MEMORY_GUIDANCE, SESSION_SEARCH_GUIDANCE, SKILLS_GUIDANCE)
4. Nous 订阅信息
5. 工具使用强制指导（模型特定）
6. ★ 持久化记忆快照 (MEMORY.md) ★
7. ★ 用户档案快照 (USER.md) ★
8. ★ 外部记忆提供者系统提示 ★
9. 技能索引
10. 上下文文件 (AGENTS.md, .hermes.md, .cursorrules)
11. 时间戳 + 模型信息
12. 环境提示 (WSL等)
13. 平台提示 (Telegram/Discord/CLI等)
```

### 5.4 外部记忆提供者

**插件架构**（`agent/memory_provider.py` 抽象基类）：

可用插件（`plugins/memory/` 目录）：
- `honcho` — Honcho 对话记忆平台
- `mem0` — Mem0 记忆管理
- `hindsight` — Hindsight 记忆引擎
- `byterover`, `holographic`, `openviking`, `retaindb`, `supermemory`

**MemoryManager 规则**：
- 内置提供者始终活跃，不可移除
- 最多 **1个** 外部提供者（防止工具 schema 冲突）
- 失败隔离：一个提供者的错误不会阻塞另一个

### 5.5 记忆与技能的关系

| 维度 | Memory (MEMORY.md/USER.md) | Skills (SKILL.md) |
|------|---------------------------|-------------------|
| 类型 | 声明性知识 | 程序性知识 |
| 内容 | 用户偏好、环境事实、工具特性 | 任务步骤、工作流、方法论 |
| 粒度 | 宽泛、通用 | 窄化、可操作 |
| 存储 | § 分隔的文本条目 | 独立的 Markdown 文件 |
| 容量 | 受字符限制（~2.2K / ~1.4K） | 每个技能最大 100K 字符 |
| 注入 | 系统提示（冻结快照） | 按需加载（progressive disclosure） |
| 触发 | 对话轮次计数器 | 工具迭代计数器 |

---

## 六、上下文工程 (Context Engineering)

### 6.1 系统提示动态构建

系统提示在首次调用时构建，之后在整个会话中保持稳定（缓存）：

```python
# run_agent.py:3121
def _build_system_prompt(self, system_message=None):
    """
    7层系统提示组装:
    1. Agent Identity (SOUL.md / DEFAULT)
    2. User/Gateway system prompt
    3. Memory snapshot (frozen at load time)
    4. Skills guidance + skills index
    5. Context files (AGENTS.md, .hermes.md, etc.)
    6. Date/time + model info
    7. Platform + environment hints
    """
```

**缓存策略**：
- `_cached_system_prompt`: 首次构建后缓存
- 上下文压缩事件触发 `_invalidate_system_prompt()` 强制重建
- 重建时从磁盘重新加载记忆（捕获本次会话中的写入）
- 网关模式下从 SQLite 恢复系统提示（保护 Anthropic 前缀缓存）

### 6.2 JIT 加载机制

```
┌─────────────────────────────────────────────────┐
│              JIT 加载层次                         │
│                                                 │
│ 会话开始时加载:                                   │
│  ├─ SOUL.md (完整加载)                           │
│  ├─ MEMORY.md / USER.md (冻结快照)               │
│  ├─ Skills Index (名称+描述列表)                  │
│  └─ Context Files (AGENTS.md 等，截断至 20K)     │
│                                                 │
│ API 调用时注入:                                   │
│  ├─ Ephemeral system prompt (API-only)           │
│  ├─ External memory prefetch (用户消息中)         │
│  └─ Plugin context (用户消息中)                   │
│                                                 │
│ 按需加载:                                        │
│  ├─ skill_view(name) → 完整 SKILL.md             │
│  ├─ skill_view(name, path) → 参考文件            │
│  └─ session_search() → 历史会话检索              │
│                                                 │
│ 后台加载:                                        │
│  └─ Memory provider queue_prefetch()             │
└─────────────────────────────────────────────────┘
```

### 6.3 上下文压缩 (Context Compression)

`agent/context_compressor.py` — `ContextCompressor` 类：

```
压缩算法:
1. 预剪枝旧工具结果（无需 LLM 调用）
2. 保护头部消息（系统提示 + 首次交互，默认 3 条）
3. 保护尾部消息（基于 token 预算，约 20K tokens）
4. 结构化摘要中间轮次（LLM 调用）
5. 后续压缩时迭代更新之前的摘要

摘要模板:
  ## Goal / ## Constraints & Preferences / ## Progress (Done/In Progress/Blocked)
  ## Key Decisions / ## Resolved Questions / ## Pending User Asks
  ## Relevant Files / ## Remaining Work / ## Critical Context / ## Tools & Patterns
```

**关键设计**：
- 迭代式摘要更新（非从头重建）
- 工具调用/结果配对完整性保护
- 失败时有静态 fallback 标记
- 焦点压缩（`/compact <topic>`）

---

## 七、集成点与模块依赖

### 7.1 模块依赖图

```
                        ┌─────────┐
                        │  cli.py │ (CLI 入口, 446KB)
                        │  hermes_cli/ │ (CLI 框架)
                        └────┬────┘
                             │
                    ┌────────▼────────┐
                    │   run_agent.py  │ (核心: AIAgent, 551KB)
                    │                 │
                    │  ┌──────────┐   │
                    │  │ Memory   │   │ ← agent/memory_manager.py
                    │  │ Manager  │   │ ← agent/memory_provider.py
                    │  └────┬─────┘   │ ← tools/memory_tool.py
                    │       │         │
                    │  ┌────▼─────┐   │
                    │  │ Prompt   │   │ ← agent/prompt_builder.py
                    │  │ Builder  │   │
                    │  └────┬─────┘   │
                    │       │         │
                    │  ┌────▼─────┐   │
                    │  │ Context  │   │ ← agent/context_compressor.py
                    │  │Compressor│   │
                    │  └──────────┘   │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
     ┌────────▼───┐  ┌──────▼──────┐  ┌───▼────────┐
     │  tools/    │  │  cron/      │  │ gateway/   │
     │            │  │             │  │            │
     │ registry   │  │ scheduler   │  │ run.py     │
     │ memory     │  │ jobs        │  │ session    │
     │ skills_*   │  │             │  │ delivery   │
     │ session_*  │  └─────────────┘  │ platforms/ │
     │ delegate   │                   │ hooks      │
     │ terminal   │  ┌─────────────┐  └────────────┘
     │ web_*      │  │ plugins/    │
     │ browser_*  │  │             │
     │ ...        │  │ memory/     │ ← 记忆提供者插件
     └────────────┘  │ context_engine/ │
                     └─────────────┘
```

### 7.2 自演化组件集成

| 组件 | 依赖 | 被依赖 | 集成方式 |
|------|------|--------|---------|
| **MemoryStore** | memory_tool.py, memory_provider.py | AIAgent.__init__, _build_system_prompt | 工具注册 + 系统提示注入 |
| **MemoryManager** | memory_provider.py, plugins/memory/ | AIAgent.__init__, run_conversation | 工具 schema 注入 + prefetch/sync |
| **SkillManager** | skills_tool.py, skill_manager_tool.py | AIAgent.__init__, skill_commands.py | 工具注册 + 系统提示索引 |
| **BackgroundReview** | AIAgent (fork), memory_tool, skill_manage | run_conversation (post-turn) | 后台线程 + 共享 store |
| **ContextCompressor** | auxiliary_client.py, context_engine.py | run_conversation (pre-flight + mid-loop) | 消息列表替换 |
| **CronScheduler** | AIAgent (新实例), jobs.py | gateway/run.py (ticker) | 独立 AIAgent 实例 |
| **PromptBuilder** | skill_utils.py, hermes_constants.py | AIAgent._build_system_prompt | 静态函数调用 |

### 7.3 数据流图

```
用户消息 ──→ gateway/cli ──→ AIAgent.run_conversation()
                                  │
                                  ├── 1. 构建/缓存系统提示
                                  │      ├─ SOUL.md → identity 层
                                  │      ├─ MEMORY.md → 冻结快照注入
                                  │      ├─ USER.md → 冻结快照注入
                                  │      ├─ Skills index → 技能索引注入
                                  │      └─ AGENTS.md → 上下文文件注入
                                  │
                                  ├── 2. 前飞压缩检查 (preflight compression)
                                  │
                                  ├── 3. 外部记忆预取 (prefetch)
                                  │      └─ memory_manager.prefetch_all()
                                  │
                                  ├── 4. 插件上下文注入 (pre_llm_call hook)
                                  │
                                  ├── 5. 主循环: LLM API → 工具调用 → 执行 → 循环
                                  │      │
                                  │      ├── 工具调用分发:
                                  │      │   ├─ memory → MemoryStore (磁盘写入)
                                  │      │   ├─ skill_view → 加载 SKILL.md (JIT)
                                  │      │   ├─ skill_manage → 创建/更新/删除技能
                                  │      │   ├─ session_search → SQLite 历史查询
                                  │      │   ├─ delegate_task → 子 Agent (继承 store)
                                  │      │   └─ ...其他工具
                                  │      │
                                  │      └── 上下文压缩 (如果超过阈值)
                                  │          └─ ContextCompressor.compress()
                                  │
                                  ├── 6. 后处理:
                                  │      ├─ 计数器更新 (_turns_since_memory, _iters_since_skill)
                                  │      ├─ 外部记忆同步 (sync_all)
                                  │      └─ 后台审查触发检查
                                  │
                                  └── 7. 后台审查 (_spawn_background_review) [异步]
                                         │
                                         ├─ 创建 forked AIAgent (共享 MemoryStore)
                                         ├─ 注入审查 prompt
                                         ├─ forked Agent 审查对话历史
                                         ├─ 调用 memory 工具 → 更新 MEMORY.md / USER.md
                                         ├─ 调用 skill_manage 工具 → 更新/创建 SKILL.md
                                         └─ 输出审查摘要

下次会话 → 加载更新后的 MEMORY.md + USER.md + Skills Index
```

---

## 八、关键数据结构及关系

### 8.1 核心数据结构

```
┌─────────────────────────────────────────────────────────────────┐
│ AIAgent (run_agent.py:526)                                      │
│                                                                 │
│ 会话状态:                                                        │
│   _cached_system_prompt: str        # 缓存的系统提示              │
│   _memory_store: MemoryStore        # 内置记忆存储                │
│   _memory_manager: MemoryManager    # 外部记忆编排器              │
│   _memory_enabled: bool             # MEMORY.md 开关             │
│   _user_profile_enabled: bool       # USER.md 开关               │
│   _memory_nudge_interval: int       # 记忆审查间隔 (默认10轮)      │
│   _skill_nudge_interval: int        # 技能审查间隔 (默认10次迭代)  │
│   _turns_since_memory: int          # 距上次记忆审查的轮次          │
│   _iters_since_skill: int           # 距上次技能审查的迭代数        │
│   tools: List[Dict]                 # 工具 schema 列表            │
│   valid_tool_names: Set[str]        # 可用工具名集合              │
│   session_id: str                   # 会话标识                    │
│   compression_count: int            # 压缩次数                    │
│                                                                 │
│ 回调:                                                           │
│   tool_progress_callback           # 工具进度回调                 │
│   stream_delta_callback            # 流式 token 回调              │
│   step_callback                    # 步骤回调                     │
│   background_review_callback       # 后台审查结果回调              │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ MemoryStore (tools/memory_tool.py:95)                           │
│                                                                 │
│   memory_entries: List[str]         # Agent 笔记条目             │
│   user_entries: List[str]           # 用户档案条目                │
│   memory_char_limit: int = 2200     # Memory 字符上限            │
│   user_char_limit: int = 1375       # User 字符上限              │
│   _system_prompt_snapshot: Dict     # 冻结的系统提示快照          │
│                                                                 │
│   文件: MEMORY.md, USER.md (§ 分隔)                              │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ SKILL.md (技能文件)                                              │
│                                                                 │
│   frontmatter:                                                    │
│     name: str (必需, ≤64 chars)                                   │
│     description: str (必需, ≤1024 chars)                          │
│     version: str (可选)                                           │
│     platforms: List[str] (可选)                                    │
│     prerequisites: Dict (可选)                                    │
│     metadata.hermes.tags: List[str] (可选)                        │
│     metadata.hermes.related_skills: List[str] (可选)              │
│     metadata.hermes.config: List[Dict] (可选)                     │
│   body: str (Markdown 指令内容)                                    │
│                                                                 │
│   目录结构:                                                      │
│     SKILL.md + references/ + templates/ + scripts/ + assets/      │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ CronJob (cron/jobs.py)                                          │
│                                                                 │
│   id: str                           # UUID[:12]                  │
│   name: str                         # 友好名称                   │
│   prompt: str                       # 执行提示                    │
│   skills: List[str]                 # 关联技能                   │
│   schedule: Dict                    # {kind, minutes/expr/run_at} │
│   next_run_at: str                  # ISO 时间戳                  │
│   last_run_at: str | None           # 上次运行时间                │
│   deliver: str                      # 交付目标                    │
│   origin: Dict                      # 创建来源                   │
│   enabled: bool                     # 启用状态                    │
│   repeat: Dict                      # {times, completed}          │
│                                                                 │
│   存储: ~/.hermes/cron/jobs.json                                  │
│   输出: ~/.hermes/cron/output/{job_id}/{timestamp}.md             │
└─────────────────────────────────────────────────────────────────┘
```

### 8.2 数据结构关系

```
AIAgent 1 ──→ 1 MemoryStore (内置)
AIAgent 1 ──→ 0..1 MemoryManager (外部)
AIAgent 1 ──→ 1 ContextCompressor
AIAgent * ──→ 0..* SKILL.md (通过 skill_view JIT 加载)
AIAgent 1 ──→ 0..* CronJob (通过 cron scheduler)

MemoryStore 1 ──→ 2 文件 (MEMORY.md, USER.md)
MemoryManager 1 ──→ 1..2 MemoryProvider (builtin + 0..1 external)
CronJob 1 ──→ 0..* SKILL.md (通过 skills 字段关联)
CronJob * ──→ 1 AIAgent (每次执行创建新实例)
```

---

## 九、逐文件自演化相关代码分析

### 9.1 run_agent.py (551KB) — 核心自演化引擎

| 行号范围 | 功能 | 自演化角色 |
|---------|------|-----------|
| 526-750 | `AIAgent.__init__()` | 初始化所有自演化组件 |
| 1132-1155 | 记忆初始化 | MemoryStore 加载，配置 nudge interval |
| 1159-1224 | MemoryManager 初始化 | 外部记忆插件加载 |
| 1236-1241 | 技能配置 | skill nudge interval 配置 |
| 2134-2167 | 审查 prompt 模板 | 3 种审查 prompt 定义 |
| 2169-2268 | `_spawn_background_review()` | **核心自演化机制**：后台审查线程 |
| 3121-3286 | `_build_system_prompt()` | 系统提示组装（注入记忆+技能） |
| 3448-3457 | `_invalidate_system_prompt()` | 压缩后强制重建（重载记忆） |
| 7745-10639 | `run_conversation()` | 主循环：触发审查、压缩、同步 |
| 7871-7878 | 记忆审查触发检查 | 基于 _turns_since_memory 计数器 |
| 8121-8125 | 技能审查触发检查 | 基于 _iters_since_skill 计数器 |
| 10587-10592 | 技能审查触发判定 | 响应后检查 iteration 阈值 |
| 10597-10601 | 外部记忆同步 | sync_all + queue_prefetch_all |
| 10606-10614 | 后台审查执行 | 调用 _spawn_background_review |

### 9.2 agent/prompt_builder.py — 系统提示组装

| 行号范围 | 功能 |
|---------|------|
| 1-73 | 上下文文件安全扫描（注入检测） |
| 134-156 | DEFAULT_AGENT_IDENTITY, MEMORY_GUIDANCE, SKILLS_GUIDANCE 常量 |
| 286-386 | PLATFORM_HINTS（平台特定指导） |
| 405-414 | 环境检测 (WSL) |
| 447-528 | 技能快照缓存（磁盘 LRU + manifest） |
| 581-806 | `build_skills_system_prompt()` — 技能索引构建与缓存 |
| 891-916 | `load_soul_md()` — SOUL.md 加载 |
| 1004-1043 | `build_context_files_prompt()` — 上下文文件发现与加载 |

### 9.3 agent/context_compressor.py — 上下文压缩

| 行号范围 | 功能 |
|---------|------|
| 60-170 | `__init__()` — 压缩器初始化（阈值、预算、摘要模型） |
| 186-241 | `_prune_old_tool_results()` — 旧工具结果预剪枝 |
| 318-483 | `_generate_summary()` — 结构化 LLM 摘要生成 |
| 506-598 | 工具调用配对完整性保护 |
| 604-660 | Token 预算尾部保护 |
| 666-820 | `compress()` — 主压缩入口 |

### 9.4 tools/memory_tool.py — 持久化记忆

| 行号范围 | 功能 |
|---------|------|
| 55-92 | 内容安全扫描 |
| 96-378 | `MemoryStore` 类 — 核心记忆存储 |
| 193-236 | `add()` — 添加条目（安全扫描 + 去重 + 容量检查） |
| 238-294 | `replace()` — 替换条目（模糊匹配） |
| 296-328 | `remove()` — 删除条目 |
| 330-341 | `format_for_system_prompt()` — 冻结快照返回 |
| 434-472 | `memory_tool()` — 工具入口函数 |
| 484-533 | MEMORY_SCHEMA — OpenAI 工具 schema |

### 9.5 tools/skills_tool.py — 技能查看

| 行号范围 | 功能 |
|---------|------|
| 1-67 | 技能格式规范文档 |
| 107-120 | `load_env()` — 环境变量加载 |
| 419-427 | `_parse_frontmatter()` — YAML frontmatter 解析 |
| 513-587 | `_find_all_skills()` — 递归技能发现 |
| 633-698 | `skills_list()` — 列表工具（Tier 1） |
| 701-1163 | `skill_view()` — 查看工具（Tier 2-3） |

### 9.6 tools/skill_manager_tool.py — 技能管理

| 行号范围 | 功能 |
|---------|------|
| 292-346 | `_create_skill()` — 创建新技能 |
| 349-379 | `_edit_skill()` — 完全重写技能 |
| 382-467 | `_patch_skill()` — 定向替换（推荐方式） |
| 470-487 | `_delete_skill()` — 删除技能 |
| 490-539 | `_write_file()` / `_remove_file()` — 支持文件管理 |
| 588-646 | `skill_manage()` — 主入口函数 |
| 653-761 | SKILL_MANAGE_SCHEMA — 工具 schema |

### 9.7 agent/memory_manager.py — 记忆编排

| 行号范围 | 功能 |
|---------|------|
| 48-68 | `sanitize_context()` / `build_memory_context_block()` — 上下文围栏 |
| 71-362 | `MemoryManager` 类 — 编排所有记忆提供者 |
| 145-162 | `build_system_prompt()` — 收集所有提供者的系统提示块 |
| 166-183 | `prefetch_all()` — 预取所有提供者的上下文 |
| 198-207 | `sync_all()` — 同步所有提供者的 turn |
| 239-255 | `handle_tool_call()` — 路由工具调用到正确的提供者 |
| 303-317 | `on_memory_write()` — 内置写入时通知外部提供者 |

### 9.8 agent/memory_provider.py — 记忆提供者抽象

| 行号范围 | 功能 |
|---------|------|
| 1-31 | 模块文档：生命周期定义 |
| 42-232 | `MemoryProvider` ABC — 抽象基类 |
| 84-89 | `system_prompt_block()` — 系统提示贡献 |
| 92-105 | `prefetch()` — 预取上下文 |
| 114-119 | `sync_turn()` — 同步 turn |
| 121-129 | `get_tool_schemas()` — 工具 schema 声明 |
| 144-162 | `on_session_end()` / `on_pre_compress()` — 可选钩子 |
| 175-186 | `on_delegation()` — 子 Agent 完成通知 |

### 9.9 cron/jobs.py + cron/scheduler.py — 定时调度

| 文件 | 行号范围 | 功能 |
|------|---------|------|
| jobs.py | 117-203 | `parse_schedule()` — 调度解析 |
| jobs.py | 368-467 | `create_job()` — 任务创建 |
| jobs.py | 580-627 | `mark_job_run()` — 运行后状态更新 |
| jobs.py | 658-734 | `get_due_jobs()` — 到期任务查询 |
| scheduler.py | 487-574 | `_build_job_prompt()` — 构建执行 prompt（含技能加载） |
| scheduler.py | 577-897 | `run_job()` — 执行任务（创建独立 AIAgent） |
| scheduler.py | 899-992 | `tick()` — 调度 tick（60s 间隔，文件锁） |

### 9.10 agent/skill_commands.py — 技能命令

| 行号范围 | 功能 |
|---------|------|
| 45-79 | `_load_skill_payload()` — 加载技能 payload |
| 82-118 | `_inject_skill_config()` — 注入技能配置值 |
| 121-197 | `_build_skill_message()` — 构建技能消息 |
| 200-262 | `scan_skill_commands()` — 扫描 `/skill-name` 命令 |
| 291-326 | `build_skill_invocation_message()` — 构建调用消息 |
| 329-368 | `build_preloaded_skills_prompt()` — 预加载技能 |

### 9.11 gateway/run.py — 网关消息处理

| 功能 | 描述 |
|------|------|
| 会话管理 | 每个 session 缓存 AIAgent 实例，保护前缀缓存 |
| 技能自动加载 | 根据频道/话题绑定自动加载技能 |
| Cron 集成 | 后台 ticker 线程，60s 间隔 |
| 上下文压缩 | 大会话自动压缩（85% 阈值） |
| 平台适配 | 多平台消息路由与格式化 |

---

## 十、总结：Hermes 的自演化 vs 传统 Agent

### 10.1 传统 Agent 架构

```
传统 Agent:
  System Prompt (固定) → LLM → 工具调用 → 响应
  ↑                                    │
  └────────────────────────────────────┘
  （无状态循环，每次对话独立，无积累）
```

### 10.2 Hermes Agent 自演化架构

```
Hermes Agent:
  System Prompt (动态: Identity + Memory + Skills + Context)
      → LLM → 工具调用 → 响应
      → 后台审查 → 更新 Memory / Skills → 影响下次系统提示
      → 上下文压缩 → 保护关键信息 → 延长对话能力

  （闭环积累，每次对话都可能更新知识库，系统随使用进化）
```

### 10.3 关键差异

| 维度 | 传统 Agent | Hermes Agent |
|------|-----------|-------------|
| **知识持久化** | 无 / 仅 session 内 | MEMORY.md + USER.md 跨会话持久 |
| **程序性知识** | 无 | SKILL.md 文件自动创建/维护 |
| **自我审查** | 无 | 后台线程定期审查对话，主动提取知识 |
| **上下文管理** | 简单截断 | 结构化压缩 + 迭代摘要 + 前缀缓存保护 |
| **记忆注入** | 无 | 冻结快照注入系统提示，外部提供者 JIT 预取 |
| **技能进化** | 无 | 发现问题时立即 patch，审查时创建新技能 |
| **跨会话学习** | 无 | session_search 检索历史 + 记忆跨会话携带 |
| **调度能力** | 无 | Cron 系统 + 技能绑定 + 多平台交付 |
| **插件扩展** | 硬编码 | 记忆提供者、上下文引擎、技能包均插件化 |

### 10.4 自演化的本质

Hermes Agent 的自演化不是"修改自身代码"或"改变模型权重"，而是 **通过结构化地积累和应用经验知识来持续增强能力**：

1. **记忆积累**：从每次对话中提取值得记住的事实和偏好
2. **技能结晶**：将成功的方法论转化为可复用的文档化流程
3. **技能维护**：主动发现和修复过时的技能
4. **上下文保护**：通过结构化压缩保留关键信息
5. **经验回放**：通过 session_search 检索历史经验

这种"自演化"的边界在于：它不改变推理逻辑本身（那是 LLM 的能力），而是在 LLM 的输入上下文中不断增加高质量的知识和指导，使同样的模型在面对类似任务时表现得越来越好。

---

## 迁移架构评估

### 1. 架构差距矩阵

以下矩阵对比 Hermes 自演化系统的每个核心组件与 kestrel 现有实现之间的差距。

| # | 组件 | kestrel 现状 | 缺失程度 | 预估工作量 |
|---|------|-------------------|---------|-----------|
| C1 | **持久化记忆 (MEMORY.md/USER.md)** | 仅有 NotesManager（结构化笔记），无自由文本记忆存储、无冻结快照、无安全扫描 | **严重缺失** | 5-8 人天 |
| C2 | **后台审查系统** | 完全不存在。无 forked agent、无后台线程审查、无计数器触发机制 | **完全缺失** | 8-12 人天 |
| C3 | **技能管理 (create/patch/delete)** | SkillsLoader 仅支持加载，无 skill_manage 工具，无创建/更新/删除能力 | **严重缺失** | 5-7 人天 |
| C4 | **技能渐进式披露** | 无 skill_view 工具，无 Tier 1/2/3 分层，技能内容一次性全量注入 | **严重缺失** | 3-5 人天 |
| C5 | **上下文压缩 (Context Compression)** | 仅有简单的消息截断（prune_messages），无 LLM 驱动的结构化摘要、无迭代更新 | **中度缺失** | 6-10 人天 |
| C6 | **动态系统提示构建** | ContextBuilder 存在但极为简单：身份+运行时元数据+工具列表。缺少记忆注入、技能索引注入、上下文文件发现、SOUL.md 加载 | **中度缺失** | 4-6 人天 |
| C7 | **外部记忆提供者插件** | 完全不存在。无 MemoryProvider trait 等价物、无插件加载机制 | **完全缺失** | 5-8 人天 |
| C8 | **跨会话搜索 (session_search)** | SessionManager 有基本的 note 搜索（search_all_notes），但无对话内容全文检索 | **轻度缺失** | 3-4 人天 |
| C9 | **SOUL.md 身份系统** | ContextBuilder 使用 `config.name` 作为身份，无 SOUL.md 文件加载、无 DEFAULT_AGENT_IDENTITY 等价物 | **轻度缺失** | 1-2 人天 |
| C10 | **上下文文件发现** | 无 AGENTS.md / .hermes.md / .cursorrules 等项目上下文文件的自动发现和注入 | **轻度缺失** | 2-3 人天 |
| C11 | **委托任务 (delegate_task)** | SubAgentManager 存在且功能完善，支持并行执行、超时、工具继承 | **基本完备** | 0.5-1 人天（适配） |
| C12 | **Cron 调度系统** | kestrel-cron 存在，支持 tick 调度、JSON 状态、CronTool 工具 | **基本完备** | 1-2 人天（增强） |

**总预估工作量：44-68 人天**

### 2. 模块映射表

| Hermes 模块 | Hermes 实现位置 | kestrel 对应位置 | 操作 |
|------------|----------------|---------------------|------|
| **MemoryStore** (内置记忆) | `tools/memory_tool.py` | 无直接对应。NotesManager 最接近但设计不同 | **新建** → `kestrel-memory` crate |
| **MemoryManager** (记忆编排) | `agent/memory_manager.py` | 无对应 | **新建** → `kestrel-memory` crate |
| **MemoryProvider** (外部记忆插件) | `agent/memory_provider.py` + `plugins/memory/` | 无对应 | **新建** → `kestrel-memory` crate（含 trait + 插件目录） |
| **PromptBuilder** (系统提示组装) | `agent/prompt_builder.py` | `kestrel-agent/src/context.rs` (ContextBuilder) | **扩展** — 增强 ContextBuilder |
| **ContextCompressor** (上下文压缩) | `agent/context_compressor.py` | `kestrel-agent/src/compaction.rs` (简单截断) | **扩展** — 重写为 LLM 驱动的结构化压缩 |
| **BackgroundReview** (后台审查) | `run_agent.py:_spawn_background_review()` | 无对应 | **新建** → `kestrel-agent` 内新模块 |
| **SkillManager** (技能管理 CRUD) | `tools/skill_manager_tool.py` | 无对应（SkillsLoader 仅读取） | **新建** → `kestrel-agent/src/skills.rs` 扩展 |
| **SkillsList/SkillView** (技能查看) | `tools/skills_tool.py` | 无对应工具（SkillsLoader 仅内部使用） | **新建** → `kestrel-tools` 新工具 |
| **SkillCommands** (技能命令 /skill) | `agent/skill_commands.py` | 无对应 | **新建** → `kestrel-agent` 或 `kestrel-tools` |
| **SessionSearch** (会话搜索) | `tools/session_search_tool.py` | `kestrel-session/src/manager.rs` (基本 note 搜索) | **扩展** — 增加对话内容全文检索 |
| **CronScheduler** (定时调度) | `cron/scheduler.py` | `kestrel-cron/` | **扩展** — 增加技能绑定和审查调度 |
| **DelegateTask** (委托任务) | `tools/delegate_tool.py` | `kestrel-agent/src/subagent.rs` (SubAgentManager) | **适配** — 接入自演化上下文 |
| **MemoryTool** (记忆工具) | `tools/memory_tool.py` | 无对应 | **新建** → `kestrel-tools` 新工具 |
| **SkillManageTool** (技能管理工具) | `tools/skill_manager_tool.py` | 无对应 | **新建** → `kestrel-tools` 新工具 |
| **SOUL.md 加载** | `agent/prompt_builder.py:load_soul_md()` | 无对应 | **新建** → `kestrel-agent/src/context.rs` 扩展 |
| **上下文文件发现** | `agent/prompt_builder.py:build_context_files_prompt()` | 无对应 | **新建** → `kestrel-agent/src/context.rs` 扩展 |

### 3. 数据流适配方案

```
kestrel 现有消息流:

  InboundMessage ──→ Bus ──→ AgentLoop::handle_message()
                                │
                                ├── 1. SessionManager::get_or_create()
                                ├── 2. session.add_message(user_msg)
                                ├── 3. 简单截断检查 (prune_messages)
                                ├── 4. ContextBuilder::build_system_prompt()
                                │      ├─ identity (config.name)
                                │      ├─ runtime metadata
                                │      └─ tools list
                                ├── 5. AgentRunner::run()
                                │      ├─ LLM API call
                                │      ├─ tool calls → ToolRegistry::execute()
                                │      └─ loop until response
                                ├── 6. session.add_message(assistant_msg)
                                ├── 7. NotesManager::extract_notes()
                                └── 8. Bus::publish(OutboundMessage)


适配自演化后的消息流 (★ = 新增/变更):

  InboundMessage ──→ Bus ──→ AgentLoop::handle_message()
                                │
                                ├── 1. SessionManager::get_or_create()
                                ├── 2. session.add_message(user_msg)
                                │
                                ├── 3. ★ 上下文压缩检查 (LLM 结构化摘要)
                                │      └─ ContextCompressor::compress_if_needed()
                                │
                                ├── 4. ★ 增强系统提示构建
                                │      ├─ SOUL.md / DEFAULT_IDENTITY
                                │      ├─ ★ 冻结记忆快照 (MEMORY.md / USER.md)
                                │      ├─ ★ 外部记忆预取 (MemoryProvider::prefetch)
                                │      ├─ ★ 技能索引 (Tier 1: 名称+描述列表)
                                │      ├─ ★ 上下文文件 (AGENTS.md, .kestrel.md 等)
                                │      ├─ runtime metadata + timestamp
                                │      └─ platform hints
                                │
                                ├── 5. AgentRunner::run()
                                │      ├─ LLM API call
                                │      ├─ tool calls:
                                │      │   ├─ ★ memory → MemoryStore (磁盘持久化)
                                │      │   ├─ ★ skill_view → 加载 SKILL.md (Tier 2/3)
                                │      │   ├─ ★ skill_manage → 创建/更新/删除技能
                                │      │   ├─ ★ session_search → 跨会话检索
                                │      │   ├─ existing tools (shell, fs, web, ...)
                                │      │   └─ ...
                                │      └─ loop until response
                                │
                                ├── 6. session.add_message(assistant_msg)
                                ├── 7. NotesManager::extract_notes()
                                │
                                ├── 8. ★ 自演化后处理
                                │      ├─ 更新计数器 (turns_since_memory, iters_since_skill)
                                │      ├─ ★ 外部记忆同步 (MemoryManager::sync_all)
                                │      └─ ★ 触发条件检查 → 后台审查
                                │
                                ├── 9. Bus::publish(OutboundMessage)
                                │
                                └── 10. ★ 后台审查 (异步, 不阻塞响应)
                                        │
                                        ├─ 创建 ReviewAgent (轻量 AgentRunner)
                                        ├─ 注入审查 prompt (MEMORY_REVIEW / SKILL_REVIEW)
                                        ├─ 审查对话历史
                                        ├─ ★ 调用 memory 工具 → 更新 MEMORY.md / USER.md
                                        ├─ ★ 调用 skill_manage 工具 → 更新/创建 SKILL.md
                                        └─ 输出审查摘要 (可选通知)


自演化反馈环:

  用户交互 ─────→ 行为记录 (计数器)
      ▲                │
      │                ▼
      │          定期审查触发 (后台线程)
      │                │
      │                ▼
      │          ReviewAgent 执行
      │           ├─ 记忆审查 → MEMORY.md / USER.md 更新
      │           └─ 技能审查 → SKILL.md 创建/更新
      │                │
      │                ▼
      │          持久化存储 (磁盘)
      │                │
      └──────── 下次会话加载冻结快照到系统提示
```

### 4. Crate 边界设计

#### 4.1 新建 Crate

```
kestrel-memory/                      # 新 crate：持久化记忆系统
├── Cargo.toml
└── src/
    ├── lib.rs                       # 公共接口导出
    ├── store.rs                     # MemoryStore：MEMORY.md / USER.md 读写
    ├── manager.rs                   # MemoryManager：编排内置+外部提供者
    ├── provider.rs                  # MemoryProvider trait (抽象基类)
    ├── snapshot.rs                  # 冻结快照管理 (前缀缓存保护)
    ├── security.rs                  # 内容安全扫描 (注入检测、Unicode 检查)
    └── plugins/                     # 外部记忆提供者插件目录
        ├── mod.rs                   # 插件注册表
        ├── honcho.rs                # Honcho 提供者
        ├── mem0.rs                  # Mem0 提供者
        └── ...                      # 其他提供者
```

#### 4.2 Crate 职责划分

| Crate | 职责 | 关键新增内容 |
|-------|------|------------|
| **kestrel-memory** (新) | 持久化记忆存储、管理、插件 | MemoryStore、MemoryManager、MemoryProvider trait、冻结快照、安全扫描 |
| **kestrel-agent** | Agent 主循环、审查、上下文 | 扩展 ContextBuilder、新增 BackgroundReview 模块、扩展 SkillsLoader 为完整的 SkillManager |
| **kestrel-tools** | 工具定义和注册 | 新增 MemoryTool、SkillViewTool、SkillManageTool、SessionSearchTool |
| **kestrel-session** | 会话持久化 | 扩展搜索能力（对话内容全文检索） |
| **kestrel-core** | 核心类型 | 新增 ReviewTrigger、MemorySnapshot 等类型 |

#### 4.3 依赖关系图

```
                    ┌─────────────┐
                    │ kestrel-core│  ← 无内部依赖
                    └──────┬──────┘
                           │
              ┌────────────┼────────────────┐
              │            │                │
      ┌───────▼──────┐ ┌───▼────────┐ ┌────▼────────┐
      │kestrel-config│ │kestrel-bus │ │kestrel-memory│ ← 新 crate
      └───────┬──────┘ └─────┬──────┘ └────┬────────┘
              │              │              │        │
              │              │              │        │
              └──────┬───────┘              │        │
                     │                      │        │
           ┌─────────▼──────────┐           │        │
           │  kestrel-session   │◄──────────┤        │
           └─────────┬──────────┘           │        │
                     │                      │        │
           ┌─────────▼──────────┐           │        │
           │  kestrel-security  │           │        │
           └─────────┬──────────┘           │        │
                     │                      │        │
           ┌─────────▼──────────┐           │        │
           │ kestrel-providers  │           │        │
           └─────────┬──────────┘           │        │
                     │                      │        │
           ┌─────────▼──────────┐           │        │
           │  kestrel-tools     │◄──────────┤  (memory tool 使用 kestrel-memory)
           └─────────┬──────────┘           │        │
                     │                      │        │
           ┌─────────▼──────────┐           │        │
           │  kestrel-agent     │◄──────────┘  (审查使用 kestrel-memory)
           └─────────┬──────────┘
                     │
           ┌─────────▼──────────┐
           │  kestrel (binary)  │
           └────────────────────┘

循环依赖分析:
  kestrel-memory → kestrel-core       ✓ 无循环
  kestrel-tools  → kestrel-memory     ✓ 单向
  kestrel-agent  → kestrel-memory     ✓ 单向
  kestrel-agent  → kestrel-tools      ✓ 单向
  kestrel-memory 不依赖 kestrel-tools  ✓ 安全
  kestrel-memory 不依赖 kestrel-agent  ✓ 安全
```

**关键设计决策**：`kestrel-memory` 仅依赖 `kestrel-core`，不依赖 `kestrel-tools` 或 `kestrel-agent`。这确保了：
- MemoryStore 可以被 ReviewAgent 直接使用（无需完整工具系统）
- 工具层（MemoryTool）通过依赖 memory crate 来暴露功能
- 不会引入循环依赖

### 5. 集成点清单

| # | Hook 点 | 文件 | 函数/位置 | 侵入性 | 说明 |
|---|--------|------|----------|--------|------|
| H1 | **系统提示构建** | `kestrel-agent/src/context.rs` | `ContextBuilder::build_system_prompt()` | **中** | 需要大幅扩展：增加 SOUL.md 加载、记忆注入、技能索引注入、上下文文件发现。现有逻辑保留，新增多个构建步骤 |
| H2 | **消息处理主循环** | `kestrel-agent/src/loop_mod.rs` | `AgentLoop::handle_message()` | **中** | 在 LLM 调用前插入：记忆预取、增强压缩检查；在 LLM 调用后插入：计数器更新、审查触发 |
| H3 | **工具注册** | `kestrel-tools/src/builtins/mod.rs` | `register_all()` | **低** | 新增 memory、skill_view、skill_manage、session_search 工具注册 |
| H4 | **上下文压缩** | `kestrel-agent/src/compaction.rs` | `compact_session()` / `prune_messages()` | **高** | 需要重写为 LLM 驱动的结构化摘要。现有截断逻辑可作为 fallback 保留 |
| H5 | **后台审查触发** | `kestrel-agent/src/loop_mod.rs` | 响应处理之后（新增代码段） | **中** | 在 `handle_message()` 末尾新增审查触发逻辑。需要新增计数器字段到 AgentLoop |
| H6 | **审查执行** | `kestrel-agent/src/` (新文件) | `background_review.rs` (新模块) | **低** | 独立新模块，创建轻量 ReviewAgent，不侵入现有代码 |
| H7 | **会话管理** | `kestrel-session/src/manager.rs` | `SessionManager` | **低** | 扩展搜索方法（search_conversations），不影响现有接口 |
| H8 | **Config Schema** | `kestrel-config/src/schema.rs` | `Config` struct | **低** | 新增 `memory`、`skills`、`review` 配置段落，均为可选（有默认值） |
| H9 | **配置验证** | `kestrel-config/src/validate.rs` | `validate()` | **低** | 新增 memory/skills/review 配置验证规则 |
| H10 | **Skills 加载** | `kestrel-agent/src/skills.rs` | `SkillsLoader` | **中** | 扩展为完整的 SkillManager：增加 create/patch/delete 能力，增加渐进式披露支持 |
| H11 | **核心类型** | `kestrel-core/src/types.rs` | 类型定义 | **低** | 新增 ReviewTrigger、MemorySnapshot、SkillIndex 等辅助类型 |
| H12 | **核心常量** | `kestrel-core/src/constants.rs` | 常量定义 | **低** | 新增审查间隔默认值、记忆容量限制等常量 |

**侵入性评级说明**：
- **低**：纯新增代码或仅添加字段/函数，不影响现有行为
- **中**：修改现有函数的流程，但保持接口兼容
- **高**：需要重写核心逻辑，可能影响现有行为

### 6. 迁移架构路线图

#### Phase 1：基础设施 (预计 15-20 人天)

**目标**：建立自演化的数据基础 —— 持久化记忆和增强系统提示

```
Phase 1 架构:

  ┌─────────────────────────────────────────────────────────┐
  │                    AgentLoop                             │
  │                                                         │
  │  ContextBuilder (增强)       MemoryStore (新)            │
  │  ┌──────────────────┐       ┌──────────────────┐        │
  │  │ 1. SOUL.md       │  ───→ │ MEMORY.md        │        │
  │  │ 2. Memory 快照   │       │ USER.md          │        │
  │  │ 3. 技能索引(T1)  │       │ 冻结快照          │        │
  │  │ 4. 上下文文件    │       │ 安全扫描          │        │
  │  │ 5. runtime meta  │       └──────────────────┘        │
  │  └──────────────────┘                                    │
  │                                                         │
  │  新增工具: MemoryTool, SkillViewTool                     │
  └─────────────────────────────────────────────────────────┘

  交付物:
  ├─ kestrel-memory crate (store + security)
  ├─ kestrel-tools 新增 MemoryTool、SkillViewTool
  ├─ ContextBuilder 增强 (记忆注入 + SOUL.md + 上下文文件)
  ├─ Config 扩展 (memory 段落)
  └─ 测试: 记忆读写、快照冻结、安全扫描
```

**具体任务**：
1. 创建 `kestrel-memory` crate，实现 `MemoryStore`（MEMORY.md/USER.md 读写、冻结快照、安全扫描）
2. 在 `kestrel-tools` 新增 `MemoryTool`（memory 工具：add/replace/remove）
3. 在 `kestrel-tools` 新增 `SkillViewTool`（skill_view 工具：渐进式披露）
4. 扩展 `ContextBuilder`：
   - 加载 SOUL.md / DEFAULT_IDENTITY
   - 注入冻结记忆快照
   - 注入技能索引（Tier 1 列表）
   - 发现并注入上下文文件（AGENTS.md 等）
5. 在 `kestrel-config` 新增 `memory` 配置段落
6. 集成测试

#### Phase 2：自演化核心 (预计 18-28 人天)

**目标**：实现自演化的核心闭环 —— 后台审查和上下文压缩

```
Phase 2 架构:

  ┌──────────────────────────────────────────────────────────────┐
  │                       AgentLoop                               │
  │                                                              │
  │  ┌──────────────┐   ┌──────────────┐   ┌─────────────────┐  │
  │  │ ContextBuilder│   │ ContextCompr. │   │ BackgroundReview│  │
  │  │  (Phase 1)   │   │  (新: LLM摘要)│   │    (新模块)      │  │
  │  └──────────────┘   └──────────────┘   └────────┬────────┘  │
  │                                                   │          │
  │  计数器: turns_since_memory / iters_since_skill    │          │
  │          │                                        │          │
  │          └── 触发条件满足 ──────────────────────────┘          │
  │                                                   │          │
  │                     ReviewAgent                    │          │
  │                     ┌──────────┐                  │          │
  │                     │ 审查对话  │                  │          │
  │                     │ → memory │ → 更新 MEMORY.md  │          │
  │                     │ → skill  │ → 更新 SKILL.md   │          │
  │                     └──────────┘                  │          │
  │                                                              │
  │  新增工具: SkillManageTool, SessionSearchTool                │
  └──────────────────────────────────────────────────────────────┘

  交付物:
  ├─ BackgroundReview 模块 (审查触发 + ReviewAgent)
  ├─ ContextCompressor 重写 (LLM 结构化摘要)
  ├─ kestrel-tools 新增 SkillManageTool、SessionSearchTool
  ├─ SkillsLoader → SkillManager 升级 (create/patch/delete)
  ├─ kestrel-session 搜索增强
  └─ 测试: 审查触发、压缩质量、技能 CRUD
```

**具体任务**：
1. 在 `kestrel-agent` 新增 `background_review.rs` 模块：
   - `ReviewTrigger` 逻辑（计数器 + 阈值判断）
   - `ReviewAgent`（轻量 AgentRunner + 审查 prompt）
   - 后台 tokio task 执行（不阻塞主循环）
2. 重写 `compaction.rs`：
   - LLM 驱动的结构化摘要（Goal/Progress/Decisions/...）
   - 迭代式摘要更新
   - 工具调用/结果配对保护
   - 失败时回退到简单截断
3. 在 `kestrel-tools` 新增 `SkillManageTool`（skill_manage: create/patch/edit/delete）
4. 扩展 `SkillsLoader` 为完整 `SkillManager`
5. 在 `kestrel-tools` 新增 `SessionSearchTool`
6. 扩展 `kestrel-session` 的搜索能力
7. 在 `kestrel-config` 新增 `skills`、`review` 配置段落
8. 集成测试

#### Phase 3：生态增强 (预计 11-20 人天)

**目标**：完善插件体系和高级特性

```
Phase 3 架构:

  ┌───────────────────────────────────────────────────────────────────┐
  │                          AgentLoop                                │
  │                                                                   │
  │  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐              │
  │  │ ContextBuilder│ │ContextCompr. │ │BackgroundRev.│              │
  │  │  (完整)       │ │  (完整)      │ │   (完整)     │              │
  │  └──────────────┘ └──────────────┘ └──────────────┘              │
  │                                                                   │
  │  ┌──────────────────────────────────────────────────┐             │
  │  │             MemoryManager (新)                     │             │
  │  │  ┌──────────────┐  ┌──────────────────┐          │             │
  │  │  │ BuiltinProv.  │  │ ExternalProvider  │          │             │
  │  │  │ (Phase 1)    │  │ ┌──────────────┐ │          │             │
  │  │  │              │  │ │ Honcho/Mem0  │ │          │             │
  │  │  │              │  │ │ Hindsight/...│ │          │             │
  │  │  │              │  │ └──────────────┘ │          │             │
  │  │  └──────────────┘  └──────────────────┘          │             │
  │  └──────────────────────────────────────────────────┘             │
  │                                                                   │
  │  Cron 增强: 审查调度 + 技能绑定                                    │
  │  Sub-agent 适配: 继承 MemoryStore 上下文                          │
  └───────────────────────────────────────────────────────────────────┘

  交付物:
  ├─ MemoryProvider trait + 插件加载机制
  ├─ 外部记忆提供者 (Honcho / Mem0 / ...)
  ├─ MemoryManager 编排层
  ├─ Cron 审查调度 (定时审查，非基于计数器)
  ├─ Sub-agent 自演化上下文继承
  ├─ /skill 命令系统
  └─ 端到端测试 + 性能基准
```

**具体任务**：
1. 在 `kestrel-memory` 新增 `provider.rs`（MemoryProvider trait）和 `manager.rs`（MemoryManager 编排层）
2. 实现至少 2 个外部记忆提供者插件（如 Honcho、Mem0）
3. 扩展 `kestrel-cron`：支持审查调度（定期触发审查，独立于对话）
4. 扩展 `SubAgentManager`：继承 MemoryStore 上下文
5. 在 `kestrel-agent` 或 `kestrel-tools` 新增 `/skill` 命令处理
6. 端到端集成测试
7. 性能基准测试（审查延迟、压缩质量、记忆命中率）

#### 路线图总览

```
Phase 1 (基础设施)          Phase 2 (自演化核心)         Phase 3 (生态增强)
─────────────────          ──────────────────          ──────────────────
│ 记忆存储     │ ────────→ │ 后台审查引擎   │ ────────→ │ 外部记忆插件   │
│ 系统提示增强  │           │ 上下文压缩重写 │           │ Cron 审查调度  │
│ 记忆/查看工具 │           │ 技能管理 CRUD  │           │ Sub-agent 适配 │
│ SOUL.md 加载  │           │ 会话搜索增强   │           │ 技能命令系统   │
│ 上下文文件    │           │               │           │ 端到端测试     │
─────────────────          ──────────────────          ──────────────────
 15-20 人天                  18-28 人天                  11-20 人天
                            │                            │
                            └──────── 总计: 44-68 人天 ──┘
```
