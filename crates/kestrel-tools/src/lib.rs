//! # kestrel-tools
//!
//! Tool system with trait definition, registry, built-in tools, and skills.

pub mod builtins;
pub mod registry;
pub mod schema;
pub mod skill_loader;
pub mod skill_view;
pub mod skills;
pub mod trait_def;

pub use registry::ToolRegistry;
pub use skill_loader::{SkillLoader, SkillWatcher, Version, VersionWarning};
pub use skill_view::SkillViewTool;
pub use skills::{Skill, SkillParameter, SkillStore};
pub use trait_def::{SpawnStatus, SubAgentSpawner, Tool, ToolError};
