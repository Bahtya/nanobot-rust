# Sprint Plan Based on Six Hats Analysis

## Inputs

- Read `CLAUDE.md`.
- Read existing Six Hats analysis docs in `docs/`, including `01-blue-hat-architecture.md`, `02-white-hat-specification.md`, `03-red-hat-critique.md`, `04-black-hat-risks.md`, `05-yellow-hat-value.md`, `06-green-hat-design.md`, `hat-*.md`, and all `task-*.md` prompts.
- Read the current Rust architecture across all crates, at minimum every crate `src/lib.rs` plus the key execution, persistence, API, tool, memory, skill, cron, heartbeat, provider, and channel files.

## Priority Rubric

- `P0`: must fix before any release; crashes, data loss, or security holes.
- `P1`: should fix soon; reliability, performance, correctness.
- `P2`: nice to have; DX, refactoring, missing features, architecture cleanup.

## Key Findings

The Six Hats docs converge on the same practical direction:

- Blue/White: the right near-term work is not a large new self-evolution port; it is wiring and hardening the existing Rust components.
- Black: the biggest risks are unguarded execution surfaces, unsafe persistence, and fake/incomplete operational safeguards.
- Yellow: highest ROI comes from making current memory/skill/tool infrastructure safe and trustworthy before adding more learning features.
- Red/Green: avoid a Hermes-style god-object migration; keep changes local, typed, and testable.

The current codebase already has strong building blocks, but the release blockers are basic platform safety and reliability, not missing advanced features.

## Prioritized Tasks

### P0

| ID | Task | What to change | Why it matters | Effort | Dependencies |
|---|---|---|---|---|---|
| P0-1 | Lock down the HTTP API surface | Add configurable API authentication to [schema.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-config/src/schema.rs:551), validate it in [validate.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-config/src/validate.rs:149), wire `ApiServer::with_api_key` from [server.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-api/src/server.rs:151), [serve.rs](/opt/kestrel/worktrees/codex-plan/src/commands/serve.rs:52), and [gateway.rs](/opt/kestrel/worktrees/codex-plan/src/commands/gateway.rs:190); change the default bind host in [schema.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-config/src/schema.rs:552) from `0.0.0.0` to loopback or fail startup when exposed without auth; keep `auth_middleware` strict in [server.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-api/src/server.rs:387). | Today the API binds to `0.0.0.0` by default and auth is effectively unreachable from config, so remote callers can hit `/v1/chat/completions` with full tool access. That is a release-blocking security hole. | M | None |
| P0-2 | Sandbox and scope built-in tools | Introduce workspace/path allowlists for filesystem and search tools in [filesystem.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/filesystem.rs:47), [filesystem.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/filesystem.rs:114), [filesystem.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/filesystem.rs:196), and [search.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/search.rs:49); replace the substring blacklist in [shell.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/shell.rs:44) with deny-by-default execution policy and real sandboxing in [shell.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/shell.rs:89); apply `SsrfGuard` from [network.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-security/src/network.rs:29) to [web.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/web.rs:98) and [web.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/web.rs:216); stop registering dangerous tools unconditionally in [mod.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/builtins/mod.rs:14). | Right now the agent can read arbitrary files, write arbitrary files, execute arbitrary shell, recurse the filesystem, and fetch arbitrary URLs with no meaningful policy boundary. Combined with P0-1, this is effectively host compromise and data exfiltration. | L | None |
| P0-3 | Make session persistence crash-safe | Replace full-overwrite writes in [store.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-session/src/store.rs:86) with temp-file plus rename plus fsync/file-locking; either remove or redesign [store.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-session/src/store.rs:118) so `append_entry` cannot create invalid session files; make [manager.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-session/src/manager.rs:98) use the safer write path consistently; add crash-recovery tests around partial writes and concurrent saves. | Session history and notes are core state. The current JSONL overwrite path is not atomic, and append/write semantics can diverge. A crash or overlapping save can silently truncate conversation data. | M | None |

### P1

| ID | Task | What to change | Why it matters | Effort | Dependencies |
|---|---|---|---|---|---|
| P1-1 | Make tool execution deterministic for mutating tools | Add execution lanes or a mutability flag to the tool layer in [trait_def.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/trait_def.rs:28) and [registry.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/registry.rs:42); stop running every tool call concurrently in [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:174) and [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:293); serialize stateful tools such as `write_file`, `edit_file`, `cron`, and `send_message`. | Concurrent tool calls are fine for pure reads, but they are unsafe for filesystem mutation, scheduling, and future memory/skill writes. This is a correctness and reliability bug waiting to happen. | M | P0-2 |
| P1-2 | Stop silently coercing bad tool arguments | Change [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:301) so JSON parse failures surface as tool errors instead of defaulting to `{}`; add tests for malformed tool arguments across common built-ins. | Silent coercion hides model/tool contract failures and can trigger unintended behavior under partial arguments. It also makes debugging much harder. | S | P1-1 |
| P1-3 | Fix session-aware event and streaming telemetry | Pass the real `session_key` into runner event emission instead of the empty placeholder in [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:155), [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:222), and [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:254); simplify re-emission logic in [loop_mod.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/loop_mod.rs:244). | Typing indicators, stream routing, and event consumers cannot reliably associate activity with the correct session today. This breaks observability and multi-session UX. | S | None |
| P1-4 | Fix usage accounting across iterations | Replace the `Option::or` accumulation in [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:128) with additive accounting for prompt/completion/total tokens over the full run. | Current usage reporting undercounts cost and load whenever the run takes more than one iteration. That undermines rate limiting, billing visibility, and debugging. | S | None |
| P1-5 | Wire heartbeat to real checks or stop advertising it as working | Replace the placeholder constructor in [service.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-heartbeat/src/service.rs:80) so it actually registers provider/tool/session/bus checks and receives a bus; update [gateway.rs](/opt/kestrel/worktrees/codex-plan/src/commands/gateway.rs:181) to register checks and call `set_bus`; ensure the health endpoints in [server.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-api/src/server.rs:840) are backed by real state. | The heartbeat service currently starts with zero useful checks and no event wiring. That creates false confidence: operators see a health subsystem that is mostly inert. | M | None |

### P2

| ID | Task | What to change | Why it matters | Effort | Dependencies |
|---|---|---|---|---|---|
| P2-1 | Expand context assembly beyond the current minimal prompt | Extend [context.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/context.rs:54) to include explicit tool guidance, stable memory fences, project/workspace context, and a progressive skill index instead of only dumping matched skill sections. | The Six Hats analysis is right here: the Rust agent already has memory, notes, and skills, but the prompt builder still exposes only a thin slice of that value. | M | P0-2 |
| P2-2 | Consolidate the duplicate skill stacks | Decide whether `kestrel-skill` or the older markdown-based tool-side skill system is canonical, then retire or adapt the duplicate code in [crates/kestrel-tools/src/skill_loader.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/skill_loader.rs:177), [crates/kestrel-tools/src/skills.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-tools/src/skills.rs:104), [crates/kestrel-skill/src/loader.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-skill/src/loader.rs:1), and [crates/kestrel-skill/src/registry.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-skill/src/registry.rs:24). | Two skill implementations with different formats and responsibilities create architectural confusion, duplicate maintenance, and make future learning work riskier. | L | P2-1 |
| P2-3 | Make API streaming actually stream | Rework [server.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-api/src/server.rs:622) so SSE is driven by incremental chunks instead of waiting for `runner.run()` to finish, and connect it to the runner’s real stream path in [runner.rs](/opt/kestrel/worktrees/codex-plan/crates/kestrel-agent/src/runner.rs:199). | Current SSE behavior is OpenAI-shaped but not actually low-latency. This is mostly a UX/perf problem, not a release blocker. | M | P1-3 |
| P2-4 | Close the learning loop beyond logging | Feed `BasicEventProcessor` outputs from [gateway.rs](/opt/kestrel/worktrees/codex-plan/src/commands/gateway.rs:251) back into memory/skill/prompt updates instead of only logging them; decide whether that belongs in `kestrel-learning`, `kestrel-agent`, or a new integration layer. | The learning pipeline exists, but it currently stops at event persistence and debug logs. This is feature work, not immediate release safety work. | L | P2-1, P2-2 |

## Sprint 6 Scope

Sprint 6 should be the release-blocker sprint. Scope:

1. `P0-1` Lock down the HTTP API surface.
2. `P0-2` Sandbox and scope built-in tools.
3. `P0-3` Make session persistence crash-safe.
4. `P1-3` Fix session-aware event and streaming telemetry.
5. `P1-4` Fix usage accounting across iterations.

Why this scope now:

- It removes the current “remote access + arbitrary tool power” failure mode.
- It hardens the only irreplaceable user data path: sessions and notes.
- It cleans up two small but high-signal correctness bugs while the runner surface is already being touched.

Definition of done for Sprint 6:

- API cannot be exposed remotely without explicit auth configuration.
- Dangerous tools are opt-in, scoped, and policy-checked.
- Session writes survive crash/interruption tests.
- Stream and tool events are session-correct.
- Usage totals are additive and tested.

## Sprint 7 Scope

Sprint 7 should focus on reliability and operational truthfulness:

1. `P1-1` Make tool execution deterministic for mutating tools.
2. `P1-2` Stop silently coercing bad tool arguments.
3. `P1-5` Wire heartbeat to real checks or stop advertising it as working.
4. `P2-1` Expand context assembly beyond the current minimal prompt.

Why this scope next:

- Sprint 6 makes the system safe enough to release.
- Sprint 7 makes the runtime behavior trustworthy under load and easier to operate.
- Prompt/context work becomes more valuable after the execution and health surfaces are no longer misleading.

## Backlog

Backlog after Sprint 7:

1. `P2-2` Consolidate the duplicate skill stacks.
2. `P2-3` Make API streaming actually stream.
3. `P2-4` Close the learning loop beyond logging.

## Recommended Order Inside Sprint 6

1. `P0-1` API auth and exposure defaults.
2. `P0-2` Tool sandboxing and scoping.
3. `P0-3` Session persistence hardening.
4. `P1-3` Session-aware event wiring.
5. `P1-4` Usage accounting fix.

This order reduces exposure first, then protects data, then cleans up runner correctness while those files are fresh.
