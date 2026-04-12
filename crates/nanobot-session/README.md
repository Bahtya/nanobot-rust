# nanobot-session

Session management with a concurrent in-memory cache backed by JSONL file persistence.

Part of the [nanobot-rust](../..) workspace.

## Overview

Manages conversation state per session key (e.g. `telegram:chat123`). Uses a `DashMap`
for lock-free concurrent access and writes each session as a JSONL file (one
`SessionEntry` per line). Automatically truncates history when it exceeds the
configured limit, always preserving the initial system message.

## Key Types

| Type | Description |
|---|---|
| `SessionManager` | High-level lifecycle: get/create, save, append, reset, flush |
| `Session` | A conversation with messages, metadata, and optional source info |
| `SessionEntry` | Single message record (role, content, timestamps, tool_call_id) |
| `SessionMetadata` | Turn count, truncation flag, created_at, last_active timestamps |
| `SessionStore` | Low-level JSONL file I/O (load, save, append_entry, delete) |

## SessionManager API

- `new(data_dir)` -- Create with default history limit (200 messages)
- `with_max_history(data_dir, n)` -- Custom truncation threshold
- `get_or_create(key, source)` -- Return cached, load from disk, or create new
- `save_session(&session)` -- Truncate if needed, persist, update cache
- `append_entry(key, &entry)` -- Append a single entry without rewriting the full file
- `reset_session(key)` / `remove_session(key)` -- Clear or delete a session
- `flush_all()` -- Persist all in-memory sessions to disk

## Usage

```rust
use nanobot_session::SessionManager;
use std::path::PathBuf;

let mgr = SessionManager::new(PathBuf::from("./data"))?;

let mut session = mgr.get_or_create("telegram:chat42", None);
session.add_user_message("Hello!".to_string());
session.add_assistant_message("Hi there!".to_string());
mgr.save_session(&session)?;

// Append incrementally without full rewrite
mgr.append_entry("telegram:chat42", &tool_result_entry)?;
```
