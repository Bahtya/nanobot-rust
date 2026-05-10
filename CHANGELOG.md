# Changelog

## [v0.9.12] - 2026-05-10

### Bug Fixes
- fix(tools): add content size limit (10MB) to `write_file` tool — previously an agent could write arbitrarily large files to disk, only the read/edit tools had size limits (Issue #322)

## [v0.9.11] - 2026-05-10

### Bug Fixes
- fix(tools): add depth/entry/size limits to GrepTool and entry limit to GlobTool — recursive grep on large directories no longer hangs or exhausts memory, blocking I/O moved to `spawn_blocking` (Issue #318)

## [v0.9.10] - 2026-05-10

### Bug Fixes
- fix(tools): add file size check (10MB limit) to `read_file` and `edit_file` tools — previously these tools would read the entire file into memory regardless of size, allowing a large file read to OOM the daemon (Issue #318, PR #319)

## [v0.9.9] - 2026-05-10

### Bug Fixes
- fix(tools): add 30s I/O timeout to all filesystem tools (`read_file`, `write_file`, `edit_file`) — operations on slow/unresponsive filesystems (NFS, FUSE) no longer hang indefinitely (Issue #316, PR #317)
- fix(tools): add depth/entry limits to `ListDirTool` and move directory traversal to `spawn_blocking` to avoid blocking the tokio executor (Issue #316, PR #317)

## [v0.9.8] - 2026-05-10

### Bug Fixes
- fix(tools): replace all `Mutex::lock().unwrap()` with poison-resistant `unwrap_or_else(|e| e.into_inner())` in terminal session — poisoned mutex from a panic would crash the entire daemon on next access (Issue #314, PR #315)
- fix(tools): add `alive` flag check to `read_output` timeout polling loop — kill() on a session no longer waits for the full timeout before returning (Issue #314, PR #315)
