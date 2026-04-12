//! Filesystem tools — read, write, edit, list.

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

// ─── ReadFileTool ────────────────────────────────────────────

pub struct ReadFileTool;

impl ReadFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and pagination."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read" },
                "offset": { "type": "integer", "description": "Line number to start reading from (0-indexed)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read" },
            },
            "required": ["path"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'path' parameter".to_string()))?;

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read file: {}", e)))?;

        let lines: Vec<&str> = content.lines().collect();
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().map(|l| l as usize);

        let selected: Vec<&str> = if let Some(limit) = limit {
            lines.iter().skip(offset).take(limit).copied().collect()
        } else {
            lines.iter().skip(offset).copied().collect()
        };

        let mut result = String::new();
        for (i, line) in selected.iter().enumerate() {
            result.push_str(&format!("{}\t{}\n", offset + i + 1, line));
        }

        Ok(result)
    }
}

// ─── WriteFileTool ────────────────────────────────────────────

pub struct WriteFileTool;

impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates parent directories if needed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write" },
                "content": { "type": "string", "description": "Content to write to the file" },
                "append": { "type": "boolean", "description": "Whether to append instead of overwrite" },
            },
            "required": ["path", "content"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'path'".to_string()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'content'".to_string()))?;
        let append = args["append"].as_bool().unwrap_or(false);

        // Create parent directory if needed
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::Execution(format!("Failed to create directory: {}", e))
                })?;
            }
        }

        if append {
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(|e| ToolError::Execution(format!("Failed to open file: {}", e)))?
                .write_all(content.as_bytes())
                .await
                .map_err(|e| ToolError::Execution(format!("Failed to write: {}", e)))?;
        } else {
            tokio::fs::write(path, content)
                .await
                .map_err(|e| ToolError::Execution(format!("Failed to write file: {}", e)))?;
        }

        Ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path
        ))
    }
}

// ─── EditFileTool ────────────────────────────────────────────

pub struct EditFileTool;

impl EditFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EditFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact text matches. Use for making targeted changes to existing files."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit" },
                "old_text": { "type": "string", "description": "Exact text to find and replace" },
                "new_text": { "type": "string", "description": "Text to replace with" },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences instead of just the first" },
            },
            "required": ["path", "old_text", "new_text"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'path'".to_string()))?;
        let old_text = args["old_text"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'old_text'".to_string()))?;
        let new_text = args["new_text"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'new_text'".to_string()))?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read file: {}", e)))?;

        let count = if replace_all {
            content.matches(old_text).count()
        } else {
            if content.contains(old_text) {
                1
            } else {
                0
            }
        };

        if count == 0 {
            return Err(ToolError::Execution("Text not found in file".to_string()));
        }

        let new_content = if replace_all {
            content.replace(old_text, new_text)
        } else {
            content.replacen(old_text, new_text, 1)
        };

        tokio::fs::write(path, new_content)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write file: {}", e)))?;

        Ok(format!("Replaced {} occurrence(s) in {}", count, path))
    }
}

// ─── ListDirTool ────────────────────────────────────────────

pub struct ListDirTool;

impl ListDirTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ListDirTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List directory contents. Supports recursive listing."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path to list" },
                "recursive": { "type": "boolean", "description": "List recursively" },
            },
            "required": ["path"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'path'".to_string()))?;
        let recursive = args["recursive"].as_bool().unwrap_or(false);

        let mut result = Vec::new();
        list_dir_recursive(path, recursive, &mut result, 0)?;

        Ok(result.join("\n"))
    }
}

fn list_dir_recursive(
    path: &str,
    recursive: bool,
    result: &mut Vec<String>,
    depth: usize,
) -> Result<(), ToolError> {
    let entries = std::fs::read_dir(path)
        .map_err(|e| ToolError::Execution(format!("Failed to read directory: {}", e)))?;

    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    let indent = "  ".repeat(depth);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry
            .file_type()
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        if file_type.is_dir() {
            result.push(format!("{}{}/", indent, name));
            if recursive {
                let sub_path = format!("{}/{}", path, name);
                list_dir_recursive(&sub_path, true, result, depth + 1)?;
            }
        } else {
            result.push(format!("{}{}", indent, name));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trait_def::Tool;

    #[test]
    fn test_read_file_tool_schema() {
        let tool = ReadFileTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["offset"].is_object());
        assert!(schema["properties"]["limit"].is_object());
        let required = schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"path"));
    }

    #[test]
    fn test_write_file_tool_schema() {
        let tool = WriteFileTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["content"].is_object());
        assert!(schema["properties"]["append"].is_object());
        let required = schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"path"));
        assert!(required_names.contains(&"content"));
    }

    #[test]
    fn test_edit_file_tool_schema() {
        let tool = EditFileTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["old_text"].is_object());
        assert!(schema["properties"]["new_text"].is_object());
        assert!(schema["properties"]["replace_all"].is_object());
        let required = schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"path"));
        assert!(required_names.contains(&"old_text"));
        assert!(required_names.contains(&"new_text"));
    }

    #[test]
    fn test_list_dir_tool_schema() {
        let tool = ListDirTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["recursive"].is_object());
        let required = schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"path"));
    }

    #[tokio::test]
    async fn test_read_file_execute() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "line1\nline2\nline3\n")
            .await
            .unwrap();

        let tool = ReadFileTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap()
            }))
            .await
            .unwrap();
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
    }

    #[tokio::test]
    async fn test_read_file_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "line1\nline2\nline3\nline4\nline5\n")
            .await
            .unwrap();

        let tool = ReadFileTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "offset": 2,
                "limit": 2
            }))
            .await
            .unwrap();
        assert!(result.contains("line3"));
        assert!(result.contains("line4"));
        assert!(!result.contains("line1"));
        assert!(!result.contains("line5"));
    }

    #[tokio::test]
    async fn test_read_file_missing_path() {
        let tool = ReadFileTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing 'path'"));
    }

    #[tokio::test]
    async fn test_read_file_nonexistent() {
        let tool = ReadFileTool::new();
        let result = tool
            .execute(json!({
                "path": "/tmp/nonexistent_nanobot_test_file_9999.txt"
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_file_execute() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("output.txt");

        let tool = WriteFileTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "content": "hello world"
            }))
            .await
            .unwrap();
        assert!(result.contains("Successfully wrote"));

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_write_file_append() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("append.txt");

        let tool = WriteFileTool::new();
        tool.execute(json!({
            "path": file_path.to_str().unwrap(),
            "content": "first"
        }))
        .await
        .unwrap();
        tool.execute(json!({
            "path": file_path.to_str().unwrap(),
            "content": " second",
            "append": true
        }))
        .await
        .unwrap();

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "first second");
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("nested").join("deep").join("file.txt");

        let tool = WriteFileTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "content": "nested"
            }))
            .await
            .unwrap();
        assert!(result.contains("Successfully wrote"));

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "nested");
    }

    #[tokio::test]
    async fn test_edit_file_execute() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("edit.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = EditFileTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "old_text": "world",
                "new_text": "rust"
            }))
            .await
            .unwrap();
        assert!(result.contains("Replaced 1 occurrence"));

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "hello rust");
    }

    #[tokio::test]
    async fn test_edit_file_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("edit_all.txt");
        tokio::fs::write(&file_path, "aaa bbb aaa ccc aaa")
            .await
            .unwrap();

        let tool = EditFileTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "old_text": "aaa",
                "new_text": "xxx",
                "replace_all": true
            }))
            .await
            .unwrap();
        assert!(result.contains("Replaced 3 occurrence"));

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "xxx bbb xxx ccc xxx");
    }

    #[tokio::test]
    async fn test_edit_file_text_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("edit.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let tool = EditFileTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "old_text": "not_present",
                "new_text": "replacement"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Text not found"));
    }

    #[tokio::test]
    async fn test_list_dir_execute() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.rs"), "").await.unwrap();
        tokio::fs::create_dir(dir.path().join("subdir"))
            .await
            .unwrap();

        let tool = ListDirTool::new();
        let result = tool
            .execute(json!({
                "path": dir.path().to_str().unwrap()
            }))
            .await
            .unwrap();
        assert!(result.contains("a.txt"));
        assert!(result.contains("b.rs"));
        assert!(result.contains("subdir/"));
    }

    #[tokio::test]
    async fn test_list_dir_recursive() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub").join("nested.txt"), "")
            .await
            .unwrap();

        let tool = ListDirTool::new();
        let result = tool
            .execute(json!({
                "path": dir.path().to_str().unwrap(),
                "recursive": true
            }))
            .await
            .unwrap();
        assert!(result.contains("sub/"));
        assert!(result.contains("nested.txt"));
    }
}
