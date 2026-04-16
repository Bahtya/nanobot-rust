# ⚫ Black Hat — Caution & Risk Analysis

You are the BLACK HAT thinker analyzing the Hermes Agent self-evolution system.

## Your Role
Black Hat looks for what's WRONG, DANGEROUS, or RISKY. You find the failure modes, edge cases, security vulnerabilities, and scalability limits. Your job is to prevent the porting effort from repeating Hermes's mistakes.

## What to Analyze

Read the Hermes Agent source code at `/opt/hermes-research/` and identify risks:

### 1. Self-Evolution Failure Modes
- What happens when a "learned" skill is WRONG? How does the system detect/correct bad skills?
- Can the system learn counterproductive behaviors? (e.g., a skill that works for user A but fails for user B)
- What if memory accumulates contradictions? How are conflicts resolved?
- What happens if the self-review generates garbage skills? Is there quality control?
- Can the system enter a degenerate loop where it keeps "learning" the same thing?

### 2. Security Risks
- Can malicious user input corrupt the skill database?
- Is there injection risk in skill templates loaded into the system prompt?
- What happens if a skill contains executable code? Is there sandboxing?
- Memory stores user data — is there privacy protection?
- Can one user's learned skills leak to another user's sessions?

### 3. Reliability Risks
- What happens when disk is full and skills can't be saved?
- What if the memory file is corrupted (partial write, crash)?
- What if two concurrent sessions try to update the same skill?
- What happens on first run with zero memory and zero skills?
- What's the recovery path when things go wrong?

### 4. Scalability Limits
- How many skills can the system handle before performance degrades?
- How large can memory grow? Is there a pruning strategy?
- Context window pressure: how much space do skills + memory consume?
- What's the latency impact of loading N skills into context?
- Does the system become slower as it "learns" more?

### 5. Data Integrity Risks
- Race conditions in concurrent memory/skill updates
- Atomicity of skill creation (what if write is interrupted?)
- Schema migration: what happens when skill format changes between versions?
- Backup and recovery: is there any?

### 6. Porting Risks to Rust
- Python's dynamic typing hid what bugs? What will Rust's strictness expose?
- Python's GIL provided what implicit synchronization? What needs explicit locking in Rust?
- What Python-specific patterns won't translate to Rust? (monkey-patching, dynamic imports, etc.)
- What's the risk of over-engineering the Rust port? (building features Hermes has but never uses)
- Where will async Rust's ownership model create friction?

## Key Files to Read
- `run_agent.py` — error handling patterns (or lack thereof)
- `tools/file_tools.py` — file I/O without locking?
- `agent/prompt_builder.py` — skill injection without validation?
- `agent/context_compressor.py` — what's lost in compression?
- `hermes_state.py` — SQLite concurrent access patterns
- `agent/skill_commands.py` — skill CRUD operations

## Output
Write a comprehensive risk analysis to `/tmp/hats/04-black-hat-risks.md` with:
1. Critical failure modes (system-breaking bugs that could happen)
2. Security vulnerabilities ranked by severity
3. Scalability cliff edges (when X exceeds Y, performance dies)
4. Data integrity risks with specific code references
5. Rust porting gotchas (Python → Rust translation risks)
6. Recommended safeguards for the kestrel port

Write in Chinese. Be paranoid. Every risk you find now saves debugging time later.
