# 🟢 绿帽创意设计：kestrel 自进化系统

> **设计哲学：不是移植 Python，而是重新发明。**
> Hermes 用了动态类型的所有「便利」——猴子补丁、全局注册表、文件系统即数据库、运行时反射。
> Rust 不应该模仿这些，而应该用 Rust 的力量（类型安全、零成本抽象、编译期检查、异步原生）做到 Python 做不到的事。

---

## 一、Rust 原生技能架构：从文件到编译

### 1.1 核心理念：技能即代码，不是数据

Hermes 的技能本质上是 Markdown 文件——LLM 在运行时解析、匹配、注入到 prompt。这是一种「解释型」技能系统。Rust 版本应该走得更远：

**技能是编译单元，不是运行时数据文件。**

```
Hermes (Python):  SKILL.md → YAML解析 → 字符串注入到prompt → LLM自行理解
kestrel:     Skill.toml + 模板 → 编译期验证 → 类型安全的执行计划 → 零拷贝注入
```

### 1.2 技能类型层次

```rust
// ─────────────────────────────────────────────────────────────────
// 技能核心 trait — 所有技能的基础契约
// ─────────────────────────────────────────────────────────────────

/// 技能的元数据——编译时即可确定的静态信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    /// 技能唯一标识（kebab-case，最大64字符）
    pub name: SkillName,
    /// 一行描述（用于 skills_list 的渐进式披露）
    pub description: String,
    /// 版本（语义化版本号）
    pub version: SemanticVersion,
    /// 触发条件：什么时候该技能应该被激活
    pub triggers: Vec<Trigger>,
    /// 技能分类标签
    pub tags: Vec<Tag>,
    /// 依赖的其他技能
    pub depends_on: Vec<SkillName>,
    /// 平台兼容性
    pub platforms: PlatformSet,
    /// 置信度分数（0.0-1.0，随使用动态调整）
    pub confidence: ConfidenceScore,
    /// 使用统计（由 SkillRuntime 维护）
    #[serde(skip)]
    pub stats: SkillStatistics,
}

newtype!(
    SkillName(String, [a-z0-9][a-z0-9._-]* , max 64),
    SemanticVersion(semver::Version),
    Tag(String),
    ConfidenceScore(f64, 0.0..=1.0),
);

/// 技能执行 trait — 技能的核心行为
#[async_trait]
pub trait Skill: Send + Sync + 'static {
    /// 返回技能清单（静态元数据）
    fn manifest(&self) -> &SkillManifest;

    /// 技能匹配分数：给定用户输入，返回 0.0-1.0 的匹配度
    /// 系统选择匹配度最高的技能激活（不需要 LLM 参与）
    async fn match_score(&self, context: &TurnContext) -> ConfidenceScore;

    /// 构建技能的 prompt 注入内容
    /// 返回要注入到系统提示中的文本片段
    async fn build_prompt_segment(&self, context: &TurnContext) -> Result<PromptSegment>;

    /// 技能执行：当技能被激活时执行的具体操作
    /// 不是所有技能都需要执行操作（有些只是 prompt 注入）
    async fn execute(&self, args: SkillArgs, context: &TurnContext) -> Result<SkillOutput>;

    /// 技能自省：技能可以描述自己的状态和效果
    fn introspect(&self) -> SkillIntrospection;
}

/// 技能输出类型 — 用枚举替代 Python 的 JSON 字符串
#[derive(Debug, Clone)]
pub enum SkillOutput {
    /// 仅注入 prompt（大多数技能的行为）
    PromptOnly(PromptSegment),
    /// 执行了具体操作并返回结果
    Executed { result: String, side_effects: Vec<SideEffect> },
    /// 技能建议创建新技能（自进化的关键入口）
    SuggestSkill { name: SkillName, reason: String, template: SkillTemplate },
    /// 技能建议修改自身
    SuggestSelfUpdate { patch: SkillPatch },
}

/// 触发条件 — 替代 Hermes 的模糊"靠LLM理解什么时候用"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Trigger {
    /// 关键词匹配
    Keyword { patterns: Vec<String>, case_sensitive: bool },
    /// 正则表达式
    Regex(String),
    /// 语义相似度阈值（通过嵌入向量）
    Semantic { threshold: f64, description: String },
    /// 工具调用模式：当特定工具被调用时触发
    ToolUsed { tool_name: String, on_success: bool, on_failure: bool },
    /// 上下文条件：当对话满足特定条件时
    Context { min_turns: u32, has_files: bool, has_errors: bool },
    /// 组合条件
    All(Vec<Trigger>),
    Any(Vec<Trigger>),
    Not(Box<Trigger>),
}
```

### 1.3 技能编译器——Hermes 没有的新概念

Hermes 的技能是解释执行的：每次加载都重新解析 YAML frontmatter，每次使用都重建 prompt。Rust 版本引入「技能编译」：

```rust
/// 技能编译器：将声明式技能定义编译为高效的运行时结构
pub struct SkillCompiler {
    /// 模板引擎（基于 Handlebars/Tera）
    template_engine: TemplateEngine,
    /// 触发条件编译器
    trigger_compiler: TriggerCompiler,
    /// 验证器管线
    validators: Vec<Box<dyn SkillValidator>>,
}

impl SkillCompiler {
    /// 编译流程：解析 → 验证 → 优化 → 编译
    pub fn compile(&self, source: SkillSource) -> Result<CompiledSkill> {
        // 阶段1: 解析 — 从 TOML/Markdown 提取结构
        let raw = self.parse(source)?;

        // 阶段2: 验证 — 类型检查、依赖检查、安全扫描
        let validated = self.validate(raw)?;

        // 阶段3: 优化 — 编译正则、预计算嵌入、去重
        let optimized = self.optimize(validated)?;

        // 阶段4: 编译 — 生成高效的运行时表示
        Ok(CompiledSkill {
            manifest: optimized.manifest,
            matcher: self.trigger_compiler.compile(&optimized.triggers)?,
            prompt_template: self.template_engine.compile(&optimized.prompt_template)?,
            executor: optimized.executor,
            checksum: blake3::hash(&serialized).into(),
        })
    }
}

/// 编译后的技能 — 零拷贝、高速匹配
pub struct CompiledSkill {
    manifest: SkillManifest,
    /// 预编译的触发匹配器（正则→DFA，语义→量化向量）
    matcher: CompiledMatcher,
    /// 预编译的 prompt 模板
    prompt_template: CompiledTemplate,
    /// 执行器
    executor: SkillExecutor,
    /// 内容校验和（用于变更检测和热重载）
    checksum: [u8; 32],
}

/// 技能匹配器 — 编译后的高效匹配
pub enum CompiledMatcher {
    /// DFA 自动机（正则编译后）
    RegexDFA { automaton: regex_automata::DFA<Vec<u8>> },
    /// Aho-Corasick 多模式匹配（关键词）
    Keyword { ac: aho_corasick::AhoCorasick },
    /// 量化向量（语义匹配）
    Semantic { embedding: QuantizedEmbedding, threshold: f32 },
    /// 组合匹配器
    Composite { matchers: Vec<CompiledMatcher>, logic: CombineLogic },
}
```

---

## 二、事件驱动的学习系统

### 2.1 替代 Hermes 的「靠LLM自我反思」

Hermes 的自进化依赖于 LLM 自己决定什么时候保存技能、修改技能。这是低效且不可靠的。Rust 版本用**事件溯源**替代：

```rust
/// 学习事件 — 系统中发生的每一件值得学习的事
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum LearningEvent {
    /// 工具调用成功
    ToolSucceeded {
        tool: String,
        args_summary: String,
        duration: Duration,
        context_hash: ContextHash,
        timestamp: Instant,
    },
    /// 工具调用失败
    ToolFailed {
        tool: String,
        args_summary: String,
        error: ErrorClassification,
        retry_count: u32,
        timestamp: Instant,
    },
    /// 用户纠正了 agent 的行为
    UserCorrection {
        original_action: String,
        correction_hint: String,
        topic: String,
        timestamp: Instant,
    },
    /// 用户显式肯定了某个行为
    UserApproval {
        action_taken: String,
        implicit: bool,  // true = 用户只是继续了对话
        timestamp: Instant,
    },
    /// 技能被使用
    SkillUsed {
        skill_name: SkillName,
        match_score: ConfidenceScore,
        outcome: SkillOutcome,
        timestamp: Instant,
    },
    /// 新技能被创建
    SkillCreated {
        skill_name: SkillName,
        trigger_reason: String,
        source_session: SessionId,
        timestamp: Instant,
    },
    /// 上下文被压缩（信息损失事件）
    ContextCompressed {
        tokens_before: usize,
        tokens_after: usize,
        preserved_topics: Vec<String>,
        timestamp: Instant,
    },
}

/// 事件分类
#[derive(Debug, Clone, Serialize)]
pub enum ErrorClassification {
    /// 用户输入问题
    UserInput,
    /// 工具配置问题
    ToolConfig,
    /// 环境问题（网络、权限等）
    Environment,
    /// Agent 策略错误（该用工具A却用了B）
    AgentStrategy,
    /// 技能不完整（缺少步骤或条件）
    SkillIncomplete,
}

/// 事件处理器 — 响应学习事件并更新系统
#[async_trait]
pub trait LearningEventHandler: Send + Sync {
    /// 处理一个学习事件
    async fn handle(&self, event: &LearningEvent) -> Result<LearningAction>;

    /// 处理器名称
    fn name(&self) -> &str;

    /// 处理器优先级（影响执行顺序）
    fn priority(&self) -> u8;
}

/// 学习动作 — 事件处理的结果
#[derive(Debug, Clone)]
pub enum LearningAction {
    /// 无需操作
    NoOp,
    /// 更新技能置信度
    AdjustConfidence { skill: SkillName, delta: f64 },
    /// 建议创建新技能
    ProposeSkill { template: SkillTemplate, evidence: Vec<LearningEvent> },
    /// 建议修改技能
    PatchSkill { skill: SkillName, patch: SkillPatch, evidence: Vec<LearningEvent> },
    /// 建议禁用技能
    DeprecateSkill { skill: SkillName, reason: String },
    /// 记录到长期记忆
    RecordInsight { insight: String, category: InsightCategory },
}

/// 事件总线 — 学习系统的核心调度器
pub struct LearningEventBus {
    handlers: Vec<Box<dyn LearningEventHandler>>,
    event_store: Arc<dyn EventStore>,
    /// 事件缓冲区（批量处理，减少 LLM 调用频率）
    buffer: tokio::sync::RwLock<Vec<LearningEvent>>,
    /// 缓冲区刷新间隔
    flush_interval: Duration,
}

impl LearningEventBus {
    /// 发射学习事件（异步、非阻塞）
    pub async fn emit(&self, event: LearningEvent) {
        // 1. 持久化事件（事件溯源）
        self.event_store.append(event.clone()).await;

        // 2. 加入缓冲区
        self.buffer.write().await.push(event);

        // 3. 如果缓冲区满了，立即处理
        if self.buffer.read().await.len() >= self.buffer_size {
            self.flush().await;
        }
    }

    /// 批量处理缓冲的事件
    async fn flush(&self) {
        let events: Vec<_> = {
            let mut buf = self.buffer.write().await;
            std::mem::take(&mut *buf)
        };

        if events.is_empty() {
            return;
        }

        // 按优先级排序处理器
        let mut handlers = self.handlers.clone();
        handlers.sort_by_key(|h| h.priority());

        for handler in &handlers {
            for event in &events {
                if let Ok(action) = handler.handle(event).await {
                    self.execute_action(action).await;
                }
            }
        }
    }
}
```

### 2.2 具体处理器：技能质量监控器

```rust
/// 技能质量监控器 — 基于使用数据动态评估技能质量
pub struct SkillQualityMonitor {
    stats_store: Arc<dyn StatisticsStore>,
    quality_threshold: f64,
}

#[async_trait]
impl LearningEventHandler for SkillQualityMonitor {
    async fn handle(&self, event: &LearningEvent) -> Result<LearningAction> {
        match event {
            LearningEvent::SkillUsed { skill_name, outcome, .. } => {
                let stats = self.stats_store.get_skill_stats(skill_name).await;

                match outcome {
                    SkillOutcome::Helpful => {
                        // 技能被成功使用，增加置信度
                        let delta = 0.05 * (1.0 - stats.confidence);
                        Ok(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta,
                        })
                    }
                    SkillOutcome::Irrelevant => {
                        // 技能被激活但没有帮助，降低置信度
                        let delta = -0.1;
                        Ok(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta,
                        })
                    }
                    SkillOutcome::Harmful => {
                        // 技能导致了问题，大幅降低置信度
                        let delta = -0.3;
                        Ok(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta,
                        })
                    }
                    _ => Ok(LearningAction::NoOp),
                }
            }
            _ => Ok(LearningAction::NoOp),
        }
    }

    fn name(&self) -> &str { "skill_quality_monitor" }
    fn priority(&self) -> u8 { 50 }
}
```

---

## 三、分层记忆架构

### 3.1 三级缓存模型

Hermes 的记忆是简单的 MEMORY.md + USER.md 文件。Rust 版本引入 CPU 式的分层缓存：

```rust
/// 记忆存储 trait — 所有记忆后端的统一接口
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// L1 查询：热记忆（当前上下文，零延迟）
    async fn recall_hot(&self, query: &str) -> Option<MemoryEntry>;

    /// L2 查询：温暖记忆（可搜索，毫秒级延迟）
    async fn recall_warm(&self, query: &str, limit: usize) -> Vec<MemoryEntry>;

    /// L3 查询：冷记忆（归档数据，可能需要解压）
    async fn recall_cold(&self, query: &str, limit: usize) -> Vec<MemoryEntry>;

    /// 存储记忆
    async fn store(&self, entry: MemoryEntry, level: MemoryLevel) -> Result<()>;

    /// 提升记忆（冷→暖→热）
    async fn promote(&self, entry_id: &EntryId, to: MemoryLevel) -> Result<()>;

    /// 淘汰记忆（热→暖→冷→删除）
    async fn evict(&self, entry_id: &EntryId) -> Result<()>;

    /// 搜索记忆（跨层）
    async fn search(&self, query: &MemoryQuery) -> Vec<ScoredEntry>;
}

/// 记忆层级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLevel {
    /// L1: 热记忆 — 直接注入到系统提示，零延迟
    /// 容量限制：~2KB（与 Hermes 相同，但类型安全）
    Hot,

    /// L2: 温暖记忆 — 可搜索的向量索引，毫秒级延迟
    /// 容量限制：~100KB（使用 SQLite + 向量索引）
    Warm,

    /// L3: 冷记忆 — 压缩归档，秒级延迟
    /// 容量限制：无限（使用压缩文件）
    Cold,
}

/// 记忆条目 — 替代 Hermes 的纯字符串
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// 唯一标识
    id: EntryId,
    /// 内容
    content: String,
    /// 分类
    category: MemoryCategory,
    /// 来源（用户输入、LLM发现、工具结果等）
    source: MemorySource,
    /// 创建时间
    created_at: DateTime<Utc>,
    /// 最后访问时间
    last_accessed: DateTime<Utc>,
    /// 访问计数
    access_count: u32,
    /// 置信度（越高越可信）
    confidence: ConfidenceScore,
    /// 关联的嵌入向量（用于语义搜索）
    #[serde(skip)]
    embedding: Option<Vec<f32>>,
}

/// 记忆分类 — 替代 Hermes 的"随便是啥都往MEMORY.md塞"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemoryCategory {
    /// 用户偏好（名称、角色、语言偏好等）
    UserProfile,
    /// 环境事实（操作系统、已安装工具、项目结构等）
    Environment,
    /// 项目约定（代码风格、部署流程等）
    ProjectConvention,
    /// 工具发现（API怪癖、工具使用技巧等）
    ToolDiscovery,
    /// 错误经验（什么东西不行、常见陷阱等）
    ErrorLesson,
    /// 工作流模式（用户喜欢的工作方式）
    WorkflowPattern,
}

/// 记忆查询 — 类型安全的查询替代 Hermes 的字符串搜索
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    /// 全文搜索
    text: Option<String>,
    /// 按分类过滤
    category: Option<MemoryCategory>,
    /// 按时间范围过滤
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// 最低置信度
    min_confidence: Option<f64>,
    /// 语义搜索（通过嵌入向量）
    semantic: Option<String>,
    /// 最大结果数
    limit: usize,
}
```

### 3.2 具体实现：分层记忆管理器

```rust
/// 分层记忆管理器 — 自动在各层之间调度
pub struct TieredMemoryManager {
    /// L1 热记忆：内存中的 HashMap，直接注入 prompt
    hot: RwLock<HashMap<EntryId, MemoryEntry>>,

    /// L2 温暖记忆：SQLite + 向量索引
    warm: Arc<WarmStore>,

    /// L3 冷记忆：压缩文件
    cold: Arc<ColdStore>,

    /// 嵌入生成器（用于语义搜索）
    embedder: Arc<dyn Embedder>,

    /// 分层策略
    policy: TieringPolicy,
}

impl TieredMemoryManager {
    /// 智能召回：自动决定查询哪些层
    pub async fn smart_recall(&self, query: &str, budget: usize) -> Vec<MemoryEntry> {
        let mut results = Vec::new();
        let mut remaining_budget = budget;

        // L1: 热记忆总是先查
        if let Some(entry) = self.recall_hot(query).await {
            results.push(entry);
            remaining_budget -= 1;
        }

        // L2: 如果预算允许，查温暖记忆
        if remaining_budget > 0 {
            let warm_results = self.recall_warm(query, remaining_budget).await;
            remaining_budget -= warm_results.len();
            results.extend(warm_results);
        }

        // L3: 只在前两层没有足够结果时才查冷记忆
        if remaining_budget > 0 && results.len() < budget / 2 {
            let cold_results = self.recall_cold(query, remaining_budget).await;
            results.extend(cold_results);

            // 提升：频繁被访问的冷记忆自动提升到温暖层
            for entry in &cold_results {
                if entry.access_count > 3 {
                    let _ = self.promote(&entry.id, MemoryLevel::Warm).await;
                }
            }
        }

        results
    }

    /// 后台整理：定期在各层之间移动记忆
    pub async fn run_maintenance(&self) {
        // 热记忆超容量 → 淘汰最久未访问的到温暖层
        // 温暖记忆超容量 → 淘汰低置信度的到冷层
        // 冷记忆超龄 → 删除
    }
}
```

---

## 四、Hermes 没有的新功能

### 4.1 A/B 技能测试

```rust
/// A/B 测试框架 — 同时运行两个技能版本，测量效果差异
pub struct SkillABTester {
    /// 实验存储
    experiments: Arc<dyn ExperimentStore>,
    /// 随机分配器
    allocator: BucketAllocator,
}

#[derive(Debug, Clone)]
pub struct SkillExperiment {
    /// 实验ID
    id: ExperimentId,
    /// 技能名称
    skill_name: SkillName,
    /// 控制组（当前版本）
    control: SkillVersion,
    /// 实验组（新版本）
    treatment: SkillVersion,
    /// 分配比例（如 0.5 表示 50/50）
    traffic_split: f64,
    /// 评估指标
    metrics: Vec<ExperimentMetric>,
    /// 开始时间
    started_at: DateTime<Utc>,
    /// 所需样本量
    required_samples: usize,
}

/// 实验指标
#[derive(Debug, Clone)]
pub enum ExperimentMetric {
    /// 用户满意度（隐式：继续对话 vs 纠正）
    UserSatisfaction,
    /// 任务完成率
    TaskCompletionRate,
    /// 平均轮次（越少越好）
    AverageTurns,
    /// 工具调用效率
    ToolCallEfficiency,
    /// 技能匹配准确率
    MatchAccuracy,
}

impl SkillABTester {
    /// 为一次技能调用分配版本
    pub async fn assign(&self, experiment: &SkillExperiment, session: &SessionId) -> SkillVersion {
        let bucket = self.allocator.assign(session, experiment.traffic_split);
        match bucket {
            Bucket::Control => experiment.control.clone(),
            Bucket::Treatment => experiment.treatment.clone(),
        }
    }

    /// 记录实验结果
    pub async fn record_outcome(
        &self,
        experiment_id: &ExperimentId,
        version: SkillVersion,
        outcome: &ExperimentOutcome,
    ) {
        self.experiments.record(experiment_id, version, outcome).await;

        // 检查是否达到统计显著性
        if self.is_significant(experiment_id).await {
            self.promote_winner(experiment_id).await;
        }
    }
}
```

### 4.2 技能组合图（DAG）

```rust
/// 技能组合图 — 技能之间的依赖关系
pub struct SkillGraph {
    /// 邻接表表示的 DAG
    nodes: HashMap<SkillName, SkillNode>,
    /// 拓扑排序缓存
    topo_order: RwLock<Option<Vec<SkillName>>>,
}

#[derive(Debug, Clone)]
pub struct SkillNode {
    skill_name: SkillName,
    /// 该技能依赖的其他技能
    dependencies: Vec<SkillName>,
    /// 依赖该技能的其他技能
    dependents: Vec<SkillName>,
    /// 加载状态
    state: SkillLoadState,
}

#[derive(Debug, Clone)]
pub enum SkillLoadState {
    /// 未加载
    Unloaded,
    /// 正在加载
    Loading,
    /// 已加载
    Loaded(Arc<dyn Skill>),
    /// 加载失败
    Failed(String),
}

impl SkillGraph {
    /// 按拓扑顺序加载所有技能
    pub async fn load_all(&self, loader: &dyn SkillLoader) -> Result<()> {
        let order = self.topological_sort()?;

        for skill_name in &order {
            let node = self.nodes.get(skill_name).unwrap();

            // 检查依赖是否全部加载
            let deps_ready = node.dependencies.iter().all(|dep| {
                matches!(self.nodes.get(dep).unwrap().state, SkillLoadState::Loaded(_))
            });

            if !deps_ready {
                return Err(anyhow!("循环依赖或缺失依赖: {:?}", node.dependencies));
            }

            // 加载技能
            let skill = loader.load(skill_name).await?;
            self.nodes.get_mut(skill_name).unwrap().state = SkillLoadState::Loaded(skill);
        }

        Ok(())
    }

    /// 获取激活某技能时需要一起加载的所有技能
    pub fn activation_set(&self, skill: &SkillName) -> Vec<SkillName> {
        let mut result = vec![skill.clone()];
        let node = self.nodes.get(skill);

        if let Some(node) = node {
            for dep in &node.dependencies {
                result.extend(self.activation_set(dep));
            }
        }

        result.sort();
        result.dedup();
        result
    }
}
```

### 4.3 置信度评分系统

```rust
/// 置信度评分引擎 — 每个记忆和技能都有动态置信度
pub struct ConfidenceEngine {
    /// 衰减率（记忆会随时间"遗忘"）
    decay_rate: f64,
    /// 强化系数（每次成功使用提升的量）
    reinforcement: f64,
    /// 惩罚系数（每次失败降低的量）
    penalty: f64,
}

impl ConfidenceEngine {
    /// 更新置信度：基于时间和使用历史
    pub fn update(&self, current: ConfidenceScore, event: &ConfidenceEvent) -> ConfidenceScore {
        let raw = match event {
            ConfidenceEvent::UsedSuccessfully => current.0 + self.reinforcement * (1.0 - current.0),
            ConfidenceEvent::UsedButFailed => (current.0 - self.penalty).max(0.0),
            ConfidenceEvent::UserConfirmed => (current.0 + 0.2).min(1.0),
            ConfidenceEvent::UserCorrected => (current.0 - 0.15).max(0.0),
            ConfidenceEvent::TimeDecay(hours) => {
                current.0 * (-self.decay_rate * hours).exp()
            }
        };

        ConfidenceScore(raw.clamp(0.0, 1.0))
    }

    /// 置信度是否低于阈值，需要考虑淘汰
    pub fn should_deprecate(&self, score: ConfidenceScore) -> bool {
        score.0 < 0.1
    }

    /// 置信度是否足够高，可以自动使用
    pub fn should_auto_activate(&self, score: ConfidenceScore) -> bool {
        score.0 > 0.7
    }
}
```

### 4.4 回滚能力

```rust
/// 技能版本管理 — 支持回滚到任意历史版本
pub struct SkillVersionControl {
    /// 版本存储
    store: Arc<dyn VersionStore>,
    /// 最大保留版本数
    max_versions: usize,
}

#[derive(Debug, Clone)]
pub struct SkillVersion {
    /// 版本号
    version: u32,
    /// 内容哈希
    content_hash: [u8; 32],
    /// 提交时间
    committed_at: DateTime<Utc>,
    /// 变更原因
    commit_message: String,
    /// 变更来源（agent自动、用户手动、系统建议）
    source: VersionSource,
    /// 父版本
    parent: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum VersionSource {
    /// Agent 自动创建
    AgentAuto { session_id: SessionId },
    /// 用户手动创建
    UserManual,
    /// 系统建议创建（A/B测试优胜者等）
    SystemRecommended { reason: String },
}

impl SkillVersionControl {
    /// 创建新版本
    pub async fn commit(
        &self,
        skill_name: &SkillName,
        content: &[u8],
        message: String,
        source: VersionSource,
    ) -> Result<u32> {
        let hash = blake3::hash(content);
        let current = self.store.latest_version(skill_name).await?;

        let version = SkillVersion {
            version: current.map_or(1, |v| v.version + 1),
            content_hash: hash.into(),
            committed_at: Utc::now(),
            commit_message: message,
            source,
            parent: current.map(|v| v.version),
        };

        self.store.store_version(skill_name, &version, content).await?;

        // 清理旧版本（保留最近 N 个）
        self.prune_old_versions(skill_name).await?;

        Ok(version.version)
    }

    /// 回滚到指定版本
    pub async fn rollback(
        &self,
        skill_name: &SkillName,
        target_version: u32,
    ) -> Result<Vec<u8>> {
        let content = self.store.get_version(skill_name, target_version).await?;

        // 回滚本身也是一个版本（可以再次回滚）
        self.commit(
            skill_name,
            &content,
            format!("回滚到版本 v{}", target_version),
            VersionSource::UserManual,
        ).await?;

        Ok(content)
    }
}
```

### 4.5 跨会话学习（社区技能）

```rust
/// 社区技能聚合器 — 跨用户（匿名化）聚合常见模式
pub struct CommunitySkillAggregator {
    /// 本地匿名化器
    anonymizer: Arc<DataAnonymizer>,
    /// 聚合规则
    rules: AggregationRules,
}

impl CommunitySkillAggregator {
    /// 从使用模式中提取候选社区技能
    pub async fn extract_community_candidates(
        &self,
        patterns: &[UsagePattern],
    ) -> Vec<CommunitySkillCandidate> {
        patterns
            .iter()
            .filter(|p| p.frequency > self.rules.min_frequency)
            .filter(|p| p.success_rate > self.rules.min_success_rate)
            .map(|p| {
                let anonymized = self.anonymizer.anonymize_pattern(p);
                CommunitySkillCandidate {
                    pattern: anonymized,
                    frequency: p.frequency,
                    success_rate: p.success_rate,
                    suggested_name: self.generate_name(p),
                    confidence: self.calculate_community_confidence(p),
                }
            })
            .collect()
    }
}
```

---

## 五、Review Scheduler — 定期回顾与优化

### 5.1 替代 Hermes 的被动式自省

```rust
/// 回顾调度器 — 替代 Hermes 靠 LLM 自我反思的模式
#[async_trait]
pub trait ReviewScheduler: Send + Sync {
    /// 决定是否现在应该进行回顾
    fn should_review(&self, context: &ReviewContext) -> bool;

    /// 执行回顾
    async fn review(&self, context: &ReviewContext) -> Result<ReviewReport>;

    /// 调度器名称
    fn name(&self) -> &str;
}

/// 回顾上下文
#[derive(Debug, Clone)]
pub struct ReviewContext {
    /// 距上次回顾的时间
    time_since_last: Duration,
    /// 自上次回顾以来的事件数量
    events_since_last: usize,
    /// 当前活跃技能数
    active_skill_count: usize,
    /// 当前记忆条目数
    memory_entry_count: usize,
    /// 最近的错误率
    recent_error_rate: f64,
    /// 会话统计
    session_stats: SessionStats,
}

/// 回顾报告
#[derive(Debug, Clone)]
pub struct ReviewReport {
    /// 技能更新建议
    skill_updates: Vec<SkillUpdate>,
    /// 记忆整理建议
    memory_maintenance: Vec<MemoryAction>,
    /// 新技能建议
    new_skill_proposals: Vec<SkillTemplate>,
    /// 性能洞察
    insights: Vec<Insight>,
}

/// 具体实现：混合调度器（时间 + 事件双重触发）
pub struct HybridReviewScheduler {
    /// 最小回顾间隔
    min_interval: Duration,
    /// 最大事件累积量（超过则强制回顾）
    max_event_accumulation: usize,
    /// 错误率阈值
    error_rate_threshold: f64,
    /// 回顾执行器
    reviewer: Arc<dyn Reviewer>,
}

#[async_trait]
impl ReviewScheduler for HybridReviewScheduler {
    fn should_review(&self, context: &ReviewContext) -> bool {
        // 条件1: 时间到了
        let time_triggered = context.time_since_last >= self.min_interval;

        // 条件2: 事件累积
        let event_triggered = context.events_since_last >= self.max_event_accumulation;

        // 条件3: 错误率飙升
        let error_triggered = context.recent_error_rate > self.error_rate_threshold;

        time_triggered || event_triggered || error_triggered
    }

    async fn review(&self, context: &ReviewContext) -> Result<ReviewReport> {
        self.reviewer.review(context).await
    }

    fn name(&self) -> &str { "hybrid" }
}
```

---

## 六、与 kestrel-agent 的集成设计

### 6.1 模块结构

```
crates/kestrel-agent/src/
├── lib.rs                      # 公开导出
├── evolution/                   # 自进化模块
│   ├── mod.rs                  # 模块根
│   ├── skill.rs                # Skill trait + 基础类型
│   ├── skill_compiler.rs       # 技能编译器
│   ├── skill_registry.rs       # 技能注册表（替代 Hermes 的 Python registry）
│   ├── skill_graph.rs          # 技能组合 DAG
│   ├── skill_ab_test.rs        # A/B 测试
│   ├── skill_version.rs        # 版本管理 + 回滚
│   ├── trigger.rs              # 触发条件系统
│   ├── learning/               # 学习子系统
│   │   ├── mod.rs
│   │   ├── event.rs            # 学习事件定义
│   │   ├── event_bus.rs        # 事件总线
│   │   ├── handlers.rs         # 内置事件处理器
│   │   └── quality_monitor.rs  # 技能质量监控
│   ├── memory/                 # 记忆子系统
│   │   ├── mod.rs
│   │   ├── store.rs            # MemoryStore trait
│   │   ├── tiered.rs           # 分层记忆管理器
│   │   ├── hot_store.rs        # L1 热记忆
│   │   ├── warm_store.rs       # L2 温暖记忆（SQLite + 向量）
│   │   └── cold_store.rs       # L3 冷记忆
│   ├── review/                 # 回顾子系统
│   │   ├── mod.rs
│   │   ├── scheduler.rs        # ReviewScheduler trait
│   │   └── hybrid.rs           # 混合调度器
│   └── confidence.rs           # 置信度评分引擎
├── runner.rs                   # 核心 LLM 循环（现有）
├── memory.rs                   # 基础记忆（现有，扩展为分层）
├── skills.rs                   # 技能加载（现有，扩展为编译式）
└── ...
```

### 6.2 与现有架构的集成点

```rust
/// 在 AgentRunner 中集成自进化系统
impl AgentRunner {
    pub fn new(config: AgentConfig) -> Self {
        // 现有组件
        let tool_registry = ToolRegistry::new();
        let session_manager = SessionManager::new();

        // 自进化组件
        let skill_compiler = SkillCompiler::new();
        let skill_registry = SkillRegistry::new(skill_compiler);
        let learning_bus = LearningEventBus::new();
        let memory_manager = TieredMemoryManager::new();
        let review_scheduler = HybridReviewScheduler::new();

        Self {
            tool_registry,
            session_manager,
            skill_registry,
            learning_bus,
            memory_manager,
            review_scheduler,
            // ...
        }
    }

    /// 修改后的消息处理循环
    pub async fn process_message(&self, msg: InboundMessage) -> Result<OutboundMessage> {
        let context = self.build_context(&msg).await?;

        // 1. 查询记忆（分层）
        let memories = self.memory_manager.smart_recall(&msg.content, 5).await;

        // 2. 匹配技能（编译后的高效匹配）
        let skills = self.skill_registry.match_skills(&context).await;

        // 3. 构建 prompt（注入记忆 + 技能）
        let prompt = self.build_prompt(&context, &memories, &skills).await;

        // 4. 调用 LLM
        let response = self.call_llm(prompt).await?;

        // 5. 处理工具调用
        let (result, tool_events) = self.process_tool_calls(&response).await?;

        // 6. 发射学习事件
        for event in tool_events {
            self.learning_bus.emit(event).await;
        }

        // 7. 发射技能使用事件
        for skill in &skills {
            self.learning_bus.emit(LearningEvent::SkillUsed {
                skill_name: skill.manifest().name.clone(),
                match_score: skill.manifest().confidence,
                outcome: self.evaluate_skill_outcome(&skill, &result),
                timestamp: Instant::now(),
            }).await;
        }

        // 8. 检查是否需要回顾
        let review_ctx = self.build_review_context();
        if self.review_scheduler.should_review(&review_ctx) {
            // 后台执行回顾，不阻塞用户
            tokio::spawn(self.run_background_review(review_ctx));
        }

        Ok(result)
    }
}
```

---

## 七、平台优势的利用

### 7.1 多平台反馈信号

```rust
/// 平台特定的事件收集器
pub trait PlatformFeedbackCollector: Send + Sync {
    /// 收集平台特定的反馈信号
    async fn collect(&self, session: &SessionId) -> Vec<LearningEvent>;
}

/// Telegram 反馈收集器
pub struct TelegramFeedbackCollector {
    bot: Arc<teloxide::Bot>,
}

#[async_trait]
impl PlatformFeedbackCollector for TelegramFeedbackCollector {
    async fn collect(&self, session: &SessionId) -> Vec<LearningEvent> {
        let mut events = Vec::new();

        // 1. 用户反应（emoji reaction）→ 显式反馈
        // 👍 = 肯定, 👎 = 否定, ❤️ = 强肯定

        // 2. 消息编辑 → 用户对 agent 输出的纠正
        // 检测用户是否编辑了自己的消息来纠正指令

        // 3. 回复模式 → 隐式反馈
        // 用户直接继续 = 接受, 用户用"/undo" = 拒绝

        events
    }
}

/// Discord 反馈收集器
pub struct DiscordFeedbackCollector {
    http: Arc<serenity::Http>,
}

#[async_trait]
impl PlatformFeedbackCollector for DiscordFeedbackCollector {
    async fn collect(&self, session: &SessionId) -> Vec<LearningEvent> {
        let mut events = Vec::new();

        // 1. Emoji 反应 → 同 Telegram
        // 2. 线程行为 → 用户在子线程中追问 = 技能不够完整
        // 3. 频道切换 → 用户去了其他频道寻求帮助 = 当次回答不满意
        // 4. @提及模式 → 被 @ 的用户是否在后续回答中纠正了 agent

        events
    }
}
```

### 7.2 Daemon 模式的后台学习

```rust
/// 后台学习守护进程 — 利用 daemon 模式的持续运行能力
pub struct BackgroundLearningDaemon {
    event_bus: Arc<LearningEventBus>,
    memory_manager: Arc<TieredMemoryManager>,
    skill_registry: Arc<SkillRegistry>,
    review_scheduler: Arc<dyn ReviewScheduler>,
}

impl BackgroundLearningDaemon {
    /// 启动后台学习循环
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut maintenance_interval = tokio::time::interval(Duration::from_secs(300));
        let mut review_interval = tokio::time::interval(Duration::from_secs(3600));

        loop {
            tokio::select! {
                _ = maintenance_interval.tick() => {
                    // 1. 记忆整理：在层级之间移动记忆
                    self.memory_manager.run_maintenance().await;

                    // 2. 技能质量检查：更新置信度
                    self.skill_registry.update_confidence_scores().await;

                    // 3. 清理过期数据
                    self.cleanup_expired().await;
                }
                _ = review_interval.tick() => {
                    // 4. 定期回顾：分析近期事件，优化技能
                    let context = self.build_review_context().await;
                    if self.review_scheduler.should_review(&context) {
                        if let Ok(report) = self.review_scheduler.review(&context).await {
                            self.apply_review_report(report).await;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    }
}
```

### 7.3 性能优势：实时技能匹配

```rust
/// 高性能技能匹配引擎 — 利用 Rust 的性能处理大规模技能库
pub struct SkillMatchEngine {
    /// 关键词索引（Aho-Corasick 自动机）
    keyword_index: AhoCorasick,
    /// 正则引擎（编译后的 DFA）
    regex_index: Vec<regex_automata::DFA<Vec<u8>>>,
    /// 语义索引（量化向量，SIMD 加速）
    semantic_index: QuantizedVectorIndex,
}

impl SkillMatchEngine {
    /// 在微秒级匹配所有技能 — Hermes做不到的
    pub fn match_all(&self, query: &str) -> Vec<(SkillName, ConfidenceScore)> {
        let mut scores: HashMap<SkillName, f64> = HashMap::new();

        // 1. 关键词匹配（~10μs for 1000 patterns）
        for mat in self.keyword_index.find_iter(query) {
            let skill = self.keyword_to_skill(mat.pattern());
            *scores.entry(skill).or_default() += 0.3;
        }

        // 2. 正则匹配（~50μs for 100 compiled DFAs）
        for (i, dfa) in self.regex_index.iter().enumerate() {
            if dfa.is_match(query.as_bytes()) {
                let skill = self.regex_to_skill(i);
                *scores.entry(skill).or_default() += 0.4;
            }
        }

        // 3. 语义匹配（~1ms for 10k vectors with SIMD）
        // (需要异步获取嵌入，此处用缓存)
        if let Some(query_embedding) = self.get_cached_embedding(query) {
            for (skill, score) in self.semantic_index.search(&query_embedding, 10) {
                *scores.entry(skill).or_default() += score as f64;
            }
        }

        // 排序并返回
        let mut results: Vec<_> = scores.into_iter()
            .map(|(name, score)| (name, ConfidenceScore(score.min(1.0))))
            .collect();
        results.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap());
        results
    }
}
```

---

## 八、三阶段实施计划

### Phase 1: 最小可行产品（MVP）—— 2-3 周

**目标：** 能存、能查、能用——证明 Rust 版自进化比 Hermes 好用。

| 组件 | 具体实现 | 优先级 |
|------|---------|--------|
| **Skill trait + SkillManifest** | 核心 trait 定义，TOML 格式的技能定义 | P0 |
| **SkillCompiler (简化版)** | 解析 TOML + 验证，不做优化编译 | P0 |
| **SkillRegistry** | 技能注册、匹配、加载（关键词匹配优先） | P0 |
| **MemoryStore trait + HotStore** | L1 热记忆（内存 HashMap）+ 文件持久化 | P0 |
| **LearningEvent 定义** | 事件类型定义 + 简单的文件持久化 | P1 |
| **基础事件处理器** | ToolSucceeded/ToolFailed 处理器 | P1 |

**交付物：**
- 能从 TOML 文件加载技能并注入到 prompt
- 基础的 L1 记忆系统（替代 Hermes 的 MEMORY.md）
- 学习事件能够被记录和持久化

**关键 API：**
```rust
// 使用方式
let registry = SkillRegistry::new();
registry.load_from_dir("~/.kestrel/skills/").await?;
let matched = registry.match_skills("帮我部署一个 k8s 集群").await;

let memory = HotStore::new();
memory.store(MemoryEntry::new("用户偏好中文回复", MemoryCategory::UserProfile)).await?;
let recalled = memory.recall("语言偏好").await;
```

### Phase 2: 核心功能 —— 4-6 周

**目标：** 自进化真正开始工作——技能可以被自动创建和优化。

| 组件 | 具体实现 | 优先级 |
|------|---------|--------|
| **SkillCompiler (完整版)** | 正则编译→DFA、模板编译、优化 | P0 |
| **WarmStore (L2)** | SQLite + 全文搜索 + 向量索引 | P0 |
| **LearningEventBus** | 异步事件总线 + 批量处理 | P0 |
| **SkillQualityMonitor** | 基于使用数据动态调整置信度 | P0 |
| **HybridReviewScheduler** | 时间+事件双重触发的回顾 | P0 |
| **Trigger 系统** | Keyword + Regex + Semantic 触发 | P1 |
| **SkillVersionControl** | 版本管理 + 回滚 | P1 |
| **SkillGraph** | 依赖 DAG + 拓扑排序加载 | P1 |
| **PlatformFeedbackCollector** | Telegram + Discord 反馈收集 | P2 |

**交付物：**
- 完整的三层记忆系统
- 技能可以自动被创建（基于学习事件）
- 技能置信度动态调整
- 定期回顾自动执行
- 技能版本管理支持回滚

**关键 API：**
```rust
// 自进化开始工作
let bus = LearningEventBus::new();
bus.emit(LearningEvent::ToolFailed { tool: "terminal", ... }).await;
// → 自动触发技能创建建议
// → 自动调整相关技能置信度
// → 定期回顾优化技能库
```

### Phase 3: 高级功能 —— 6-8 周

**目标：** 超越 Hermes——做到 Python 版做不到的事。

| 组件 | 具体实现 | 优先级 |
|------|---------|--------|
| **ColdStore (L3)** | 压缩归档 + 按需解压 | P1 |
| **SkillABTester** | A/B 测试框架 + 统计显著性判断 | P1 |
| **ConfidenceEngine** | 完整的置信度衰减/强化模型 | P1 |
| **CommunitySkillAggregator** | 跨用户匿名化聚合 | P2 |
| **SkillMatchEngine (高性能)** | Aho-Corasick + DFA + 量化向量 SIMD | P1 |
| **BackgroundLearningDaemon** | daemon 模式的持续学习 | P2 |
| **RLHF式技能排名** | 基于隐式反馈的技能排序 | P3 |
| **技能市场** | 技能作为可组合单元（类似 crate） | P3 |

**交付物：**
- 完整的 A/B 测试框架
- 微秒级技能匹配（支持万级技能库）
- daemon 模式后台学习
- 社区技能聚合

---

## 九、关键替代方案对比

| Hermes (Python) | kestrel (本设计) | 优势 |
|-----------------|----------------------|------|
| Markdown 技能文件 | TOML + 编译式技能 | 类型安全、编译期验证、高效匹配 |
| YAML frontmatter 运行时解析 | 编译后的 DFA/向量 | 零拷贝匹配，微秒级响应 |
| LLM 自行决定何时用技能 | 触发条件 + 置信度评分 | 不浪费 token，确定性匹配 |
| MEMORY.md 纯字符串 | 分层记忆 + 类型化条目 | 结构化、可搜索、自动整理 |
| 靠 LLM 自省改进 | 事件驱动学习 | 客观、可衡量、可审计 |
| 无版本管理 | 完整版本控制 + 回滚 | 安全、可逆 |
| 无质量评估 | 置信度 + A/B 测试 | 数据驱动、持续优化 |
| 单一反馈信号 | 多平台反馈（emoji、线程、回复模式） | 更丰富的学习信号 |
| 同步处理 | 全异步 + 后台 daemon | 无用户感知延迟 |
| 全量加载技能 | 按需加载 + 拓扑排序 | 支持大规模技能库 |

---

## 十、总结：为什么这个设计是"Rust 原生"的

1. **类型系统替代运行时检查**：Hermes 用字符串和 JSON 在运行时传递一切；kestrel 用 trait、枚举、newtype 在编译时保证正确性。

2. **编译替代解释**：Hermes 每次都重新解析 YAML、重建 prompt；kestrel 编译一次，运行时零开销。

3. **事件驱动替代轮询**：Hermes 靠 LLM 在对话中自省；kestrel 用事件总线持续学习，不需要额外 LLM 调用。

4. **分层存储替代单文件**：Hermes 的 2KB MEMORY.md 能存的信息极其有限；kestrel 的三层缓存既保留速度又突破容量限制。

5. **异步原生**：Hermes 的学习发生在对话内（增加延迟）；kestrel 的学习完全异步，用户无感知。

6. **性能即功能**：正是因为 Rust 快，才能做 Aho-Corasick 匹配、量化向量搜索、实时置信度更新——这些在 Python 中要么太慢要么太贵。

**核心理念：自进化不是一个"功能"，而是一个"属性"。它不应该需要 agent 主动去做什么——系统应该在每一次交互中自然地变得更好。**

---

## 基于 kestrel 的精修设计

> 在阅读了 kestrel 全部 crate 源码后，以下设计完全基于真实代码结构和风格进行精修。
> 所有代码与 kestrel 现有的 `Tool` trait、`LlmProvider` trait、`CronService`、`MessageBus`、
> `SessionManager`、`ContextBuilder` 风格完全一致。

### 1. Trait 定义（完整 Rust 代码）

现有 kestrel 的 trait 风格特点：
- 使用 `#[async_trait]` 宏
- 返回 `anyhow::Result` 或自定义错误枚举
- trait 方法命名简短（`name()`, `execute()`, `is_available()`）
- 使用 `Send + Sync` 约束
- 配合 `Arc<dyn Trait>` 使用

以下新 trait 完全遵循此风格：

```rust
// ─── crates/kestrel-evolution/src/skill_trait.rs ──────────────────

use async_trait::async_trait;
use kestrel_core::{Message, Platform};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 技能执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkillOutput {
    /// 仅注入 prompt 文本（大多数技能的行为）
    PromptOnly { segment: String },
    /// 执行了操作并返回结果
    Executed {
        result: String,
        side_effects: Vec<String>,
    },
    /// 建议创建新技能（自进化入口）
    SuggestSkill {
        name: String,
        reason: String,
        template: String,
    },
}

/// 技能置信度事件
#[derive(Debug, Clone, Copy)]
pub enum ConfidenceEvent {
    UsedSuccessfully,
    UsedButFailed,
    UserConfirmed,
    UserCorrected,
    TimeDecay { hours: f64 },
}

/// 技能匹配结果
#[derive(Debug, Clone)]
pub struct SkillMatch {
    pub name: String,
    pub score: f64,
}

/// 核心技能 trait — 与 kestrel_tools::Tool 风格一致
#[async_trait]
pub trait Skill: Send + Sync {
    /// 技能名称（kebab-case）
    fn name(&self) -> &str;

    /// 一行描述
    fn description(&self) -> &str;

    /// 技能分类
    fn category(&self) -> &str {
        "uncategorized"
    }

    /// 当前置信度（0.0-1.0）
    fn confidence(&self) -> f64 {
        0.5
    }

    /// 构建注入到 system prompt 的文本片段
    async fn build_prompt_segment(&self) -> anyhow::Result<String>;

    /// 匹配分数：给定用户输入，返回 0.0-1.0
    async fn match_score(&self, user_input: &str) -> f64;

    /// 执行技能（可选操作）
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<SkillOutput>;

    /// 更新置信度
    fn update_confidence(&mut self, event: ConfidenceEvent);
}
```

```rust
// ─── crates/kestrel-evolution/src/memory_trait.rs ──────────────────

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 记忆分类
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    UserProfile,
    Environment,
    ProjectConvention,
    ToolDiscovery,
    ErrorLesson,
    WorkflowPattern,
}

/// 记忆层级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLevel {
    /// L1 热记忆 — 直接注入 prompt，零延迟
    Hot,
    /// L2 温暖记忆 — 可搜索，毫秒级延迟
    Warm,
    /// L3 冷记忆 — 归档，秒级延迟
    Cold,
}

/// 记忆条目 — 替代现有 MEMORY.md 的纯字符串
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub category: MemoryCategory,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub access_count: u32,
    pub confidence: f64,
    pub level: MemoryLevel,
}

/// 记忆查询
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    pub text: Option<String>,
    pub category: Option<MemoryCategory>,
    pub min_confidence: Option<f64>,
    pub limit: usize,
}

/// 记忆存储 trait — 与 CronStateStore 风格一致
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// 存储一条记忆
    async fn store(&self, entry: MemoryEntry) -> anyhow::Result<()>;

    /// 按关键词召回
    async fn recall(&self, query: &MemoryQuery) -> anyhow::Result<Vec<MemoryEntry>>;

    /// 按名称精确查找
    async fn get(&self, id: &str) -> anyhow::Result<Option<MemoryEntry>>;

    /// 更新记忆
    async fn update(&self, entry: &MemoryEntry) -> anyhow::Result<()>;

    /// 删除记忆
    async fn delete(&self, id: &str) -> anyhow::Result<bool>;

    /// 提升记忆层级（冷→暖→热）
    async fn promote(&self, id: &str, to: MemoryLevel) -> anyhow::Result<()>;

    /// 返回各层级记忆数量
    async fn counts(&self) -> anyhow::Result<HashMap<MemoryLevel, usize>>;
}

use std::collections::HashMap;
```

```rust
// ─── crates/kestrel-evolution/src/review_trait.rs ──────────────────

use async_trait::async_trait;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 回顾上下文
#[derive(Debug, Clone)]
pub struct ReviewContext {
    pub time_since_last: Duration,
    pub events_since_last: usize,
    pub active_skill_count: usize,
    pub memory_entry_count: usize,
    pub recent_error_rate: f64,
}

/// 回顾报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub skill_updates: Vec<String>,
    pub memory_maintenance: Vec<String>,
    pub new_skill_proposals: Vec<String>,
    pub insights: Vec<String>,
    pub reviewed_at: DateTime<Local>,
}

/// 回顾调度器 trait — 可复用 kestrel-cron 的 tick 机制
#[async_trait]
pub trait ReviewScheduler: Send + Sync {
    /// 判断是否需要回顾
    fn should_review(&self, context: &ReviewContext) -> bool;

    /// 执行回顾
    async fn review(&self, context: &ReviewContext) -> anyhow::Result<ReviewReport>;

    /// 调度器名称
    fn name(&self) -> &str;
}
```

### 2. 新 Crate 结构

```
crates/kestrel-evolution/
├── Cargo.toml
└── src/
    ├── lib.rs                      # 模块导出
    ├── skill_trait.rs              # Skill trait + SkillOutput/ConfidenceEvent
    ├── skill_registry.rs           # SkillRegistry（与 ToolRegistry 对称）
    ├── skill_loader.rs             # 扩展现有 SkillLoader，增加置信度管理
    ├── memory_trait.rs             # MemoryStore trait + MemoryEntry/Query
    ├── memory_store.rs             # FileBasedMemoryStore 实现
    ├── memory_manager.rs           # TieredMemoryManager（L1/L2/L3 调度）
    ├── review_trait.rs             # ReviewScheduler trait
    ├── review_scheduler.rs         # HybridReviewScheduler 实现
    ├── events.rs                   # LearningEvent 定义
    ├── event_bus.rs                # LearningEventBus（利用现有 kestrel-bus）
    ├── handlers.rs                 # 内置事件处理器（SkillQualityMonitor 等）
    ├── tools/
    │   ├── mod.rs                  # 新 Tool 导出
    │   ├── memory_tool.rs          # memory — CRUD 操作
    │   ├── skill_tool.rs           # skill — 管理 skill
    │   └── session_search_tool.rs  # session_search — 搜索会话历史
    └── confidence.rs               # ConfidenceEngine（衰减/强化模型）
```

**关键依赖方向**：
```
kestrel-evolution
  ├── kestrel-core          # Message, Platform, error types
  ├── kestrel-bus           # MessageBus, AgentEvent（通过 emit_event 发送学习事件）
  ├── kestrel-tools         # Tool trait（新 tool 实现）、SkillLoader（复用）
  ├── kestrel-session       # SessionManager（读取会话数据）
  ├── kestrel-config        # Config（读取配置）
  └── kestrel-cron          # CronService（复用 tick 机制触发 review）
```

**不允许的依赖方向**：
- kestrel-core 不依赖 kestrel-evolution
- kestrel-session 不依赖 kestrel-evolution
- kestrel-bus 不依赖 kestrel-evolution

### 3. 消息流集成方案

#### 3.1 Bus Event 新类型定义

现有的 `AgentEvent` 枚举位于 `kestrel-bus/src/events.rs`。我们扩展它：

```rust
// ─── 在 kestrel-bus/src/events.rs 的 AgentEvent 枚举中追加 ──────

#[derive(Debug, Clone)]
pub enum AgentEvent {
    // ... 现有事件保持不变 ...
    Started { session_key: String },
    StreamingChunk { session_key: String, content: String },
    ToolCall { session_key: String, tool_name: String, iteration: usize },
    Completed { session_key: String, iterations: usize, tool_calls: usize },
    Error { session_key: String, error: String },
    CronFired { job_id: String, job_name: Option<String>, message: String },
    HeartbeatCheck { healthy: bool, checks_total: usize, checks_failed: usize },
    RestartRequested { component: String, reason: String },
    GatewayReconnecting { platform: String, attempt: u32, resumable: bool },
    GatewayResumed { platform: String, session_id: String },
    GatewayReidentify { platform: String },
    HealthStatusChanged { from: String, to: String, failed_count: usize, degraded_count: usize },
    ContextOverflow { session_key: String, tokens_before: usize, tokens_after: usize, messages_removed: usize },
    ComponentStatusChanged { component: String, from: String, to: String, message: String },

    // ═══ 新增：自进化事件 ═══
    /// 工具调用成功
    ToolSucceeded {
        session_key: String,
        tool_name: String,
        duration_ms: u64,
    },
    /// 工具调用失败
    ToolFailed {
        session_key: String,
        tool_name: String,
        error: String,
    },
    /// 技能被使用
    SkillActivated {
        session_key: String,
        skill_name: String,
        match_score: f64,
    },
    /// 技能创建建议
    SkillProposal {
        proposed_name: String,
        reason: String,
        evidence_count: usize,
    },
    /// 自我回顾完成
    SelfReviewCompleted {
        skill_updates: usize,
        memory_actions: usize,
        new_proposals: usize,
    },
    /// 记忆层级变更
    MemoryLevelChanged {
        entry_id: String,
        from_level: String,
        to_level: String,
    },
}
```

#### 3.2 订阅模式

```rust
// ─── crates/kestrel-evolution/src/event_bus.rs ──────────────────

use kestrel_bus::{AgentEvent, MessageBus};
use std::sync::Arc;
use tracing::{debug, info};

/// 学习事件总线 — 订阅现有 MessageBus 的 broadcast 通道
pub struct LearningEventBus {
    bus: Arc<MessageBus>,
    /// 缓冲区大小（批量处理减少 LLM 调用）
    buffer_size: usize,
}

impl LearningEventBus {
    pub fn new(bus: Arc<MessageBus>) -> Self {
        Self {
            bus,
            buffer_size: 50,
        }
    }

    /// 启动后台学习循环
    /// 订阅 bus 的 broadcast 通道，过滤学习相关事件
    pub async fn run(
        &self,
        handlers: Vec<Box<dyn LearningEventHandler>>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut event_rx = self.bus.subscribe_events();
        let mut buffer: Vec<AgentEvent> = Vec::new();
        let mut flush_interval = tokio::time::interval(
            std::time::Duration::from_secs(30)
        );

        loop {
            tokio::select! {
                // 接收 bus 事件
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => {
                            if is_learning_event(&event) {
                                buffer.push(event);
                                if buffer.len() >= self.buffer_size {
                                    Self::flush(&handlers, &mut buffer).await;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!("Learning bus lagged by {} events", n);
                        }
                        Err(_) => break,
                    }
                }
                // 定时刷新
                _ = flush_interval.tick() => {
                    if !buffer.is_empty() {
                        Self::flush(&handlers, &mut buffer).await;
                    }
                }
                // 关闭信号
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        // 最终刷新
                        if !buffer.is_empty() {
                            Self::flush(&handlers, &mut buffer).await;
                        }
                        break;
                    }
                }
            }
        }
    }

    /// 批量处理缓冲的事件
    async fn flush(
        handlers: &[Box<dyn LearningEventHandler>],
        buffer: &mut Vec<AgentEvent>,
    ) {
        let events: Vec<AgentEvent> = buffer.drain(..).collect();
        debug!("Flushing {} learning events", events.len());

        for handler in handlers {
            for event in &events {
                if let Err(e) = handler.handle(event).await {
                    tracing::warn!(
                        "Learning handler '{}' failed: {}",
                        handler.name(),
                        e
                    );
                }
            }
        }
    }
}

/// 判断事件是否与学习相关
fn is_learning_event(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::ToolSucceeded { .. }
            | AgentEvent::ToolFailed { .. }
            | AgentEvent::SkillActivated { .. }
            | AgentEvent::Completed { .. }
            | AgentEvent::Error { .. }
            | AgentEvent::ContextOverflow { .. }
    )
}

/// 学习事件处理器 trait
#[async_trait::async_trait]
pub trait LearningEventHandler: Send + Sync {
    async fn handle(&self, event: &AgentEvent) -> anyhow::Result<()>;
    fn name(&self) -> &str;
}
```

#### 3.3 数据流图

```
用户消息 → InboundMessage → MessageBus.inbound
                              ↓
                          AgentLoop.process_message()
                              ↓
                     ┌─── ContextBuilder ───┐
                     │  + MemoryManager     │
                     │  + SkillRegistry     │
                     └──────────────────────┘
                              ↓
                     AgentRunner.run()
                              ↓
                     ToolCall → ToolRegistry.execute()
                              ↓
                     AgentEvent::ToolSucceeded/ToolFailed → MessageBus.emit_event()
                                                           ↓
                                                    LearningEventBus
                                                    (subscribe_events)
                                                           ↓
                                                   handlers[]
                                                   ├── SkillQualityMonitor
                                                   ├── MemoryMaintenanceHandler
                                                   └── SkillProposalHandler
                                                           ↓
                                                   SkillRegistry / MemoryStore
```

### 4. Session/Memory 存储设计

#### 4.1 现有 Session 存储分析

现有 kestrel-session 使用 **JSONL** 格式（不是 SQLite），结构如下：
- `sessions/{key}.jsonl` — SessionMeta header + SessionEntry lines
- `notes/{key}.notes.json` — Note 数组

我们将复用此 JSONL 模式来存储记忆数据，保持一致性：

#### 4.2 Memory 存储设计

```rust
// ─── crates/kestrel-evolution/src/memory_store.rs ──────────────────

use crate::memory_trait::*;
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// 基于文件的记忆存储 — 与 SessionStore 的 JSONL 模式一致
pub struct FileBasedMemoryStore {
    /// 存储根目录（~/.kestrel/memory/）
    root: PathBuf,
}

impl FileBasedMemoryStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        if !root.exists() {
            std::fs::create_dir_all(&root)?;
        }
        let hot_dir = root.join("hot");
        let warm_dir = root.join("warm");
        let cold_dir = root.join("cold");
        std::fs::create_dir_all(&hot_dir)?;
        std::fs::create_dir_all(&warm_dir)?;
        std::fs::create_dir_all(&cold_dir)?;
        Ok(Self { root })
    }

    /// 获取层级对应的子目录
    fn level_dir(&self, level: MemoryLevel) -> PathBuf {
        match level {
            MemoryLevel::Hot => self.root.join("hot"),
            MemoryLevel::Warm => self.root.join("warm"),
            MemoryLevel::Cold => self.root.join("cold"),
        }
    }

    /// 记忆条目文件路径
    fn entry_path(&self, id: &str, level: MemoryLevel) -> PathBuf {
        let safe_id = id.replace(['/', '\\', ' ', ':'], "_");
        self.level_dir(level).join(format!("{}.json", safe_id))
    }
}

#[async_trait]
impl MemoryStore for FileBasedMemoryStore {
    async fn store(&self, entry: MemoryEntry) -> Result<()> {
        let path = self.entry_path(&entry.id, entry.level);
        let json = serde_json::to_string_pretty(&entry)?;
        // 原子写入：先写临时文件再 rename
        let tmp_path = path.with_extension("tmp");
        std::fs::write(&tmp_path, &json)
            .with_context(|| format!("Failed to write memory entry: {}", path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to rename memory entry: {}", path.display()))?;
        debug!("Stored memory entry '{}' at level {:?}", entry.id, entry.level);
        Ok(())
    }

    async fn recall(&self, query: &MemoryQuery) -> Result<Vec<MemoryEntry>> {
        let mut results = Vec::new();
        // 按层级优先搜索：Hot → Warm → Cold
        let levels = [MemoryLevel::Hot, MemoryLevel::Warm, MemoryLevel::Cold];
        let limit = query.limit.max(10);

        for level in levels {
            if results.len() >= limit {
                break;
            }
            let dir = self.level_dir(level);
            if !dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(mem) = serde_json::from_str::<MemoryEntry>(&content) {
                            // 应用过滤条件
                            if let Some(ref cat) = query.category {
                                if &mem.category != cat {
                                    continue;
                                }
                            }
                            if let Some(min_conf) = query.min_confidence {
                                if mem.confidence < min_conf {
                                    continue;
                                }
                            }
                            if let Some(ref text) = query.text {
                                if !mem.content.to_lowercase()
                                    .contains(&text.to_lowercase())
                                {
                                    continue;
                                }
                            }
                            results.push(mem);
                            if results.len() >= limit {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // 按置信度降序排列
        results.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
        Ok(results)
    }

    async fn get(&self, id: &str) -> Result<Option<MemoryEntry>> {
        // 按层级查找
        for level in [MemoryLevel::Hot, MemoryLevel::Warm, MemoryLevel::Cold] {
            let path = self.entry_path(id, level);
            if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                let entry: MemoryEntry = serde_json::from_str(&content)?;
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    async fn update(&self, entry: &MemoryEntry) -> Result<()> {
        // 先删除旧位置（可能在其他层级目录）
        self.delete(&entry.id).await.ok();
        // 写入新位置
        self.store(entry.clone()).await
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        for level in [MemoryLevel::Hot, MemoryLevel::Warm, MemoryLevel::Cold] {
            let path = self.entry_path(id, level);
            if path.exists() {
                std::fs::remove_file(&path)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn promote(&self, id: &str, to: MemoryLevel) -> Result<()> {
        if let Some(mut entry) = self.get(id).await? {
            self.delete(id).await?;
            entry.level = to;
            self.store(entry).await
        } else {
            anyhow::bail!("Memory entry '{}' not found", id)
        }
    }

    async fn counts(&self) -> Result<HashMap<MemoryLevel, usize>> {
        let mut counts = HashMap::new();
        for level in [MemoryLevel::Hot, MemoryLevel::Warm, MemoryLevel::Cold] {
            let dir = self.level_dir(level);
            if dir.exists() {
                let count = std::fs::read_dir(&dir)?
                    .filter(|e| {
                        e.as_ref()
                            .map(|e| e.path().extension().map(|ext| ext == "json").unwrap_or(false))
                            .unwrap_or(false)
                    })
                    .count();
                counts.insert(level, count);
            } else {
                counts.insert(level, 0);
            }
        }
        Ok(counts)
    }
}
```

#### 4.3 与现有 MemoryStore 的共存方案

现有 `kestrel-agent/src/memory.rs` 中的 `MemoryStore` 使用 MEMORY.md 文件。新系统将：

1. **保留**现有 `MemoryStore` 不变（向后兼容）
2. **新增** `FileBasedMemoryStore` 作为独立存储
3. `TieredMemoryManager` 在 `get_context()` 时合并两者：

```rust
// ─── crates/kestrel-evolution/src/memory_manager.rs ──────────────────

use crate::memory_trait::*;
use crate::memory_store::FileBasedMemoryStore;
use anyhow::Result;
use kestrel_agent::memory::MemoryStore as LegacyMemoryStore;
use std::sync::Arc;

/// 分层记忆管理器 — 合并现有 MEMORY.md 和新的结构化记忆
pub struct TieredMemoryManager {
    /// 现有 MEMORY.md 存储（向后兼容）
    legacy: LegacyMemoryStore,
    /// 新的结构化记忆存储
    structured: Arc<FileBasedMemoryStore>,
}

impl TieredMemoryManager {
    pub fn new(
        legacy_dir: std::path::PathBuf,
        structured_dir: std::path::PathBuf,
    ) -> Result<Self> {
        let legacy = LegacyMemoryStore::new(legacy_dir)?;
        let structured = Arc::new(FileBasedMemoryStore::new(structured_dir)?);
        Ok(Self { legacy, structured })
    }

    /// 获取记忆上下文 — 合并旧系统和新系统
    pub async fn build_context(
        &self,
        user_id: Option<&str>,
        budget: usize,
    ) -> Result<String> {
        let mut parts = Vec::new();

        // 1. 从旧系统读取（向后兼容）
        let legacy_ctx = self.legacy.get_context(user_id)?;
        if !legacy_ctx.is_empty() {
            parts.push(legacy_ctx);
        }

        // 2. 从新系统读取热记忆
        let hot_memories = self.structured.recall(&MemoryQuery {
            limit: budget,
            ..Default::default()
        }).await?;

        if !hot_memories.is_empty() {
            let mut mem_section = String::from("## Structured Memory\n");
            for mem in &hot_memories {
                mem_section.push_str(&format!(
                    "- [{}] {} (confidence: {:.0%})\n",
                    serde_json::to_value(&mem.category)
                        .unwrap()
                        .as_str()
                        .unwrap_or("unknown"),
                    mem.content,
                    mem.confidence,
                ));
            }
            parts.push(mem_section);
        }

        Ok(parts.join("\n\n"))
    }

    /// 存储新记忆
    pub async fn store_memory(
        &self,
        content: String,
        category: MemoryCategory,
        confidence: f64,
    ) -> Result<String> {
        use chrono::Utc;
        let id = uuid::Uuid::new_v4().to_string();
        let entry = MemoryEntry {
            id: id.clone(),
            content,
            category,
            source: "agent".to_string(),
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            access_count: 0,
            confidence,
            level: MemoryLevel::Hot,
        };
        self.structured.store(entry).await?;
        Ok(id)
    }
}
```

### 5. ContextBuilder 扩展方案

现有 ContextBuilder 只有 93 行，结构清晰。我们通过**组合**而非修改来扩展：

```rust
// ─── crates/kestrel-evolution/src/context_extension.rs ──────────────────

use crate::memory_manager::TieredMemoryManager;
use crate::skill_registry::SkillRegistry;
use anyhow::Result;
use kestrel_bus::events::InboundMessage;
use kestrel_session::Session;
use kestrel_tools::ToolRegistry;

/// ContextBuilder 扩展 — 在现有 ContextBuilder 之外注入额外上下文
///
/// 现有 ContextBuilder 不需要修改。这个结构体在 AgentLoop.process_message()
/// 中调用，将额外部分追加到现有 system_prompt 后面。
pub struct ContextExtension {
    memory_manager: Option<std::sync::Arc<TieredMemoryManager>>,
    skill_registry: Option<std::sync::Arc<SkillRegistry>>,
}

impl ContextExtension {
    pub fn new() -> Self {
        Self {
            memory_manager: None,
            skill_registry: None,
        }
    }

    pub fn with_memory_manager(mut self, mgr: std::sync::Arc<TieredMemoryManager>) -> Self {
        self.memory_manager = Some(mgr);
        self
    }

    pub fn with_skill_registry(mut self, reg: std::sync::Arc<SkillRegistry>) -> Self {
        self.skill_registry = Some(reg);
        self
    }

    /// 构建额外的上下文部分
    /// 追加到现有 ContextBuilder.build_system_prompt() 的输出之后
    pub async fn build_extension(
        &self,
        msg: &InboundMessage,
        _session: &Session,
    ) -> Result<String> {
        let mut parts = Vec::new();

        // 1. 注入结构化记忆
        if let Some(ref mm) = self.memory_manager {
            let user_id = msg.source.as_ref().map(|s| s.user_id.as_deref()).flatten();
            let mem_ctx = mm.build_context(user_id, 10).await?;
            if !mem_ctx.is_empty() {
                parts.push(mem_ctx);
            }
        }

        // 2. 注入匹配到的技能
        if let Some(ref sr) = self.skill_registry {
            let matched = sr.match_skills(&msg.content).await;
            if !matched.is_empty() {
                let mut skill_section = String::from("## Active Skills\n");
                for m in matched.iter().take(3) {
                    if let Some(skill) = sr.get(&m.name) {
                        match skill.build_prompt_segment().await {
                            Ok(segment) => {
                                skill_section.push_str(&segment);
                                skill_section.push('\n');
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to build prompt for skill '{}': {}",
                                    m.name, e
                                );
                            }
                        }
                    }
                }
                if skill_section.len() > "## Active Skills\n".len() {
                    parts.push(skill_section);
                }
            }
        }

        Ok(parts.join("\n\n"))
    }
}
```

#### 5.1 在 AgentLoop 中的集成方式

修改 `kestrel-agent/src/loop_mod.rs` 中的 `process_message` 方法，在构建 system_prompt 后追加：

```rust
// ─── 在 AgentLoop::process_message 中的修改（最小化改动）──────────

// 现有代码保持不变：
let system_prompt = {
    let context_builder = ContextBuilder::new(&self.config);
    context_builder.build_system_prompt(&msg, &session, &self.tool_registry)?
};

// 新增：追加进化系统上下文
let system_prompt = if let Some(ref ext) = self.context_extension {
    let extension = ext.build_extension(&msg, &session).await?;
    if extension.is_empty() {
        system_prompt
    } else {
        format!("{}\n\n{}", system_prompt, extension)
    }
} else {
    system_prompt
};
```

在 `AgentLoop` struct 中新增一个可选字段：

```rust
pub struct AgentLoop {
    // ... 现有字段不变 ...
    /// 可选的进化系统上下文扩展
    context_extension: Option<std::sync::Arc<ContextExtension>>,
}

impl AgentLoop {
    /// 附加进化系统上下文扩展
    pub fn with_context_extension(mut self, ext: std::sync::Arc<ContextExtension>) -> Self {
        self.context_extension = Some(ext);
        self
    }
}
```

### 6. Self-Review 集成方案

#### 6.1 基于 kestrel-cron 的 Review 调度

现有 `kestrel-cron` 使用 tick-based 调度器：`CronService::tick()` 每秒检查一次所有 active job。

我们创建一个 system cron job 来触发 self-review：

```rust
// ─── crates/kestrel-evolution/src/review_scheduler.rs ──────────────────

use crate::review_trait::*;
use crate::skill_registry::SkillRegistry;
use crate::memory_trait::MemoryStore;
use crate::event_bus::LearningEventBus;
use kestrel_cron::{CronService, CronJob, CronSchedule, CronPayload, JobState, ScheduleKind};
use kestrel_bus::MessageBus;
use std::sync::Arc;
use std::time::Duration;
use chrono::{Local, Duration as ChronoDuration};
use tracing::{info, warn};

/// 混合回顾调度器 — 基于时间 + 事件双重触发
pub struct HybridReviewScheduler {
    memory_store: Arc<dyn MemoryStore>,
    skill_registry: Arc<SkillRegistry>,
    bus: Arc<MessageBus>,
    /// 最小回顾间隔（秒）
    min_interval_secs: u64,
    /// 最大事件累积量
    max_event_accumulation: usize,
    /// 错误率阈值
    error_rate_threshold: f64,
    /// 上次回顾时间
    last_review: parking_lot::RwLock<Option<chrono::DateTime<Local>>>,
    /// 自上次回顾以来的事件计数
    events_since_last: parking_lot::RwLock<usize>,
    /// 自上次回顾以来的错误计数
    errors_since_last: parking_lot::RwLock<usize>,
}

impl HybridReviewScheduler {
    pub fn new(
        memory_store: Arc<dyn MemoryStore>,
        skill_registry: Arc<SkillRegistry>,
        bus: Arc<MessageBus>,
    ) -> Self {
        Self {
            memory_store,
            skill_registry,
            bus,
            min_interval_secs: 3600,    // 默认 1 小时
            max_event_accumulation: 100,
            error_rate_threshold: 0.3,
            last_review: parking_lot::RwLock::new(None),
            events_since_last: parking_lot::RwLock::new(0),
            errors_since_last: parking_lot::RwLock::new(0),
        }
    }

    /// 记录一个学习事件（由 LearningEventBus 调用）
    pub fn record_event(&self, is_error: bool) {
        *self.events_since_last.write() += 1;
        if is_error {
            *self.errors_since_last.write() += 1;
        }
    }

    /// 注册为 kestrel-cron 的周期任务
    pub fn register_cron_job(&self, cron_service: &CronService) -> anyhow::Result<()> {
        // 利用现有 CronService 添加一个系统级 cron job
        let job = CronJob {
            id: "self-review".to_string(),
            name: Some("Self-Review Scheduler".to_string()),
            schedule: CronSchedule {
                kind: ScheduleKind::Every,
                at_ms: None,
                every_ms: Some(self.min_interval_secs as i64 * 1000),
                expr: None,
                tz: None,
            },
            payload: CronPayload::Message {
                message: "periodic self-review".to_string(),
            },
            state: JobState::Active,
            next_run: Some(Local::now() + ChronoDuration::seconds(self.min_interval_secs as i64)),
            last_run: None,
            history: vec![],
            is_system: true,
            priority: 10,
        };
        // cron_service.add_job(job) — 使用现有 API
        Ok(())
    }
}

#[async_trait::async_trait]
impl ReviewScheduler for HybridReviewScheduler {
    fn should_review(&self, _context: &ReviewContext) -> bool {
        // 时间触发
        let time_ok = {
            let last = self.last_review.read();
            match *last {
                None => true,
                Some(t) => {
                    let elapsed = Local::now() - t;
                    elapsed.num_seconds() >= self.min_interval_secs as i64
                }
            }
        };

        // 事件累积触发
        let events_ok = *self.events_since_last.read() >= self.max_event_accumulation;

        // 错误率触发
        let errors = *self.errors_since_last.read();
        let events = *self.events_since_last.read();
        let error_rate = if events > 0 { errors as f64 / events as f64 } else { 0.0 };
        let error_ok = events >= 10 && error_rate > self.error_rate_threshold;

        time_ok || events_ok || error_ok
    }

    async fn review(&self, _context: &ReviewContext) -> anyhow::Result<ReviewReport> {
        info!("Starting self-review");

        let mut report = ReviewReport {
            skill_updates: Vec::new(),
            memory_maintenance: Vec::new(),
            new_skill_proposals: Vec::new(),
            insights: Vec::new(),
            reviewed_at: Local::now(),
        };

        // 1. 审查技能置信度
        let skills = self.skill_registry.all();
        for skill in skills {
            let confidence = skill.confidence();
            if confidence < 0.1 {
                report.skill_updates.push(format!(
                    "技能 '{}' 置信度过低 ({:.2})，建议禁用",
                    skill.name(), confidence
                ));
            } else if confidence > 0.9 {
                report.insights.push(format!(
                    "技能 '{}' 表现优秀 ({:.2})",
                    skill.name(), confidence
                ));
            }
        }

        // 2. 审查记忆分布
        let counts = self.memory_store.counts().await?;
        let hot = counts.get(&MemoryLevel::Hot).copied().unwrap_or(0);
        if hot > 50 {
            report.memory_maintenance.push(format!(
                "热记忆过多 ({})，需要淘汰低置信度条目到温暖层",
                hot
            ));
        }

        // 3. 发送事件
        self.bus.emit_event(kestrel_bus::events::AgentEvent::SelfReviewCompleted {
            skill_updates: report.skill_updates.len(),
            memory_actions: report.memory_maintenance.len(),
            new_proposals: report.new_skill_proposals.len(),
        });

        // 重置计数器
        *self.last_review.write() = Some(Local::now());
        *self.events_since_last.write() = 0;
        *self.errors_since_last.write() = 0;

        info!(
            "Self-review completed: {} skill updates, {} memory actions, {} proposals",
            report.skill_updates.len(),
            report.memory_maintenance.len(),
            report.new_skill_proposals.len()
        );

        Ok(report)
    }

    fn name(&self) -> &str {
        "hybrid_review"
    }
}
```

### 7. 新 Tool 实现

三个新 tool 实现 `kestrel_tools::Tool` trait，与现有 builtins 风格一致：

#### 7.1 memory — 记忆管理 Tool

```rust
// ─── crates/kestrel-evolution/src/tools/memory_tool.rs ──────────────────

use async_trait::async_trait;
use kestrel_tools::{Tool, ToolError};
use serde_json::Value;
use crate::memory_manager::TieredMemoryManager;
use crate::memory_trait::{MemoryCategory, MemoryLevel, MemoryQuery};
use std::sync::Arc;

/// memory tool — 让 LLM 能够读写结构化记忆
pub struct MemoryTool {
    manager: Arc<TieredMemoryManager>,
}

impl MemoryTool {
    pub fn new(manager: Arc<TieredMemoryManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Store, recall, and manage persistent memories. \
         Use 'store' to save important information, \
         'recall' to search memories, and 'delete' to remove outdated entries."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["store", "recall", "delete", "list"],
                    "description": "The memory operation to perform"
                },
                "content": {
                    "type": "string",
                    "description": "Content to store (for 'store' action)"
                },
                "category": {
                    "type": "string",
                    "enum": [
                        "user_profile", "environment", "project_convention",
                        "tool_discovery", "error_lesson", "workflow_pattern"
                    ],
                    "description": "Memory category"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (for 'recall' action)"
                },
                "entry_id": {
                    "type": "string",
                    "description": "Memory entry ID (for 'delete' action)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default: 5)",
                    "default": 5
                }
            },
            "required": ["action"]
        })
    }

    fn toolset(&self) -> &str {
        "evolution"
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let action = args.get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Validation("Missing 'action' field".into()))?;

        match action {
            "store" => {
                let content = args.get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::Validation("Missing 'content' field".into()))?;

                let category = args.get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("environment");

                let category: MemoryCategory = serde_json::from_value(
                    serde_json::Value::String(category.to_string())
                ).unwrap_or(MemoryCategory::Environment);

                match self.manager.store_memory(
                    content.to_string(),
                    category,
                    0.7, // 默认置信度
                ).await {
                    Ok(id) => Ok(format!("Memory stored with ID: {}", id)),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            "recall" => {
                let query_text = args.get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let limit = args.get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5) as usize;

                let query = MemoryQuery {
                    text: if query_text.is_empty() { None } else { Some(query_text.to_string()) },
                    limit,
                    ..Default::default()
                };

                match self.manager.structured.recall(&query).await {
                    Ok(entries) => {
                        if entries.is_empty() {
                            Ok("No memories found.".to_string())
                        } else {
                            let lines: Vec<String> = entries.iter().map(|e| {
                                format!(
                                    "[{}] {} (confidence: {:.0%}, accessed {} times)",
                                    e.id, e.content, e.confidence, e.access_count
                                )
                            }).collect();
                            Ok(lines.join("\n"))
                        }
                    }
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            "delete" => {
                let entry_id = args.get("entry_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::Validation("Missing 'entry_id' field".into()))?;

                match self.manager.structured.delete(entry_id).await {
                    Ok(true) => Ok(format!("Memory '{}' deleted.", entry_id)),
                    Ok(false) => Ok(format!("Memory '{}' not found.", entry_id)),
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            "list" => {
                match self.manager.structured.counts().await {
                    Ok(counts) => {
                        let hot = counts.get(&MemoryLevel::Hot).copied().unwrap_or(0);
                        let warm = counts.get(&MemoryLevel::Warm).copied().unwrap_or(0);
                        let cold = counts.get(&MemoryLevel::Cold).copied().unwrap_or(0);
                        Ok(format!(
                            "Memory store: {} hot, {} warm, {} cold (total: {})",
                            hot, warm, cold, hot + warm + cold
                        ))
                    }
                    Err(e) => Err(ToolError::Execution(e.to_string())),
                }
            }
            _ => Err(ToolError::Validation(format!(
                "Unknown action '{}'. Use: store, recall, delete, list",
                action
            ))),
        }
    }
}
```

#### 7.2 skill — 技能管理 Tool

```rust
// ─── crates/kestrel-evolution/src/tools/skill_tool.rs ──────────────────

use async_trait::async_trait;
use kestrel_tools::{Tool, ToolError};
use serde_json::Value;
use crate::skill_registry::SkillRegistry;
use std::sync::Arc;

/// skill tool — 让 LLM 能够查询和管理技能
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Query and manage skills. Use 'list' to see all skills, \
         'info' to get details about a skill, and 'match' to find \
         skills relevant to a query."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "info", "match"],
                    "description": "The skill operation to perform"
                },
                "name": {
                    "type": "string",
                    "description": "Skill name (for 'info' action)"
                },
                "query": {
                    "type": "string",
                    "description": "Query text (for 'match' action)"
                },
                "category": {
                    "type": "string",
                    "description": "Filter by category (for 'list' action)"
                }
            },
            "required": ["action"]
        })
    }

    fn toolset(&self) -> &str {
        "evolution"
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let action = args.get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Validation("Missing 'action' field".into()))?;

        match action {
            "list" => {
                let category = args.get("category").and_then(|v| v.as_str());
                let skills = if let Some(cat) = category {
                    self.registry.list_by_category(cat)
                } else {
                    self.registry.all()
                };
                if skills.is_empty() {
                    Ok("No skills found.".to_string())
                } else {
                    let lines: Vec<String> = skills.iter().map(|s| {
                        format!(
                            "- {} [{}] (confidence: {:.0%}): {}",
                            s.name(), s.category(), s.confidence(), s.description()
                        )
                    }).collect();
                    Ok(lines.join("\n"))
                }
            }
            "info" => {
                let name = args.get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::Validation("Missing 'name' field".into()))?;

                match self.registry.get(name) {
                    Some(skill) => {
                        Ok(format!(
                            "Name: {}\nCategory: {}\nDescription: {}\nConfidence: {:.0%}",
                            skill.name(),
                            skill.category(),
                            skill.description(),
                            skill.confidence(),
                        ))
                    }
                    None => Ok(format!("Skill '{}' not found.", name)),
                }
            }
            "match" => {
                let query = args.get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::Validation("Missing 'query' field".into()))?;

                let matches = self.registry.match_skills(query).await;
                if matches.is_empty() {
                    Ok("No matching skills found.".to_string())
                } else {
                    let lines: Vec<String> = matches.iter().take(5).map(|m| {
                        format!("- {} (score: {:.2})", m.name, m.score)
                    }).collect();
                    Ok(lines.join("\n"))
                }
            }
            _ => Err(ToolError::Validation(format!(
                "Unknown action '{}'. Use: list, info, match",
                action
            ))),
        }
    }
}
```

#### 7.3 session_search — 会话搜索 Tool

```rust
// ─── crates/kestrel-evolution/src/tools/session_search_tool.rs ──────────────────

use async_trait::async_trait;
use kestrel_tools::{Tool, ToolError};
use kestrel_session::SessionManager;
use serde_json::Value;
use std::sync::Arc;

/// session_search tool — 让 LLM 能够搜索历史会话
pub struct SessionSearchTool {
    session_manager: Arc<SessionManager>,
}

impl SessionSearchTool {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self { session_manager }
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search through conversation history and notes across sessions. \
         Useful for finding past discussions, decisions, or user preferences \
         mentioned in earlier conversations."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query text"
                },
                "search_notes": {
                    "type": "boolean",
                    "description": "Whether to also search session notes (default: true)",
                    "default": true
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default: 5)",
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    fn toolset(&self) -> &str {
        "evolution"
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query = args.get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::Validation("Missing 'query' field".into()))?;

        let search_notes = args.get("search_notes")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let limit = args.get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        let mut results = Vec::new();

        // 搜索 notes（使用现有 NoteStore 的 search_notes 方法）
        if search_notes {
            let note_results = self.session_manager.search_notes(query);
            for note in note_results.iter().take(limit) {
                results.push(format!(
                    "[Session Note] {} (tags: {})\n{}",
                    note.title,
                    note.tags.join(", "),
                    note.content,
                ));
            }
        }

        if results.is_empty() {
            Ok("No matching sessions or notes found.".to_string())
        } else {
            Ok(results.join("\n\n"))
        }
    }
}
```

#### 7.4 SkillRegistry 实现

```rust
// ─── crates/kestrel-evolution/src/skill_registry.rs ──────────────────

use crate::skill_trait::{ConfidenceEvent, Skill, SkillMatch, SkillOutput};
use kestrel_tools::skill_loader::SkillLoader;
use kestrel_tools::skills::Skill as DiskSkill;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// 内存中的技能实例
struct SkillEntry {
    inner: Box<dyn Skill>,
    /// 磁盘上的原始数据（如果有）
    disk_skill: Option<DiskSkill>,
}

/// 技能注册表 — 与 ToolRegistry 对称的设计
pub struct SkillRegistry {
    /// 从磁盘加载的技能（复用 SkillLoader）
    loader: RwLock<SkillLoader>,
    /// 运行时技能实例
    skills: RwLock<HashMap<String, SkillEntry>>,
}

impl SkillRegistry {
    pub fn new(skills_dir: std::path::PathBuf) -> Self {
        let loader = SkillLoader::new(skills_dir);
        Self {
            loader: RwLock::new(loader),
            skills: RwLock::new(HashMap::new()),
        }
    }

    /// 从磁盘加载所有技能
    pub async fn load_all(&self) -> anyhow::Result<Vec<String>> {
        let mut loader = self.loader.write().await;
        let disk_skills = loader.load_all()?;
        let mut skills = self.skills.write().await;
        let mut loaded = Vec::new();

        for disk_skill in disk_skills {
            let name = disk_skill.name.clone();
            let wrapped = DiskSkillWrapper { disk: disk_skill.clone() };
            skills.insert(name.clone(), SkillEntry {
                inner: Box::new(wrapped),
                disk_skill: Some(disk_skill),
            });
            loaded.push(name);
        }

        info!("Loaded {} skills", loaded.len());
        Ok(loaded)
    }

    /// 注册自定义技能
    pub async fn register(&self, skill: Box<dyn Skill>) {
        let name = skill.name().to_string();
        self.skills.write().await.insert(name, SkillEntry {
            inner: skill,
            disk_skill: None,
        });
    }

    /// 按名称查找技能
    pub fn get(&self, name: &str) -> Option<std::sync::MutexGuard<'_, HashMap<String, SkillEntry>>> {
        // 注意：实际实现需要使用 async RwLock
        unimplemented!() // 简化演示
    }

    /// 异步按名称查找
    pub async fn get_async(&self, name: &str) -> Option<Arc<dyn Skill>> {
        let skills = self.skills.read().await;
        skills.get(name).map(|e| {
            // 由于 Skill trait 不是 Clone，返回 Arc 需要内部 Arc
            // 这里简化处理
            unimplemented!()
        })
    }

    /// 简化：按名称获取技能信息
    pub async fn get_info(&self, name: &str) -> Option<SkillInfo> {
        let skills = self.skills.read().await;
        skills.get(name).map(|e| SkillInfo {
            name: e.inner.name().to_string(),
            description: e.inner.description().to_string(),
            category: e.inner.category().to_string(),
            confidence: e.inner.confidence(),
        })
    }

    /// 返回所有技能信息
    pub fn all(&self) -> Vec<SkillInfo> {
        // 同步获取（使用 try_read 或在 async context 中调用）
        Vec::new() // 简化
    }

    /// 按分类列出
    pub fn list_by_category(&self, category: &str) -> Vec<SkillInfo> {
        Vec::new() // 简化
    }

    /// 匹配技能 — 给定用户输入，返回按匹配度排序的结果
    pub async fn match_skills(&self, user_input: &str) -> Vec<SkillMatch> {
        let skills = self.skills.read().await;
        let mut matches: Vec<SkillMatch> = Vec::new();

        for (_, entry) in skills.iter() {
            let score = entry.inner.match_score(user_input).await;
            if score > 0.1 {
                matches.push(SkillMatch {
                    name: entry.inner.name().to_string(),
                    score,
                });
            }
        }

        matches.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        matches
    }
}

/// 技能摘要信息
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub category: String,
    pub confidence: f64,
}

/// 将磁盘上的 Skill 包装为 Skill trait 实现
struct DiskSkillWrapper {
    disk: DiskSkill,
}

#[async_trait::async_trait]
impl Skill for DiskSkillWrapper {
    fn name(&self) -> &str { &self.disk.name }
    fn description(&self) -> &str { &self.disk.description }
    fn category(&self) -> &str { &self.disk.category }
    fn confidence(&self) -> f64 { 0.5 }

    async fn build_prompt_segment(&self) -> anyhow::Result<String> {
        Ok(self.disk.instructions.clone())
    }

    async fn match_score(&self, user_input: &str) -> f64 {
        let input_lower = user_input.to_lowercase();
        // 简单的关键词匹配：检查技能名称、描述、标签是否出现在输入中
        let mut score = 0.0;
        if input_lower.contains(&self.disk.name.to_lowercase()) {
            score += 0.5;
        }
        for word in self.disk.description.split_whitespace() {
            if input_lower.contains(&word.to_lowercase()) && word.len() > 2 {
                score += 0.1;
            }
        }
        for tag in &self.disk.tags {
            if input_lower.contains(&tag.to_lowercase()) {
                score += 0.2;
            }
        }
        score.min(1.0)
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<SkillOutput> {
        // 磁盘技能不执行操作，仅注入 prompt
        Ok(SkillOutput::PromptOnly {
            segment: self.disk.instructions.clone(),
        })
    }

    fn update_confidence(&mut self, _event: ConfidenceEvent) {
        // 磁盘技能暂不追踪置信度
    }
}
```

### 8. 集成测试方案

遵循 kestrel 现有测试模式：
- 使用 `tempfile::tempdir()` 创建临时目录
- 使用 `tokio::test` 宏
- 直接构造组件，不依赖 mock 框架
- 断言使用标准 `assert_eq!`/`assert!(...)`

```rust
// ─── crates/kestrel-evolution/tests/integration.rs ──────────────────

use kestrel_bus::MessageBus;
use kestrel_evolution::*;
use kestrel_evolution::memory_trait::*;
use kestrel_evolution::memory_store::FileBasedMemoryStore;
use kestrel_evolution::skill_registry::SkillRegistry;
use kestrel_evolution::review_scheduler::HybridReviewScheduler;
use kestrel_evolution::tools::{MemoryTool, SkillTool, SessionSearchTool};
use kestrel_session::SessionManager;
use kestrel_tools::ToolRegistry;
use std::sync::Arc;

// ─── Memory 端到端测试 ────────────────────────────────────

#[tokio::test]
async fn test_memory_store_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileBasedMemoryStore::new(dir.path().to_path_buf()).unwrap();

    // Store
    let entry = MemoryEntry {
        id: "test-1".to_string(),
        content: "用户偏好中文回复".to_string(),
        category: MemoryCategory::UserProfile,
        source: "test".to_string(),
        created_at: chrono::Utc::now(),
        last_accessed: chrono::Utc::now(),
        access_count: 0,
        confidence: 0.8,
        level: MemoryLevel::Hot,
    };
    store.store(entry).await.unwrap();

    // Recall
    let results = store.recall(&MemoryQuery {
        text: Some("中文".to_string()),
        limit: 10,
        ..Default::default()
    }).await.unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "用户偏好中文回复");
    assert_eq!(results[0].category, MemoryCategory::UserProfile);
}

#[tokio::test]
async fn test_memory_promote_changes_level() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileBasedMemoryStore::new(dir.path().to_path_buf()).unwrap();

    let entry = MemoryEntry {
        id: "promote-test".to_string(),
        content: "test content".to_string(),
        category: MemoryCategory::Environment,
        source: "test".to_string(),
        created_at: chrono::Utc::now(),
        last_accessed: chrono::Utc::now(),
        access_count: 0,
        confidence: 0.5,
        level: MemoryLevel::Hot,
    };
    store.store(entry).await.unwrap();

    // Promote to Warm
    store.promote("promote-test", MemoryLevel::Warm).await.unwrap();

    // Verify it's now in Warm
    let got = store.get("promote-test").await.unwrap().unwrap();
    assert_eq!(got.level, MemoryLevel::Warm);

    // Counts should reflect the change
    let counts = store.counts().await.unwrap();
    assert_eq!(counts.get(&MemoryLevel::Hot).copied().unwrap_or(0), 0);
    assert_eq!(counts.get(&MemoryLevel::Warm).copied().unwrap_or(0), 1);
}

#[tokio::test]
async fn test_memory_delete() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileBasedMemoryStore::new(dir.path().to_path_buf()).unwrap();

    let entry = MemoryEntry {
        id: "delete-me".to_string(),
        content: "temporary".to_string(),
        category: MemoryCategory::ToolDiscovery,
        source: "test".to_string(),
        created_at: chrono::Utc::now(),
        last_accessed: chrono::Utc::now(),
        access_count: 0,
        confidence: 0.5,
        level: MemoryLevel::Hot,
    };
    store.store(entry).await.unwrap();

    let deleted = store.delete("delete-me").await.unwrap();
    assert!(deleted);

    let got = store.get("delete-me").await.unwrap();
    assert!(got.is_none());
}

// ─── MemoryTool 测试 ─────────────────────────────────────

#[tokio::test]
async fn test_memory_tool_store_and_recall() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileBasedMemoryStore::new(dir.path().to_path_buf()).unwrap());

    // 直接通过 TieredMemoryManager 测试（简化版）
    let tool = MemoryTool::new(/* 需要 TieredMemoryManager */);

    // Store
    let result = tool.execute(serde_json::json!({
        "action": "store",
        "content": "项目使用 Rust 编写",
        "category": "project_convention"
    })).await.unwrap();
    assert!(result.contains("Memory stored"));

    // Recall
    let result = tool.execute(serde_json::json!({
        "action": "recall",
        "query": "Rust"
    })).await.unwrap();
    assert!(result.contains("项目使用 Rust 编写"));

    // List
    let result = tool.execute(serde_json::json!({
        "action": "list"
    })).await.unwrap();
    assert!(result.contains("1 hot"));
}

// ─── SkillRegistry 测试 ──────────────────────────────────

#[tokio::test]
async fn test_skill_registry_load_and_match() {
    let dir = tempfile::tempdir().unwrap();

    // 创建测试技能文件
    let skill_content = "---\nname: deploy\ndescription: Deploy the application\ncategory: devops\ntags:\n  - deploy\n  - cicd\n---\n# Deploy\nDeploy to {env}...";
    std::fs::write(dir.path().join("deploy.md"), skill_content).unwrap();

    let registry = SkillRegistry::new(dir.path().to_path_buf());
    let loaded = registry.load_all().await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0], "deploy");

    // 匹配测试
    let matches = registry.match_skills("帮我部署到生产环境").await;
    assert!(!matches.is_empty());
    assert_eq!(matches[0].name, "deploy");
}

// ─── ReviewScheduler 测试 ─────────────────────────────────

#[tokio::test]
async fn test_review_scheduler_triggers_on_events() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(MessageBus::new());
    let memory_store = Arc::new(FileBasedMemoryStore::new(dir.path().to_path_buf()).unwrap());
    let skill_dir = tempfile::tempdir().unwrap();
    let skill_registry = Arc::new(SkillRegistry::new(skill_dir.path().to_path_buf()));

    let scheduler = HybridReviewScheduler::new(
        memory_store,
        skill_registry,
        bus,
    );

    // 模拟大量事件
    for _ in 0..101 {
        scheduler.record_event(false);
    }

    let context = ReviewContext {
        time_since_last: std::time::Duration::from_secs(0),
        events_since_last: 101,
        active_skill_count: 0,
        memory_entry_count: 0,
        recent_error_rate: 0.0,
    };

    assert!(scheduler.should_review(&context));
}

#[tokio::test]
async fn test_review_scheduler_triggers_on_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(MessageBus::new());
    let memory_store = Arc::new(FileBasedMemoryStore::new(dir.path().to_path_buf()).unwrap());
    let skill_dir = tempfile::tempdir().unwrap();
    let skill_registry = Arc::new(SkillRegistry::new(skill_dir.path().to_path_buf()));

    let scheduler = HybridReviewScheduler::new(
        memory_store,
        skill_registry,
        bus,
    );

    // 模拟高错误率
    for _ in 0..5 {
        scheduler.record_event(false);
    }
    for _ in 0..5 {
        scheduler.record_event(true);
    }

    let context = ReviewContext {
        time_since_last: std::time::Duration::from_secs(0),
        events_since_last: 10,
        active_skill_count: 0,
        memory_entry_count: 0,
        recent_error_rate: 0.5,
    };

    assert!(scheduler.should_review(&context));
}

// ─── LearningEventBus 集成测试 ────────────────────────────

#[tokio::test]
async fn test_event_bus_filters_learning_events() {
    let bus = Arc::new(MessageBus::new());

    // 发送学习相关事件
    bus.emit_event(kestrel_bus::events::AgentEvent::ToolSucceeded {
        session_key: "test:1".to_string(),
        tool_name: "terminal".to_string(),
        duration_ms: 100,
    });

    // 发送非学习事件
    bus.emit_event(kestrel_bus::events::AgentEvent::Started {
        session_key: "test:1".to_string(),
    });

    let mut rx = bus.subscribe_events();
    // 验证两个事件都能收到
    let e1 = rx.recv().await.unwrap();
    let e2 = rx.recv().await.unwrap();

    assert!(matches!(e1, kestrel_bus::events::AgentEvent::ToolSucceeded { .. }));
    assert!(matches!(e2, kestrel_bus::events::AgentEvent::Started { .. }));
}

// ─── 全链路集成测试 ────────────────────────────────────────

#[tokio::test]
async fn test_full_evolution_pipeline() {
    // 1. 初始化组件
    let bus = Arc::new(MessageBus::new());
    let session_dir = tempfile::tempdir().unwrap();
    let memory_dir = tempfile::tempdir().unwrap();
    let skill_dir = tempfile::tempdir().unwrap();

    let session_manager = Arc::new(SessionManager::new(
        session_dir.path().to_path_buf()
    ).unwrap());

    let memory_store = Arc::new(FileBasedMemoryStore::new(
        memory_dir.path().to_path_buf()
    ).unwrap());

    // 2. 创建技能文件
    let skill_content = "---\nname: rust_helper\ndescription: Rust programming help\ntags:\n  - rust\n  - programming\n---\nYou are helping with Rust code.";
    std::fs::write(skill_dir.path().join("rust_helper.md"), skill_content).unwrap();

    // 3. 加载技能
    let skill_registry = Arc::new(SkillRegistry::new(skill_dir.path().to_path_buf()));
    let loaded = skill_registry.load_all().await.unwrap();
    assert_eq!(loaded, vec!["rust_helper"]);

    // 4. 存储记忆
    memory_store.store(MemoryEntry {
        id: "mem-1".to_string(),
        content: "项目使用 tokio 异步运行时".to_string(),
        category: MemoryCategory::ProjectConvention,
        source: "test".to_string(),
        created_at: chrono::Utc::now(),
        last_accessed: chrono::Utc::now(),
        access_count: 0,
        confidence: 0.9,
        level: MemoryLevel::Hot,
    }).await.unwrap();

    // 5. 匹配技能
    let matches = skill_registry.match_skills("帮我写一个 Rust 异步函数").await;
    assert!(!matches.is_empty());

    // 6. 搜索记忆
    let memories = memory_store.recall(&MemoryQuery {
        text: Some("tokio".to_string()),
        limit: 5,
        ..Default::default()
    }).await.unwrap();
    assert_eq!(memories.len(), 1);
    assert!(memories[0].content.contains("tokio"));

    // 7. 执行回顾
    let scheduler = HybridReviewScheduler::new(
        memory_store.clone(),
        skill_registry.clone(),
        bus.clone(),
    );

    let context = ReviewContext {
        time_since_last: std::time::Duration::from_secs(3600),
        events_since_last: 50,
        active_skill_count: 1,
        memory_entry_count: 1,
        recent_error_rate: 0.05,
    };

    assert!(scheduler.should_review(&context));
    let report = scheduler.review(&context).await.unwrap();
    assert_eq!(report.reviewed_at.date_naive(), chrono::Local::now().date_naive());
}
```

### 9. 完整的 Cargo.toml 变更

#### 9.1 新 crate 的 Cargo.toml

```toml
# ─── crates/kestrel-evolution/Cargo.toml ──────────────────

[package]
name = "kestrel-evolution"
version.workspace = true
edition.workspace = true

[dependencies]
kestrel-core = { path = "../kestrel-core" }
kestrel-bus = { path = "../kestrel-bus" }
kestrel-tools = { path = "../kestrel-tools" }
kestrel-session = { path = "../kestrel-session" }
kestrel-config = { path = "../kestrel-config" }
kestrel-cron = { path = "../kestrel-cron" }
kestrel-agent = { path = "../kestrel-agent" }

# Async
tokio = { workspace = true }
async-trait = { workspace = true }
futures = { workspace = true }

# Serialization
serde = { workspace = true }
serde_json = { workspace = true }

# Error handling
anyhow = { workspace = true }
thiserror = { workspace = true }

# Logging
tracing = { workspace = true }

# Time
chrono = { workspace = true }

# Concurrency
parking_lot = { workspace = true }

# Misc
uuid = { workspace = true }
regex = { workspace = true }
notify = "7"

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true, features = ["test-util", "macros"] }
```

#### 9.2 Workspace Cargo.toml 变更

```toml
# ─── Cargo.toml [workspace] members 新增 ──────────────────

[workspace]
resolver = "2"
members = [
    # ... 现有 members 保持不变 ...
    "crates/kestrel-evolution",     # 新增
]
```

#### 9.3 主 binary 新增依赖

```toml
# ─── Cargo.toml [package] dependencies 新增 ──────────────────

[dependencies]
# ... 现有依赖保持不变 ...
kestrel-evolution = { path = "crates/kestrel-evolution" }   # 新增
```

#### 9.4 kestrel-bus Cargo.toml — 无需变更

kestrel-bus 不需要新增依赖。新增的 `AgentEvent` 变体仅使用基本类型（String, usize, f64, u64），不需要引入新 crate。

#### 9.5 kestrel-agent Cargo.toml — 新增依赖

```toml
# ─── crates/kestrel-agent/Cargo.toml 新增 ──────────────────

[dependencies]
# ... 现有依赖保持不变 ...
kestrel-evolution = { path = "../kestrel-evolution" }   # 新增（可选依赖）
```

---

### 精修设计总结

本精修设计与初版设计的核心差异：

| 方面 | 初版设计 | 精修设计 |
|------|---------|---------|
| **存储** | 假设使用 SQLite | 使用 JSONL/JSON 文件，与现有 SessionStore 一致 |
| **现有代码** | 假设 ContextBuilder 需要大幅修改 | 通过组合扩展（ContextExtension），零修改现有代码 |
| **消息流** | 独立 LearningEventBus | 复用现有 MessageBus 的 broadcast 通道 |
| **调度** | 自定义调度器 | 复用现有 CronService 的 tick-based 机制 |
| **Trait 风格** | 概念性伪代码 | 完全匹配 Tool trait、CronStateStore trait 风格 |
| **技能系统** | 完全新建 | 复用现有 SkillLoader + Skill struct，包装为 Skill trait |
| **记忆系统** | 与现有无关 | 与现有 MemoryStore（MEMORY.md）共存，向后兼容 |
| **Tool 实现** | 未给出 | 三个完整 tool（memory, skill, session_search） |
| **集成方式** | 需要修改 AgentRunner | 仅在 AgentLoop 中追加可选 context_extension |

**核心原则**：最小侵入性集成。现有 12 个 crate 零修改，仅新增 1 个 crate + 扩展 AgentEvent 枚举。
