# Hotfix: XML Escaping for Memory Content (PR #120 follow-up)

## Problem
PR #120 introduced `<memory-context>` XML wrapping for recalled memories, but the memory content is not XML-escaped. If a memory entry contains `</memory-context>`, it prematurely closes the tag and breaks isolation — this is a prompt injection vector since `store_conversation_memory()` stores user message excerpts.

## Task

1. Create branch `fix/xml-escaping-memory-context` from main
2. In `crates/kestrel-agent/src/loop_mod.rs`, find the `recall_memories()` function where memory lines are wrapped in `<memory-context>` tags
3. Add XML escaping for `<`, `>`, `&` characters in each memory line before wrapping
4. Add a test that verifies content containing `</memory-context>` is properly escaped
5. Also fix: change `let _ = self.save_to_disk().await;` in `mark_dirty()` (hot_store.rs) to log the error (use `tracing::warn!`)
6. Run `cargo test --workspace` and `cargo clippy --workspace --all-targets --all-features` — all must pass
7. Commit and push
8. Create PR with title `fix(memory): XML-escape memory content in prompt injection protection` and body referencing this as follow-up to PR #120

## Constraints
- Every commit must pass `cargo test --workspace` + `cargo clippy --workspace` = 0 failures, 0 warnings
- Do NOT merge the PR yourself — only create it for review
- JOBS=2 or fewer for cargo build/test
