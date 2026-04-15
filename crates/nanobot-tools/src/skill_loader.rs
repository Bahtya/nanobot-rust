//! Skill loader — filesystem-backed skill discovery with content-hash caching,
//! dependency resolution, version checking, and file-watcher hot-reload.
//!
//! [`SkillLoader`] scans a skills directory for `.md` files, parses YAML
//! frontmatter (`name`, `description`, `category`, `tags`, `version`,
//! `dependencies`), and caches parsed skills by content hash. Subsequent
//! loads skip files whose content hasn't changed, avoiding redundant
//! filesystem reads and YAML parsing.
//!
//! ## File format
//!
//! ```markdown
//! ---
//! name: deploy
//! description: Deploy the application
//! category: devops
//! version: "1.2.0"
//! dependencies:
//!   - build
//!   - test
//! tags:
//!   - deploy
//!   - cicd
//! ---
//!
//! # Deploy instructions
//! Deploy to {env}...
//! ```

use crate::skills::{Skill, SkillParameter};
use anyhow::{Context, Result};
use notify::Watcher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Version type
// ---------------------------------------------------------------------------

/// A lightweight semantic version (major.minor.patch).
///
/// Supports parsing `"1.2.3"`, `"1.2"`, and `"1"` formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Major version component.
    pub major: u32,
    /// Minor version component.
    pub minor: u32,
    /// Patch version component.
    pub patch: u32,
}

impl Version {
    /// Parse a version string like `"1.2.3"`, `"1.2"`, or `"1"`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let parts: Vec<&str> = s.split('.').collect();
        let major = parts.first().and_then(|p| p.parse().ok())?;
        let minor = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
    }
}

// ---------------------------------------------------------------------------
// Version warning
// ---------------------------------------------------------------------------

/// A version-related warning for a loaded skill.
#[derive(Debug, Clone)]
pub struct VersionWarning {
    /// Name of the skill the warning is about.
    pub skill_name: String,
    /// Human-readable warning message.
    pub message: String,
}

impl std::fmt::Display for VersionWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.skill_name, self.message)
    }
}

// ---------------------------------------------------------------------------
// Cache entry
// ---------------------------------------------------------------------------

/// A cached skill together with a hash of its source file content.
struct CacheEntry {
    /// Hash of the file content when it was last parsed.
    content_hash: u64,
    /// The parsed skill.
    skill: Skill,
}

// ---------------------------------------------------------------------------
// SkillWatcher — file system watcher for hot-reload
// ---------------------------------------------------------------------------

/// Watches a directory for file changes using the `notify` crate.
///
/// Buffers file system events and deduplicates them on drain.
pub struct SkillWatcher {
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
}

impl SkillWatcher {
    /// Create a new watcher that recursively watches `root` for changes.
    pub fn new(root: PathBuf) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut watcher =
            notify::recommended_watcher(tx).context("Failed to create file watcher")?;

        watcher
            .watch(&root, notify::RecursiveMode::Recursive)
            .with_context(|| format!("Failed to watch directory: {}", root.display()))?;

        Ok(Self { watcher, rx })
    }

    /// Drain buffered file-change events and return deduplicated paths.
    ///
    /// Only returns paths for `.md` files. Non-`.md` events are filtered out.
    /// Events are drained non-blocking — if no events are pending, returns
    /// an empty vec.
    pub fn changed_paths(&self) -> Vec<PathBuf> {
        let mut paths = HashSet::new();
        while let Ok(result) = self.rx.try_recv() {
            if let Ok(event) = result {
                for path in &event.paths {
                    if path.extension().is_some_and(|ext| ext == "md") {
                        paths.insert(path.clone());
                    }
                }
            }
        }
        paths.into_iter().collect()
    }
}

// ---------------------------------------------------------------------------
// SkillLoader
// ---------------------------------------------------------------------------

/// Loads skill files from a directory with content-hash caching.
///
/// On [`load_all`], each `.md` file is read and hashed. If the hash
/// matches the cached entry the file is skipped entirely (no YAML parse).
/// Only changed or new files incur the full parse cost.
///
/// Supports dependency resolution via topological sort, version checking,
/// and hot-reload via file system watching.
///
/// Call [`invalidate_all`] or [`invalidate_path`] to force re-reads.
pub struct SkillLoader {
    /// Root directory to scan for `.md` skill files.
    root: PathBuf,
    /// Cache keyed by canonical file path.
    cache: HashMap<PathBuf, CacheEntry>,
    /// Parsed skills indexed by name for fast lookup.
    by_name: HashMap<String, Skill>,
    /// Reverse dependency index: skill name → list of skills that depend on it.
    dependents: HashMap<String, Vec<String>>,
    /// Optional file watcher for hot-reload.
    watcher: Option<SkillWatcher>,
}

impl SkillLoader {
    /// Create a new loader pointing at the given root directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: HashMap::new(),
            by_name: HashMap::new(),
            dependents: HashMap::new(),
            watcher: None,
        }
    }

    /// Load all skill files from the root directory (recursive).
    ///
    /// Uses content-hash caching: unchanged files are served from the
    /// in-memory cache without re-reading or re-parsing.
    ///
    /// Returns the full list of loaded skills.
    pub fn load_all(&mut self) -> Result<Vec<Skill>> {
        if !self.root.exists() {
            debug!("Skills root does not exist: {}", self.root.display());
            self.cache.clear();
            self.by_name.clear();
            self.dependents.clear();
            return Ok(Vec::new());
        }

        let files = collect_md_files(&self.root)?;

        // Prune cache entries for files that no longer exist.
        let file_set: Vec<PathBuf> = files
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
        self.cache.retain(|k, _| file_set.contains(k));

        let mut skills = Vec::new();
        let mut new_by_name = HashMap::new();
        let mut seen_names: HashMap<String, PathBuf> = HashMap::new();

        for path in &files {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

            // Read file content (always needed for hashing).
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read {}: {}", path.display(), e);
                    continue;
                }
            };

            let hash = hash_content(&content);

            // Cache hit — reuse existing parsed skill.
            if let Some(entry) = self.cache.get(&canonical) {
                if entry.content_hash == hash {
                    debug!("Cache hit for {}", path.display());
                    if let Some(prev) = seen_names.insert(entry.skill.name.clone(), path.clone()) {
                        warn!(
                            "Duplicate skill name '{}' — file {} shadows {}",
                            entry.skill.name,
                            path.display(),
                            prev.display()
                        );
                    }
                    new_by_name.insert(entry.skill.name.clone(), entry.skill.clone());
                    skills.push(entry.skill.clone());
                    continue;
                }
            }

            // Cache miss or content changed — parse the file.
            let relative = path.strip_prefix(&self.root).unwrap_or(path).to_path_buf();
            match Self::parse_skill(&content, path, &relative) {
                Ok(skill) => {
                    debug!(
                        "Parsed skill '{}' [{}] from {}",
                        skill.name,
                        skill.category,
                        relative.display()
                    );
                    if let Some(prev) = seen_names.insert(skill.name.clone(), path.clone()) {
                        warn!(
                            "Duplicate skill name '{}' — file {} shadows {}",
                            skill.name,
                            path.display(),
                            prev.display()
                        );
                    }
                    new_by_name.insert(skill.name.clone(), skill.clone());
                    self.cache.insert(
                        canonical,
                        CacheEntry {
                            content_hash: hash,
                            skill: skill.clone(),
                        },
                    );
                    skills.push(skill);
                }
                Err(e) => {
                    warn!("Failed to parse skill {}: {}", relative.display(), e);
                }
            }
        }

        self.by_name = new_by_name;
        self.rebuild_dependents();
        info!(
            "Loaded {} skill(s) from {}",
            skills.len(),
            self.root.display()
        );
        Ok(skills)
    }

    /// Load all skills and return them in dependency-safe order.
    ///
    /// Skills with no dependencies come first, then skills that depend on
    /// them, and so on. Circular dependencies are broken with a warning.
    /// Missing dependencies are logged as warnings but don't prevent loading.
    pub fn load_all_ordered(&mut self) -> Result<Vec<Skill>> {
        self.load_all()?;
        let order = self.dependency_order();
        let mut ordered = Vec::with_capacity(order.len());
        for name in &order {
            if let Some(skill) = self.by_name.get(name) {
                ordered.push(skill.clone());
            }
        }
        Ok(ordered)
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name)
    }

    /// Return all loaded skills.
    pub fn all(&self) -> Vec<&Skill> {
        self.by_name.values().collect()
    }

    /// Number of loaded skills.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether any skills are loaded.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Return all unique categories with their skill counts.
    pub fn categories(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for skill in self.by_name.values() {
            *counts.entry(skill.category.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// List skills in a given category.
    pub fn list_by_category(&self, category: &str) -> Vec<&Skill> {
        self.by_name
            .values()
            .filter(|s| s.category == category)
            .collect()
    }

    /// Find skills matching any of the given tags.
    pub fn find_by_tag(&self, tag: &str) -> Vec<&Skill> {
        self.by_name
            .values()
            .filter(|s| s.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Dependency resolution
    // -----------------------------------------------------------------------

    /// Return all skill names in topological (dependency-safe) order.
    ///
    /// Uses Kahn's algorithm. Circular dependencies are detected and the
    /// cycle is broken by including remaining nodes in arbitrary order
    /// (with a warning logged).
    pub fn dependency_order(&self) -> Vec<String> {
        let names: HashSet<&str> = self.by_name.keys().map(|s| s.as_str()).collect();

        // Build adjacency list: dep → dependents, and in-degree count.
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

        for name in &names {
            in_degree.entry(name).or_insert(0);
            adj.entry(name).or_default();
        }

        for (name, skill) in &self.by_name {
            for dep in &skill.dependencies {
                if names.contains(dep.as_str()) {
                    // dep → name (name depends on dep)
                    adj.entry(dep.as_str()).or_default().push(name);
                    *in_degree.entry(name).or_insert(0) += 1;
                } else {
                    warn!("Skill '{}' depends on '{}' which is not loaded", name, dep);
                }
            }
        }

        // Kahn's algorithm.
        let mut zero_degree: Vec<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&name, _)| name)
            .collect();

        // Sort the initial queue for determinism.
        zero_degree.sort();
        let mut queue: VecDeque<&str> = zero_degree.into_iter().collect();

        let mut result = Vec::with_capacity(names.len());

        while let Some(name) = queue.pop_front() {
            result.push(name.to_string());
            if let Some(neighbors) = adj.get(name) {
                let mut ready = Vec::new();
                for &neighbor in neighbors {
                    if let Some(deg) = in_degree.get_mut(neighbor) {
                        *deg -= 1;
                        if *deg == 0 {
                            ready.push(neighbor);
                        }
                    }
                }
                // Sort for deterministic order.
                ready.sort();
                queue.extend(ready);
            }
        }

        // Remaining nodes are in cycles.
        if result.len() < names.len() {
            let remaining: Vec<String> = names
                .iter()
                .filter(|n| !result.contains(&n.to_string()))
                .map(|n| n.to_string())
                .collect();
            warn!("Circular dependency detected among skills: {:?}", remaining);
            result.extend(remaining);
        }

        result
    }

    /// Resolve transitive dependencies for a named skill.
    ///
    /// Returns ordered list of skill names that must be loaded before the
    /// named skill (dependencies of dependencies included). The named skill
    /// itself is NOT included in the result.
    pub fn resolve_dependencies(&self, name: &str) -> Vec<String> {
        let mut visited = HashSet::new();
        let mut result = Vec::new();
        self.resolve_deps_recursive(name, &mut visited, &mut result);
        // Remove the skill itself if it appeared.
        result.retain(|n| n != name);
        result
    }

    fn resolve_deps_recursive(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        result: &mut Vec<String>,
    ) {
        if visited.contains(name) {
            return;
        }
        visited.insert(name.to_string());

        if let Some(skill) = self.by_name.get(name) {
            for dep in &skill.dependencies {
                self.resolve_deps_recursive(dep, visited, result);
            }
        }
        result.push(name.to_string());
    }

    // -----------------------------------------------------------------------
    // Version checking
    // -----------------------------------------------------------------------

    /// Check version-related issues across all loaded skills.
    ///
    /// Returns warnings for:
    /// - Skills with no `version` field (advisory)
    /// - Skills that depend on other skills with no version
    pub fn check_versions(&self) -> Vec<VersionWarning> {
        let mut warnings = Vec::new();

        for skill in self.by_name.values() {
            if skill.version.is_none() {
                warnings.push(VersionWarning {
                    skill_name: skill.name.clone(),
                    message: "No version declared — consider adding a 'version' field".to_string(),
                });
            }

            for dep in &skill.dependencies {
                if let Some(dep_skill) = self.by_name.get(dep) {
                    if dep_skill.version.is_none() {
                        warnings.push(VersionWarning {
                            skill_name: skill.name.clone(),
                            message: format!("Dependency '{}' has no version declared", dep),
                        });
                    }
                }
            }
        }

        warnings
    }

    /// Parse the version string of a skill into a [`Version`] struct.
    ///
    /// Returns `None` if the skill has no version or the version string
    /// cannot be parsed.
    pub fn skill_version(&self, name: &str) -> Option<Version> {
        self.by_name
            .get(name)
            .and_then(|s| s.version.as_deref())
            .and_then(Version::parse)
    }

    // -----------------------------------------------------------------------
    // Cache invalidation
    // -----------------------------------------------------------------------

    /// Invalidate the entire cache, forcing full re-reads on next load.
    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }

    /// Invalidate the cache for a specific file path.
    ///
    /// `path` can be absolute or relative to the skills root.
    pub fn invalidate_path(&mut self, path: &Path) {
        let canonical = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        if let Ok(canonical) = canonical.canonicalize() {
            if let Some(entry) = self.cache.remove(&canonical) {
                self.by_name.remove(&entry.skill.name);
                self.rebuild_dependents();
            }
        }
    }

    /// Invalidate a skill by name.
    ///
    /// Removes the skill from the cache. It will be re-parsed on next
    /// [`load_all`] call.
    pub fn invalidate_by_name(&mut self, name: &str) {
        if let Some(skill) = self.by_name.remove(name) {
            // Find and remove from path cache.
            let canonical = skill
                .source_path
                .canonicalize()
                .unwrap_or(skill.source_path.clone());
            self.cache.remove(&canonical);
            self.rebuild_dependents();
        }
    }

    /// Invalidate all skills in a given category.
    ///
    /// Returns the names of invalidated skills.
    pub fn invalidate_category(&mut self, category: &str) -> Vec<String> {
        let names: Vec<String> = self
            .by_name
            .values()
            .filter(|s| s.category == category)
            .map(|s| s.name.clone())
            .collect();

        for name in &names {
            self.invalidate_by_name(name);
        }
        names
    }

    /// Invalidate skills whose relative path matches a glob pattern.
    ///
    /// Returns the names of invalidated skills.
    pub fn invalidate_pattern(&mut self, pattern: &str) -> Vec<String> {
        let glob = glob::Pattern::new(pattern);
        let names: Vec<String> = match glob {
            Ok(g) => self
                .by_name
                .values()
                .filter(|s| g.matches(&s.relative_path.to_string_lossy()))
                .map(|s| s.name.clone())
                .collect(),
            Err(e) => {
                warn!("Invalid glob pattern '{}': {}", pattern, e);
                return Vec::new();
            }
        };

        for name in &names {
            self.invalidate_by_name(name);
        }
        names
    }

    /// Invalidate a skill and all skills that (transitively) depend on it.
    ///
    /// Returns all invalidated skill names (the named skill plus dependents).
    pub fn invalidate_cascade(&mut self, name: &str) -> Vec<String> {
        let mut to_invalidate = HashSet::new();
        self.collect_dependents_recursive(name, &mut to_invalidate);

        let names: Vec<String> = to_invalidate.into_iter().collect();
        for n in &names {
            self.invalidate_by_name(n);
        }
        names
    }

    fn collect_dependents_recursive(&self, name: &str, result: &mut HashSet<String>) {
        if result.contains(name) {
            return;
        }
        result.insert(name.to_string());
        if let Some(deps) = self.dependents.get(name) {
            for dep in deps {
                self.collect_dependents_recursive(dep, result);
            }
        }
    }

    /// Rebuild the reverse dependency index from current skills.
    fn rebuild_dependents(&mut self) {
        self.dependents.clear();
        for skill in self.by_name.values() {
            for dep in &skill.dependencies {
                self.dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(skill.name.clone());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Hot-reload
    // -----------------------------------------------------------------------

    /// Start watching the skills directory for file changes.
    ///
    /// After calling this, use [`poll_changes`] to detect and apply changes,
    /// or [`reload_changed`] to combine polling with reloading.
    pub fn start_watcher(&mut self) -> Result<()> {
        if self.watcher.is_some() {
            return Ok(()); // Already watching
        }
        let watcher = SkillWatcher::new(self.root.clone())?;
        self.watcher = Some(watcher);
        info!(
            "Started watching {} for skill file changes",
            self.root.display()
        );
        Ok(())
    }

    /// Stop the file watcher if running.
    pub fn stop_watcher(&mut self) {
        self.watcher.take();
        info!("Stopped skill file watcher");
    }

    /// Poll for file changes, reload affected skills, and return reloaded names.
    ///
    /// If a file watcher is active, uses it for efficient change detection.
    /// Otherwise falls back to a full mtime-based scan (like [`reload_changed_fallback`]).
    pub fn reload_changed(&mut self) -> Result<Vec<String>> {
        if let Some(ref watcher) = self.watcher {
            let paths = watcher.changed_paths();
            if paths.is_empty() {
                return Ok(Vec::new());
            }

            for path in &paths {
                self.invalidate_path(path);
            }

            // Reload everything (cache will skip unchanged files).
            self.load_all()?;

            let reloaded: Vec<String> = paths
                .iter()
                .filter_map(|p| {
                    self.by_name
                        .values()
                        .find(|s| {
                            s.source_path == *p
                                || s.source_path.canonicalize().ok() == p.canonicalize().ok()
                        })
                        .map(|s| s.name.clone())
                })
                .collect();

            if !reloaded.is_empty() {
                info!("Hot-reloaded {} skill(s): {:?}", reloaded.len(), reloaded);
            }
            Ok(reloaded)
        } else {
            self.reload_changed_fallback()
        }
    }

    /// Fallback hot-reload using modification time comparison.
    ///
    /// Checks all cached skills for changed mtime and reloads those
    /// whose files have changed on disk. Also discovers newly added files.
    fn reload_changed_fallback(&mut self) -> Result<Vec<String>> {
        let mut reloaded = Vec::new();

        // Check existing skills for mtime changes.
        let changed_paths: Vec<PathBuf> = self
            .cache
            .values()
            .filter_map(|entry| {
                let current_mtime = std::fs::metadata(&entry.skill.source_path)
                    .ok()
                    .and_then(|m| m.modified().ok());
                if current_mtime != entry.skill.modified_at {
                    Some(entry.skill.source_path.clone())
                } else {
                    None
                }
            })
            .collect();

        for path in &changed_paths {
            self.invalidate_path(path);
        }

        // Check for new files.
        if self.root.exists() {
            let files = collect_md_files(&self.root)?;
            let cached_paths: HashSet<PathBuf> = self.cache.keys().cloned().collect();

            let mut new_paths = Vec::new();
            for file in &files {
                let canonical = file.canonicalize().unwrap_or_else(|_| file.clone());
                if !cached_paths.contains(&canonical) {
                    new_paths.push(canonical);
                }
            }

            // Invalidate new paths so they get parsed.
            drop(cached_paths);
            for _ in &new_paths {
                // No need to invalidate — they're not in cache yet.
            }
        }

        if !changed_paths.is_empty() {
            let names_before: HashSet<String> = self.by_name.keys().cloned().collect();
            self.load_all()?;
            let names_after: HashSet<String> = self.by_name.keys().cloned().collect();

            // Names that are new or were reloaded.
            reloaded = names_after.difference(&names_before).cloned().collect();

            // Also include names whose cache was invalidated (changed content).
            for path in &changed_paths {
                if let Some(name) = self
                    .by_name
                    .values()
                    .find(|s| s.source_path.canonicalize().ok() == path.canonicalize().ok())
                    .map(|s| s.name.clone())
                {
                    if !reloaded.contains(&name) {
                        reloaded.push(name);
                    }
                }
            }
        } else {
            // No changes detected, but check for new files.
            let count_before = self.by_name.len();
            self.load_all()?;
            let count_after = self.by_name.len();
            if count_after > count_before {
                reloaded = self.by_name.keys().cloned().collect();
            }
        }

        Ok(reloaded)
    }

    /// Get the root directory path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Number of entries in the disk cache (for diagnostics / testing).
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    /// Whether a file watcher is active.
    pub fn is_watching(&self) -> bool {
        self.watcher.is_some()
    }

    // -----------------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------------

    /// Parse a skill file from its raw content.
    ///
    /// Returns an error if the skill name is empty. Warns (via tracing) for
    /// empty instructions or names with unusual characters.
    fn parse_skill(content: &str, path: &Path, relative: &Path) -> Result<Skill> {
        let (frontmatter, body) = parse_frontmatter(content)?;

        let name = frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            });

        if name.is_empty() {
            anyhow::bail!("Skill name is empty (no frontmatter 'name' and file stem is empty)");
        }

        if name.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
            warn!(
                "Skill name '{}' in {} contains unusual characters — consider using only alphanumeric, underscore, or hyphen",
                name,
                relative.display()
            );
        }

        let instructions = body.trim().to_string();
        if instructions.is_empty() {
            warn!(
                "Skill '{}' in {} has no instructions (empty body after frontmatter)",
                name,
                relative.display()
            );
        }

        let description = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let category = frontmatter
            .get("category")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                relative
                    .iter()
                    .next()
                    .map(|c| c.to_string_lossy().to_string())
                    .filter(|_| relative.iter().count() > 1)
                    .unwrap_or_else(|| "uncategorized".to_string())
            });

        let tags = yaml_string_array(&frontmatter, "tags");
        let parameters = parse_parameters(&frontmatter);
        let version = frontmatter
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        let dependencies = yaml_string_array(&frontmatter, "dependencies");
        let modified_at = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

        Ok(Skill {
            name,
            description,
            category,
            instructions,
            parameters,
            tags,
            version,
            dependencies,
            source_path: path.to_path_buf(),
            relative_path: relative.to_path_buf(),
            modified_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Recursively collect all `.md` files under `dir`, sorted.
fn collect_md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_dir(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("Failed to read dir: {}", dir.display()))?;

    for entry in entries {
        let entry = entry.context("Failed to read dir entry")?;
        let path = entry.path();

        // Skip symlinks to avoid infinite recursion
        if path.is_symlink() {
            debug!("Skipping symlink: {}", path.display());
            continue;
        }

        if path.is_dir() {
            walk_dir(&path, files)?;
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            files.push(path);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter from a markdown string.
///
/// Expects `---\n...\n---\nbody` format. Returns `(frontmatter, body)`.
/// If no frontmatter is found, returns `(Null, original_content)`.
fn parse_frontmatter(content: &str) -> Result<(serde_yaml::Value, String)> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return Ok((serde_yaml::Value::Null, content.to_string()));
    }

    let after_first = &trimmed[3..];
    let end = after_first.find("---").context("Unclosed frontmatter")?;

    let frontmatter_str = &after_first[..end];
    let body = after_first[end + 3..].to_string();

    let frontmatter: serde_yaml::Value = serde_yaml::from_str(frontmatter_str)
        .with_context(|| "Failed to parse frontmatter YAML")?;

    Ok((frontmatter, body))
}

/// Parse the `parameters` field from frontmatter.
///
/// Skips parameters with empty names (logs a warning).
fn parse_parameters(frontmatter: &serde_yaml::Value) -> Vec<SkillParameter> {
    let params = match frontmatter.get("parameters").and_then(|v| v.as_sequence()) {
        Some(seq) => seq,
        None => return Vec::new(),
    };

    params
        .iter()
        .filter_map(|v| {
            if v.is_mapping() {
                let name = match v.get("name").and_then(|n| n.as_str()) {
                    Some(n) if !n.is_empty() => n.to_string(),
                    Some(_) => {
                        warn!("Skipping parameter with empty name in frontmatter");
                        return None;
                    }
                    None => return None,
                };
                let description = v
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let required = v.get("required").and_then(|r| r.as_bool()).unwrap_or(false);
                Some(SkillParameter {
                    name,
                    description,
                    required,
                })
            } else {
                v.as_str()
                    .filter(|n| !n.is_empty())
                    .map(|name| SkillParameter {
                        name: name.to_string(),
                        description: String::new(),
                        required: false,
                    })
            }
        })
        .collect()
}

/// Extract a string array from a YAML frontmatter field.
fn yaml_string_array(value: &serde_yaml::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Content hashing
// ---------------------------------------------------------------------------

/// FNV-1a hash of file content for cache comparison.
fn hash_content(content: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in content.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn write_skill(dir: &Path, filename: &str, content: &str) -> PathBuf {
        let path = dir.join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    // -----------------------------------------------------------------------
    // Version parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_version_parse_full() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
    }

    #[test]
    fn test_version_parse_major_minor() {
        let v = Version::parse("2.5").unwrap();
        assert_eq!(v.major, 2);
        assert_eq!(v.minor, 5);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn test_version_parse_major_only() {
        let v = Version::parse("3").unwrap();
        assert_eq!(v.major, 3);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn test_version_parse_empty() {
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn test_version_parse_whitespace() {
        assert!(Version::parse("  ").is_none());
    }

    #[test]
    fn test_version_parse_trimmed() {
        let v = Version::parse("  1.0.0  ").unwrap();
        assert_eq!(
            v,
            Version {
                major: 1,
                minor: 0,
                patch: 0
            }
        );
    }

    #[test]
    fn test_version_display() {
        let v = Version {
            major: 1,
            minor: 2,
            patch: 3,
        };
        assert_eq!(format!("{}", v), "1.2.3");
    }

    #[test]
    fn test_version_ordering() {
        let v1 = Version::parse("1.0.0").unwrap();
        let v2 = Version::parse("1.1.0").unwrap();
        let v3 = Version::parse("2.0.0").unwrap();
        let v4 = Version::parse("1.0.1").unwrap();

        assert!(v1 < v2);
        assert!(v2 < v3);
        assert!(v1 < v4);
        assert!(v4 < v2);
        assert!(v1 < v3);
    }

    #[test]
    fn test_version_equality() {
        assert_eq!(Version::parse("1.2.3"), Version::parse("1.2.3"));
        assert_ne!(Version::parse("1.2.3"), Version::parse("1.2.4"));
    }

    // -----------------------------------------------------------------------
    // VersionWarning display
    // -----------------------------------------------------------------------

    #[test]
    fn test_version_warning_display() {
        let w = VersionWarning {
            skill_name: "deploy".to_string(),
            message: "no version".to_string(),
        };
        assert_eq!(format!("{}", w), "deploy: no version");
    }

    // -----------------------------------------------------------------------
    // parse_frontmatter
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_frontmatter_valid() {
        let input = "---\nname: test\n---\nbody text";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str(), Some("test"));
        assert_eq!(body.trim(), "body text");
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let input = "Just regular text";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert!(fm.is_null());
        assert_eq!(body, input);
    }

    #[test]
    fn test_parse_frontmatter_unclosed() {
        assert!(parse_frontmatter("---\nname: test\n").is_err());
    }

    #[test]
    fn test_parse_frontmatter_multiline_body() {
        let input = "---\nname: test\n---\n# Heading\n\nParagraph\n- item";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str(), Some("test"));
        assert!(body.contains("# Heading"));
    }

    // -----------------------------------------------------------------------
    // parse_parameters
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_parameters_structured() {
        let yaml = serde_yaml::from_str(
            "parameters:\n  - name: env\n    description: Target\n    required: true\n",
        )
        .unwrap();
        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "env");
        assert_eq!(params[0].description, "Target");
        assert!(params[0].required);
    }

    #[test]
    fn test_parse_parameters_string_shorthand() {
        let yaml = serde_yaml::from_str("parameters:\n  - query\n  - limit\n").unwrap();
        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 2);
        assert!(!params[0].required);
    }

    #[test]
    fn test_parse_parameters_empty() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("name: x").unwrap();
        assert!(parse_parameters(&yaml).is_empty());
    }

    // -----------------------------------------------------------------------
    // hash_content
    // -----------------------------------------------------------------------

    #[test]
    fn test_hash_content_deterministic() {
        let a = hash_content("hello world");
        let b = hash_content("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn test_hash_content_differs_on_change() {
        let a = hash_content("hello world");
        let b = hash_content("hello universe");
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------------
    // yaml_string_array
    // -----------------------------------------------------------------------

    #[test]
    fn test_yaml_string_array_present() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("tags:\n  - a\n  - b\n").unwrap();
        assert_eq!(yaml_string_array(&yaml, "tags"), vec!["a", "b"]);
    }

    #[test]
    fn test_yaml_string_array_missing() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("name: x\n").unwrap();
        assert!(yaml_string_array(&yaml, "tags").is_empty());
    }

    // -----------------------------------------------------------------------
    // collect_md_files
    // -----------------------------------------------------------------------

    #[test]
    fn test_collect_md_files_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "root.md", "r");
        write_skill(tmp.path(), "sub/nested.md", "n");
        write_skill(tmp.path(), "sub/deep/deeper.md", "d");
        write_skill(tmp.path(), "sub/deep/readme.txt", "t");

        let files = collect_md_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_collect_md_files_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let files = collect_md_files(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    // -----------------------------------------------------------------------
    // SkillLoader — basic loading
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_all_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
        assert!(loader.is_empty());
    }

    #[test]
    fn test_load_all_nonexistent_dir() {
        let mut loader = SkillLoader::new(PathBuf::from("/tmp/no_such_skills_dir_xyz"));
        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_load_all_single_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy.md",
            "---\nname: deploy\ndescription: Deploy the app\n---\n# Deploy\nRun it.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "Deploy the app");
        assert!(skills[0].instructions.contains("Run it."));
        assert!(skills[0].source_path.ends_with("deploy.md"));
        assert!(skills[0].modified_at.is_some());
    }

    #[test]
    fn test_load_all_name_defaults_to_file_stem() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "my_skill.md", "---\n---\nNo name field.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills[0].name, "my_skill");
    }

    #[test]
    fn test_load_all_no_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "plain.md", "Just plain markdown.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills[0].name, "plain");
        assert!(skills[0].instructions.contains("Just plain markdown."));
    }

    #[test]
    fn test_load_all_ignores_non_md() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "skill.md", "---\nname: good\n---\nOK");
        write_skill(tmp.path(), "notes.txt", "Not a skill");
        write_skill(tmp.path(), "config.yaml", "key: val");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Version and dependencies parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_version_parsed_from_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "v.md",
            "---\nname: v\nversion: \"1.2.3\"\n---\nV.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("v").unwrap().version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn test_version_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "nov.md", "---\nname: nov\n---\nNo version.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert!(loader.get("nov").unwrap().version.is_none());
    }

    #[test]
    fn test_dependencies_parsed_from_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "c.md",
            "---\nname: c\ndependencies:\n  - a\n  - b\n---\nC.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("c").unwrap().dependencies, vec!["a", "b"]);
    }

    #[test]
    fn test_dependencies_empty_when_not_set() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "solo.md", "---\nname: solo\n---\nSolo.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert!(loader.get("solo").unwrap().dependencies.is_empty());
    }

    // -----------------------------------------------------------------------
    // Dependency resolution — topological sort
    // -----------------------------------------------------------------------

    #[test]
    fn test_dependency_order_no_deps() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let order = loader.dependency_order();
        assert_eq!(order.len(), 2);
        assert!(order.contains(&"a".to_string()));
        assert!(order.contains(&"b".to_string()));
    }

    #[test]
    fn test_dependency_order_linear_chain() {
        // c depends on b, b depends on a
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(
            tmp.path(),
            "b.md",
            "---\nname: b\ndependencies:\n  - a\n---\nB.",
        );
        write_skill(
            tmp.path(),
            "c.md",
            "---\nname: c\ndependencies:\n  - b\n---\nC.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let order = loader.dependency_order();
        let pos_a = order.iter().position(|n| n == "a").unwrap();
        let pos_b = order.iter().position(|n| n == "b").unwrap();
        let pos_c = order.iter().position(|n| n == "c").unwrap();
        assert!(pos_a < pos_b, "a should come before b");
        assert!(pos_b < pos_c, "b should come before c");
    }

    #[test]
    fn test_dependency_order_diamond() {
        // d depends on b and c, b depends on a, c depends on a
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(
            tmp.path(),
            "b.md",
            "---\nname: b\ndependencies:\n  - a\n---\nB.",
        );
        write_skill(
            tmp.path(),
            "c.md",
            "---\nname: c\ndependencies:\n  - a\n---\nC.",
        );
        write_skill(
            tmp.path(),
            "d.md",
            "---\nname: d\ndependencies:\n  - b\n  - c\n---\nD.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let order = loader.dependency_order();
        let pos_a = order.iter().position(|n| n == "a").unwrap();
        let pos_b = order.iter().position(|n| n == "b").unwrap();
        let pos_c = order.iter().position(|n| n == "c").unwrap();
        let pos_d = order.iter().position(|n| n == "d").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_a < pos_c);
        assert!(pos_b < pos_d);
        assert!(pos_c < pos_d);
    }

    #[test]
    fn test_dependency_order_circular() {
        // a depends on b, b depends on a — circular
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a.md",
            "---\nname: a\ndependencies:\n  - b\n---\nA.",
        );
        write_skill(
            tmp.path(),
            "b.md",
            "---\nname: b\ndependencies:\n  - a\n---\nB.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let order = loader.dependency_order();
        // Should still return all skills (circular deps broken)
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn test_dependency_order_missing_dep() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "x.md",
            "---\nname: x\ndependencies:\n  - nonexistent\n---\nX.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let order = loader.dependency_order();
        assert_eq!(order, vec!["x"]);
    }

    // -----------------------------------------------------------------------
    // resolve_dependencies
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_dependencies_transitive() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(
            tmp.path(),
            "b.md",
            "---\nname: b\ndependencies:\n  - a\n---\nB.",
        );
        write_skill(
            tmp.path(),
            "c.md",
            "---\nname: c\ndependencies:\n  - b\n---\nC.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let deps = loader.resolve_dependencies("c");
        assert!(deps.contains(&"a".to_string()));
        assert!(deps.contains(&"b".to_string()));
        assert!(!deps.contains(&"c".to_string()));

        // a should come before b
        let pos_a = deps.iter().position(|n| n == "a").unwrap();
        let pos_b = deps.iter().position(|n| n == "b").unwrap();
        assert!(pos_a < pos_b);
    }

    #[test]
    fn test_resolve_dependencies_no_deps() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "solo.md", "---\nname: solo\n---\nSolo.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let deps = loader.resolve_dependencies("solo");
        assert!(deps.is_empty());
    }

    #[test]
    fn test_resolve_dependencies_unknown_skill() {
        let loader = SkillLoader::new(PathBuf::from("/tmp/empty"));
        let deps = loader.resolve_dependencies("nonexistent");
        assert!(deps.is_empty());
    }

    // -----------------------------------------------------------------------
    // load_all_ordered
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_all_ordered_respects_deps() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "base.md", "---\nname: base\n---\nBase.");
        write_skill(
            tmp.path(),
            "mid.md",
            "---\nname: mid\ndependencies:\n  - base\n---\nMid.",
        );
        write_skill(
            tmp.path(),
            "top.md",
            "---\nname: top\ndependencies:\n  - mid\n---\nTop.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all_ordered().unwrap();

        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        let pos_base = names.iter().position(|&n| n == "base").unwrap();
        let pos_mid = names.iter().position(|&n| n == "mid").unwrap();
        let pos_top = names.iter().position(|&n| n == "top").unwrap();
        assert!(pos_base < pos_mid);
        assert!(pos_mid < pos_top);
    }

    // -----------------------------------------------------------------------
    // Version checking
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_versions_warns_no_version() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "nov.md", "---\nname: nov\n---\nNo version.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let warnings = loader.check_versions();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].skill_name, "nov");
        assert!(warnings[0].message.contains("No version"));
    }

    #[test]
    fn test_check_versions_ok_with_version() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "v.md",
            "---\nname: v\nversion: \"1.0.0\"\n---\nHas version.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let warnings = loader.check_versions();
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_check_versions_warns_dep_no_version() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "base.md", "---\nname: base\n---\nNo version.");
        write_skill(
            tmp.path(),
            "top.md",
            "---\nname: top\nversion: \"1.0.0\"\ndependencies:\n  - base\n---\nTop.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let warnings = loader.check_versions();
        // base has no version (1 warning) + top depends on base which has no version (1 warning)
        assert_eq!(warnings.len(), 2);
        let messages: Vec<&str> = warnings.iter().map(|w| w.message.as_str()).collect();
        assert!(messages.iter().any(|m| m.contains("No version declared")));
        assert!(messages
            .iter()
            .any(|m| m.contains("Dependency 'base' has no version")));
    }

    #[test]
    fn test_skill_version_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "v.md",
            "---\nname: v\nversion: \"2.1.0\"\n---\nV.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let v = loader.skill_version("v").unwrap();
        assert_eq!(v.major, 2);
        assert_eq!(v.minor, 1);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn test_skill_version_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "nov.md", "---\nname: nov\n---\nNo version.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert!(loader.skill_version("nov").is_none());
    }

    // -----------------------------------------------------------------------
    // Cache invalidation — new methods
    // -----------------------------------------------------------------------

    #[test]
    fn test_invalidate_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 2);

        let _removed = loader.invalidate_by_name("a");
        // invalidate_by_name doesn't return, but the effect should be visible
        assert!(loader.get("a").is_none());
        assert!(loader.get("b").is_some());
        assert_eq!(loader.cache_size(), 1);
    }

    #[test]
    fn test_invalidate_by_name_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        loader.invalidate_by_name("nonexistent");
        assert_eq!(loader.cache_size(), 1);
        assert_eq!(loader.len(), 1);
    }

    #[test]
    fn test_invalidate_category() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/a.md",
            "---\nname: a\ncategory: devops\n---\nA.",
        );
        write_skill(
            tmp.path(),
            "devops/b.md",
            "---\nname: b\ncategory: devops\n---\nB.",
        );
        write_skill(
            tmp.path(),
            "monitor/c.md",
            "---\nname: c\ncategory: monitor\n---\nC.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 3);

        let removed = loader.invalidate_category("devops");
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"a".to_string()));
        assert!(removed.contains(&"b".to_string()));
        assert!(loader.get("a").is_none());
        assert!(loader.get("b").is_none());
        assert!(loader.get("c").is_some());
        assert_eq!(loader.cache_size(), 1);
    }

    #[test]
    fn test_invalidate_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "devops/a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "monitor/b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 2);

        let removed = loader.invalidate_pattern("devops/*.md");
        assert_eq!(removed.len(), 1);
        assert!(removed.contains(&"a".to_string()));
        assert!(loader.get("a").is_none());
        assert!(loader.get("b").is_some());
    }

    #[test]
    fn test_invalidate_cascade() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "base.md", "---\nname: base\n---\nBase.");
        write_skill(
            tmp.path(),
            "mid.md",
            "---\nname: mid\ndependencies:\n  - base\n---\nMid.",
        );
        write_skill(
            tmp.path(),
            "top.md",
            "---\nname: top\ndependencies:\n  - mid\n---\nTop.",
        );
        write_skill(
            tmp.path(),
            "unrelated.md",
            "---\nname: unrelated\n---\nUnrelated.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 4);

        let removed = loader.invalidate_cascade("base");
        // Should invalidate base, mid, and top (transitive dependents)
        assert!(removed.contains(&"base".to_string()));
        assert!(removed.contains(&"mid".to_string()));
        assert!(removed.contains(&"top".to_string()));
        assert!(!removed.contains(&"unrelated".to_string()));

        assert!(loader.get("base").is_none());
        assert!(loader.get("mid").is_none());
        assert!(loader.get("top").is_none());
        assert!(loader.get("unrelated").is_some());
    }

    #[test]
    fn test_invalidate_cascade_no_dependents() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "solo.md", "---\nname: solo\n---\nSolo.");
        write_skill(tmp.path(), "other.md", "---\nname: other\n---\nOther.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let removed = loader.invalidate_cascade("solo");
        assert_eq!(removed, vec!["solo"]);
        assert!(loader.get("solo").is_none());
        assert!(loader.get("other").is_some());
    }

    // -----------------------------------------------------------------------
    // Category from subdirectories
    // -----------------------------------------------------------------------

    #[test]
    fn test_category_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/deploy.md",
            "---\nname: deploy\n---\nDeploy it.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("deploy").unwrap().category, "devops");
    }

    #[test]
    fn test_category_from_frontmatter_overrides_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/deploy.md",
            "---\nname: deploy\ncategory: infrastructure\n---\nDeploy it.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("deploy").unwrap().category, "infrastructure");
    }

    #[test]
    fn test_category_uncategorized_when_root_level() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "tool.md", "---\nname: tool\n---\nRoot skill.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("tool").unwrap().category, "uncategorized");
    }

    #[test]
    fn test_category_deep_nested_uses_first_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/kubernetes/deploy.md",
            "---\nname: k8s_deploy\n---\nDeploy to k8s.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("k8s_deploy").unwrap().category, "devops");
    }

    // -----------------------------------------------------------------------
    // Subdirectory scanning
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_all_nested_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy/staging.md",
            "---\nname: deploy_staging\n---\nStaging deploy.",
        );
        write_skill(
            tmp.path(),
            "monitor/health.md",
            "---\nname: health_check\n---\nHealth check.",
        );
        write_skill(
            tmp.path(),
            "deep/nested/skill.md",
            "---\nname: deep_skill\n---\nDeep.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();

        assert_eq!(skills.len(), 3);
        assert!(loader.get("deploy_staging").is_some());
        assert!(loader.get("health_check").is_some());
        assert!(loader.get("deep_skill").is_some());
    }

    // -----------------------------------------------------------------------
    // Lookup and listing
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: alpha\n---\nAlpha.");
        write_skill(tmp.path(), "b.md", "---\nname: beta\n---\nBeta.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("alpha").unwrap().instructions, "Alpha.");
        assert_eq!(loader.get("beta").unwrap().instructions, "Beta.");
        assert!(loader.get("gamma").is_none());
    }

    #[test]
    fn test_list_by_category() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/deploy.md",
            "---\nname: deploy\n---\nDeploy.",
        );
        write_skill(
            tmp.path(),
            "devops/rollback.md",
            "---\nname: rollback\n---\nRollback.",
        );
        write_skill(
            tmp.path(),
            "monitor/health.md",
            "---\nname: health\n---\nHealth.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.list_by_category("devops").len(), 2);
        assert_eq!(loader.list_by_category("monitor").len(), 1);
        assert!(loader.list_by_category("nonexistent").is_empty());
    }

    #[test]
    fn test_categories_summary() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a/x.md", "---\nname: x\n---\nX.");
        write_skill(tmp.path(), "a/y.md", "---\nname: y\n---\nY.");
        write_skill(tmp.path(), "b/z.md", "---\nname: z\n---\nZ.");
        write_skill(tmp.path(), "root.md", "---\nname: root\n---\nRoot.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let cats = loader.categories();
        assert_eq!(cats.get("a"), Some(&2));
        assert_eq!(cats.get("b"), Some(&1));
        assert_eq!(cats.get("uncategorized"), Some(&1));
    }

    #[test]
    fn test_all_returns_everything() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.all().len(), 2);
    }

    #[test]
    fn test_len_and_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        assert!(loader.is_empty());
        assert_eq!(loader.len(), 0);

        write_skill(tmp.path(), "s.md", "---\nname: s\n---\nS.");
        loader.load_all().unwrap();
        assert_eq!(loader.len(), 1);
        assert!(!loader.is_empty());
    }

    // -----------------------------------------------------------------------
    // Parameters and tags
    // -----------------------------------------------------------------------

    #[test]
    fn test_parameters_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "search.md",
            "---\nname: search\nparameters:\n  - name: query\n    description: Terms\n    required: true\n  - count\n---\nSearch.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let skill = loader.get("search").unwrap();
        assert_eq!(skill.parameters.len(), 2);
        assert_eq!(skill.parameters[0].name, "query");
        assert!(skill.parameters[0].required);
        assert_eq!(skill.parameters[1].name, "count");
        assert!(!skill.parameters[1].required);
    }

    #[test]
    fn test_tags_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy.md",
            "---\nname: deploy\ntags:\n  - deploy\n  - cicd\n---\nDeploy.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("deploy").unwrap().tags, vec!["deploy", "cicd"]);
    }

    #[test]
    fn test_find_by_tag() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a.md",
            "---\nname: a\ntags:\n  - deploy\n---\nA.",
        );
        write_skill(
            tmp.path(),
            "b.md",
            "---\nname: b\ntags:\n  - monitor\n---\nB.",
        );
        write_skill(
            tmp.path(),
            "c.md",
            "---\nname: c\ntags:\n  - deploy\n  - cicd\n---\nC.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let deploy_skills = loader.find_by_tag("deploy");
        assert_eq!(deploy_skills.len(), 2);

        let monitor_skills = loader.find_by_tag("monitor");
        assert_eq!(monitor_skills.len(), 1);
    }

    #[test]
    fn test_find_by_tag_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a.md",
            "---\nname: a\ntags:\n  - Backend\n---\nA.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.find_by_tag("backend").len(), 1);
        assert_eq!(loader.find_by_tag("BACKEND").len(), 1);
    }

    // -----------------------------------------------------------------------
    // Caching — the key differentiator
    // -----------------------------------------------------------------------

    #[test]
    fn test_cache_hit_avoids_reparse() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "cached.md",
            "---\nname: cached\n---\nOriginal content.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());

        // First load — parses and caches.
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);
        assert_eq!(
            loader.get("cached").unwrap().instructions,
            "Original content."
        );

        // load_all without changes keeps cache.
        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(loader.cache_size(), 1);
    }

    #[test]
    fn test_cache_invalidated_on_content_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(
            tmp.path(),
            "change.md",
            "---\nname: change\n---\nVersion 1.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.get("change").unwrap().instructions, "Version 1.");

        // Overwrite the file with different content.
        fs::write(&path, "---\nname: change\n---\nVersion 2.").unwrap();

        loader.load_all().unwrap();
        assert_eq!(loader.get("change").unwrap().instructions, "Version 2.");
    }

    #[test]
    fn test_invalidate_all_forces_full_reload() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nAlpha.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);

        loader.invalidate_all();
        assert_eq!(loader.cache_size(), 0);

        // Skills should still be available until next load_all.
        assert_eq!(loader.len(), 1);

        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);
        assert_eq!(loader.get("a").unwrap().instructions, "Alpha.");
    }

    #[test]
    fn test_invalidate_path_specific() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 2);

        // Invalidate only "a.md".
        loader.invalidate_path(&PathBuf::from("a.md"));
        assert_eq!(loader.cache_size(), 1);
        assert!(loader.get("a").is_none());
        assert!(loader.get("b").is_some());
    }

    #[test]
    fn test_cache_prunes_deleted_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path_a = write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 2);

        // Delete file a.
        fs::remove_file(&path_a).unwrap();

        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);
        assert!(loader.get("a").is_none());
        assert!(loader.get("b").is_some());
    }

    #[test]
    fn test_new_file_discovered_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "first.md", "---\nname: first\n---\nFirst.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.len(), 1);

        write_skill(tmp.path(), "second.md", "---\nname: second\n---\nSecond.");

        loader.load_all().unwrap();
        assert_eq!(loader.len(), 2);
        assert_eq!(loader.cache_size(), 2);
        assert!(loader.get("second").is_some());
    }

    // -----------------------------------------------------------------------
    // Root path accessor
    // -----------------------------------------------------------------------

    #[test]
    fn test_root_accessor() {
        let loader = SkillLoader::new(PathBuf::from("/some/path"));
        assert_eq!(loader.root(), Path::new("/some/path"));
    }

    // -----------------------------------------------------------------------
    // Name validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_skill_empty_name_rejected() {
        let content = "---\nname: ''\n---\nBody text.";
        let path = Path::new("some.md");
        let relative = Path::new("some.md");
        assert!(SkillLoader::parse_skill(content, path, relative).is_err());
    }

    #[test]
    fn test_parse_skill_valid_name_ok() {
        let content = "---\nname: my-skill_v2\n---\nBody.";
        let path = Path::new("my-skill_v2.md");
        let relative = Path::new("my-skill_v2.md");
        let skill = SkillLoader::parse_skill(content, path, relative).unwrap();
        assert_eq!(skill.name, "my-skill_v2");
    }

    #[test]
    fn test_parse_skill_no_instructions_warns() {
        let content = "---\nname: empty_body\n---\n";
        let path = Path::new("empty_body.md");
        let relative = Path::new("empty_body.md");
        let skill = SkillLoader::parse_skill(content, path, relative).unwrap();
        assert!(skill.instructions.is_empty());
    }

    // -----------------------------------------------------------------------
    // Duplicate name detection (last-wins, warned)
    // -----------------------------------------------------------------------

    #[test]
    fn test_duplicate_name_last_wins() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: dup\n---\nFirst.");
        write_skill(tmp.path(), "b.md", "---\nname: dup\n---\nSecond.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();

        // Both skills returned in list
        assert_eq!(skills.len(), 2);
        // by_name lookup returns the last one loaded
        assert_eq!(loader.get("dup").unwrap().instructions, "Second.");
    }

    // -----------------------------------------------------------------------
    // Parameter name validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_parameters_skips_empty_name() {
        let yaml = serde_yaml::from_str(
            "parameters:\n  - name: ''\n    description: Empty name\n  - name: good\n",
        )
        .unwrap();
        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "good");
    }

    #[test]
    fn test_parse_parameters_skips_empty_shorthand() {
        let yaml = serde_yaml::from_str("parameters:\n  - ''\n  - valid\n").unwrap();
        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "valid");
    }

    // -----------------------------------------------------------------------
    // Symlink skipping
    // -----------------------------------------------------------------------

    #[test]
    fn test_collect_md_files_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "real.md", "---\nname: real\n---\nReal.");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = tmp.path().join("link.md");
            symlink(tmp.path().join("real.md"), &link).unwrap();
        }

        let files = collect_md_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("real.md"));
    }

    // -----------------------------------------------------------------------
    // Hot-reload: file watcher
    // -----------------------------------------------------------------------

    #[test]
    fn test_watcher_detects_modification() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(
            tmp.path(),
            "watched.md",
            "---\nname: watched\n---\nOriginal.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.get("watched").unwrap().instructions, "Original.");

        // Start watcher.
        loader.start_watcher().unwrap();
        assert!(loader.is_watching());

        // Give watcher time to initialize.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Modify file.
        fs::write(&path, "---\nname: watched\n---\nUpdated.").unwrap();

        // Give watcher time to detect change.
        std::thread::sleep(std::time::Duration::from_millis(500));

        let reloaded = loader.reload_changed().unwrap();
        assert!(
            !reloaded.is_empty() || loader.get("watched").unwrap().instructions == "Updated.",
            "Expected reload after modification"
        );

        loader.stop_watcher();
        assert!(!loader.is_watching());
    }

    #[test]
    fn test_watcher_detects_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "existing.md",
            "---\nname: existing\n---\nExisting.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.len(), 1);

        loader.start_watcher().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Add a new file.
        write_skill(
            tmp.path(),
            "new_skill.md",
            "---\nname: new_skill\n---\nNew.",
        );

        std::thread::sleep(std::time::Duration::from_millis(500));

        let reloaded = loader.reload_changed().unwrap();
        // After reload, both skills should be present.
        loader.load_all().unwrap();
        assert_eq!(loader.len(), 2);
        assert!(loader.get("new_skill").is_some());

        loader.stop_watcher();
    }

    #[test]
    fn test_reload_changed_fallback_no_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(
            tmp.path(),
            "fallback.md",
            "---\nname: fallback\n---\nOriginal.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert!(!loader.is_watching());

        // Modify file mtime to trigger reload.
        let new_time = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(new_time)).unwrap();
        fs::write(&path, "---\nname: fallback\n---\nUpdated.").unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(new_time)).unwrap();

        let reloaded = loader.reload_changed().unwrap();
        assert!(reloaded.contains(&"fallback".to_string()));
        assert_eq!(loader.get("fallback").unwrap().instructions, "Updated.");
    }

    #[test]
    fn test_start_stop_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "s.md", "---\nname: s\n---\nS.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert!(!loader.is_watching());
        loader.start_watcher().unwrap();
        assert!(loader.is_watching());
        loader.stop_watcher();
        assert!(!loader.is_watching());
    }

    // -----------------------------------------------------------------------
    // SkillWatcher unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_watcher_changed_paths_empty_initially() {
        let tmp = tempfile::tempdir().unwrap();
        let watcher = SkillWatcher::new(tmp.path().to_path_buf()).unwrap();
        let paths = watcher.changed_paths();
        assert!(paths.is_empty());
    }
}
