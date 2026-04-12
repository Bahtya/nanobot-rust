//! JSON Schema types for tool parameter validation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON Schema type enumeration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemaType {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Object,
}

/// A JSON Schema definition for tool parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterSchema {
    #[serde(rename = "type")]
    pub schema_type: SchemaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<ParameterSchema>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<std::collections::HashMap<String, ParameterSchema>>,
    #[serde(default)]
    pub required: Vec<String>,
}

/// Build a tool parameters schema (the outer "object" wrapper).
pub fn tool_parameters_schema(
    properties: std::collections::HashMap<String, ParameterSchema>,
    required: Vec<String>,
) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_parameter_schema_serialization() {
        let schema = ParameterSchema {
            schema_type: SchemaType::String,
            description: Some("A test parameter".to_string()),
            default: Some(serde_json::json!("default_val")),
            enum_values: None,
            items: None,
            properties: None,
            required: vec![],
        };
        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["type"], "string");
        assert_eq!(json["description"], "A test parameter");
        assert_eq!(json["default"], "default_val");
        assert!(json.get("enum_values").is_none());
        assert!(json.get("items").is_none());
        assert!(json.get("properties").is_none());
    }

    #[test]
    fn test_tool_parameters_schema() {
        let mut props = HashMap::new();
        props.insert(
            "query".to_string(),
            ParameterSchema {
                schema_type: SchemaType::String,
                description: Some("Search query".to_string()),
                default: None,
                enum_values: None,
                items: None,
                properties: None,
                required: vec![],
            },
        );
        let schema = tool_parameters_schema(props, vec!["query".to_string()]);
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["query"].is_object());
        assert_eq!(schema["required"][0], "query");
    }

    #[test]
    fn test_schema_type_serde() {
        let cases = vec![
            (SchemaType::String, "string"),
            (SchemaType::Integer, "integer"),
            (SchemaType::Number, "number"),
            (SchemaType::Boolean, "boolean"),
            (SchemaType::Array, "array"),
            (SchemaType::Object, "object"),
        ];
        for (schema_type, expected) in cases {
            let json = serde_json::to_value(&schema_type).unwrap();
            assert_eq!(json, expected);
            let deserialized: SchemaType = serde_json::from_value(json).unwrap();
            assert!(
                matches!(deserialized, _ if std::mem::discriminant(&deserialized) == std::mem::discriminant(&schema_type))
            );
        }
    }
}
