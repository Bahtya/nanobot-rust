# nanobot-tools

Tool trait definition, registry, and built-in tool implementations.

Part of the [nanobot-rust](../..) workspace.

## Overview

Provides the `Tool` trait that every agent tool implements, a `ToolRegistry` for
dynamic registration and dispatch, and a set of built-in tools for shell execution,
filesystem operations, web access, search, cron scheduling, messaging, and subagent
spawning.

## Key Types

| Type | Description |
|---|---|
| `Tool` (trait) | `name()`, `description()`, `parameters_schema()`, `execute(args)` |
| `ToolError` | Validation, Execution, Timeout, PermissionDenied, NotAvailable |
| `ToolRegistry` | Thread-safe registry with `register()`, `execute()`, `get_definitions()` |

## Built-in Tools

| Tool | Module | Description |
|---|---|---|
| `ExecTool` | `shell` | Execute shell commands |
| `ReadFileTool`, `WriteFileTool`, `EditFileTool`, `ListDirTool` | `filesystem` | File I/O |
| `WebSearchTool`, `WebFetchTool` | `web` | Web search and URL fetching |
| `GrepTool`, `GlobTool` | `search` | Code and content search |
| `MessageTool` | `message` | Send messages to channels |
| `CronTool` | `cron` | Schedule cron jobs |
| `SpawnTool` | `spawn` | Spawn subagent tasks |

## Usage

```rust
use nanobot_tools::{ToolRegistry, Tool, ToolError};
use nanobot_tools::builtins;

let registry = ToolRegistry::new();
builtins::register_all(&registry);

// Execute a tool by name
let result = registry.execute("exec", serde_json::json!({"command": "ls"})).await?;

// Get OpenAI function definitions for the LLM
let definitions = registry.get_definitions();

// Custom tool
struct MyTool;
#[async_trait::async_trait]
impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }
    fn description(&self) -> &str { "Does something useful" }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"input": {"type": "string"}}})
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        Ok(format!("Result for {:?}", args))
    }
}
```
