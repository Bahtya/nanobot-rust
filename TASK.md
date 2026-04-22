# Task: Fix WarmStore issues #127 and #128

Branch: `fix/warmstore-127-128`
Issues: #127 (LanceDB predicate injection), #128 (race condition)

## #127 LanceDB predicate injection

- File: `crates/kestrel-memory/src/warm_store.rs`
- Problem: `format!("id = '{id}'")` builds predicate by string concatenation
- Fix: Add strict input validation before formatting — only allow `[a-zA-Z0-9_-]` chars in `id`. Or better, use parameterized query if LanceDB supports it. If not, validate and escape.

## #128 WarmStore race condition

- File: `crates/kestrel-memory/src/warm_store.rs`
- Problem: `store()` has no locking; concurrent `append()` to LanceDB may corrupt data
- Fix: Add `tokio::sync::RwLock<()>` or `Mutex<()>` around the `append()` call in `store()`. Must be stored in `WarmStore` struct.

## Rules

1. Do NOT run cargo build/test/clippy locally. Commit + push, let GitHub CI verify.
2. Add tests for both fixes.
3. Comment on issues when starting and when done.
4. Do NOT merge the PR yourself.
