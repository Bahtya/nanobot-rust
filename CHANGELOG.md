# Changelog

## [v0.9.8] - 2026-05-10

### Bug Fixes
- fix(tools): replace all `Mutex::lock().unwrap()` with poison-resistant `unwrap_or_else(|e| e.into_inner())` in terminal session — poisoned mutex from a panic would crash the entire daemon on next access (Issue #314, PR #315)
- fix(tools): add `alive` flag check to `read_output` timeout polling loop — kill() on a session no longer waits for the full timeout before returning (Issue #314, PR #315)

## [v0.9.7] - 2026-05-10

### Bug Fixes
- fix(tools): add 30s HTTP timeout to WebSearchTool — `search_brave()` and `search_tavily()` used bare `reqwest::Client::new()` without timeout, allowing requests to hang indefinitely until `tool_timeout` aborted the task (Issue #312, PR #313)

## [v0.9.6] - 2026-05-10

### Bug Fixes
