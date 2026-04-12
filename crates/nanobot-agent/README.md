# nanobot-agent

Agent loop, runner, context builder, memory, hooks, skills, and subagent management.

Part of the [nanobot-rust](../..) workspace.

## Overview

The core orchestration crate. `AgentLoop` consumes inbound messages from the bus,
builds a system prompt via `ContextBuilder`, runs the iterative LLM-tool-LLM cycle
via `AgentRunner`, and publishes outbound responses. Supports streaming output,
lifecycle hooks, file-based memory, markdown skill loading, and background subagent
tasks.

## Key Types

| Type | Description |
|---|---|
| `AgentLoop` | Main loop: consumes from bus, orchestrates run, publishes responses |
| `AgentRunner` | Iterative LLM -> tool call -> result -> LLM cycle with streaming |
| `ContextBuilder` | Assembles system prompt from identity, runtime metadata, tools, memory |
| `AgentHook` (trait) | Lifecycle event handler (`on_event`) |
| `CompositeHook` | Fan-out to multiple hooks, tolerates individual failures |
| `MemoryStore` | File-based persistent memory (MEMORY.md + per-user files) |
| `Consolidator` | Archives conversation summaries into the memory store |
| `SkillsLoader` | Loads markdown skill definitions with YAML frontmatter |
| `SubagentManager` | Tracks background tasks with Running/Completed/Failed status |

## AgentRunner Flow

1. Build `CompletionRequest` with system prompt + conversation + tool definitions
2. Send to LLM (streaming or non-streaming)
3. If response contains tool calls: execute concurrently, append results, go to step 2
4. If no tool calls: return `RunResult` with final content and usage
5. Repeat until max iterations reached

## Usage

```rust
use nanobot_agent::{AgentLoop, AgentRunner};
use std::sync::Arc;

// Full agent loop (wired to bus)
let agent_loop = AgentLoop::new(
    config, bus, session_manager, provider_registry, tool_registry,
);
agent_loop.run().await?;

// Or use the runner directly
let runner = AgentRunner::new(
    Arc::new(config), Arc::new(providers), Arc::new(tools),
);
let result = runner.run(system_prompt, messages).await?;
println!("Response: {}", result.content);
```
