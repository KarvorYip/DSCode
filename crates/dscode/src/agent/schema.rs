//! Minimal JSON Schema validation for spawn output contracts (tools.zh.md §3.8).
//! Hand-rolled subset — no new crate: type / properties / required / items / enum.
//! Unsupported keywords are ignored (permissive by construction); `schemaMode` decides
//! how a mismatch is treated by the caller (strict rejects, permissive warns).

use serde_json::Value;

/// schemaMode: strict rejects non-conforming output; permissive accepts with a warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaMode {
    Strict,
    Permissive,
}

impl SchemaMode {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("permissive") => SchemaMode::Permissive,
            _ => SchemaMode::Strict,
        }
    }
}

/// Validate `instance` against `schema`. Err carries a path-qualified message.
pub fn validate(instance: &Value, schema: &Value) -> Result<(), String> {
    validate_at(instance, schema, "$")
}

fn validate_at(instance: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return Ok(()); // a non-object schema is treated as no constraint
    };
    if let Some(ty) = obj.get("type").and_then(Value::as_str) {
        if !type_matches(instance, ty) {
            return Err(format!("{path}：类型应为 {ty}，实际 {}", type_of(instance)));
        }
    }
    if let Some(en) = obj.get("enum").and_then(Value::as_array) {
        if !en.contains(instance) {
            return Err(format!("{path}：不在 enum 允许值内"));
        }
    }
    match instance {
        Value::Object(map) => {
            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                for (key, sub) in props {
                    if let Some(v) = map.get(key) {
                        validate_at(v, sub, &format!("{path}.{key}"))?;
                    }
                }
            }
            if let Some(req) = obj.get("required").and_then(Value::as_array) {
                for r in req {
                    if let Some(name) = r.as_str() {
                        if !map.contains_key(name) {
                            return Err(format!("{path}：缺少必填字段 {name}"));
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = obj.get("items") {
                for (i, v) in items.iter().enumerate() {
                    validate_at(v, item_schema, &format!("{path}[{i}]"))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn type_matches(v: &Value, ty: &str) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        // JSON Schema: integer accepts numbers with zero fractional part.
        "integer" => {
            v.as_i64().is_some()
                || v.as_f64()
                    .is_some_and(|f| f.fract() == 0.0 && f.is_finite())
        }
        _ => true, // unknown type keyword: no constraint
    }
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "files": { "type": "array", "items": { "type": "string" } },
                "count": { "type": "integer" }
            },
            "required": ["files"]
        })
    }

    #[test]
    fn schema_合法产出通过() {
        assert!(validate(&json!({ "files": ["a.rs"], "count": 2 }), &schema()).is_ok());
    }

    #[test]
    fn schema_缺必填字段拒绝并点名() {
        let err = validate(&json!({ "count": 1 }), &schema()).unwrap_err();
        assert!(err.contains("files"), "须点名缺失字段：{err}");
    }

    #[test]
    fn schema_数组元素类型错误带路径() {
        let err = validate(&json!({ "files": ["a.rs", 3] }), &schema()).unwrap_err();
        assert!(err.contains("$.files[1]"), "路径应定位到元素：{err}");
    }

    #[test]
    fn schema_类型不匹配拒绝() {
        let err = validate(&json!("plain"), &schema()).unwrap_err();
        assert!(err.contains("object"), "应报告期望类型：{err}");
    }

    #[test]
    fn schema_整数接受浮点整数值() {
        assert!(validate(&json!(3.0), &json!({ "type": "integer" })).is_ok());
        assert!(validate(&json!(3.5), &json!({ "type": "integer" })).is_err());
    }

    #[test]
    fn schema_enum约束() {
        let s = json!({ "enum": ["a", "b"] });
        assert!(validate(&json!("a"), &s).is_ok());
        assert!(validate(&json!("c"), &s).is_err());
    }
}
