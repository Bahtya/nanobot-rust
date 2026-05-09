# CLAUDE.md

## CRITICAL RULES

**禁止本地构建和测试**：绝不允许执行 `cargo build`、`cargo test`、`cargo check` 或 `cargo clean`。所有编译和测试验证必须交给 GitHub Actions CI。直接 commit + push，根据 CI 结果修复。

**允许本地 lint 和格式化**：`cargo fmt` 和 `cargo clippy` 可以在本地运行，用于在推送前捕获格式和 lint 问题。
