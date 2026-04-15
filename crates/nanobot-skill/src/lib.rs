//! # nanobot-skill
//!
//! Skill trait, TOML manifests, registry, compiler, and loader for the nanobot-rs framework.
//!
//! Skills are the primary extension mechanism. Each skill is defined by a TOML manifest
//! and loaded from `~/.nanobot-rs/skills/` (or `NANOBOT_RS_HOME/skills/`).

pub mod compiler;
pub mod config;
pub mod error;
pub mod loader;
pub mod manifest;
pub mod registry;
pub mod skill;

pub use compiler::SkillCompiler;
pub use config::SkillConfig;
pub use error::{SkillError, SkillResult};
pub use loader::SkillLoader;
pub use manifest::SkillManifest;
pub use registry::SkillRegistry;
pub use skill::{CompiledSkill, ConfidenceEvent, Skill, SkillOutput};
