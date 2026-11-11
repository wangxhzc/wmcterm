use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputField {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub required: bool,
    pub options: Vec<String>,
}

/// Provider-neutral bounded form shown by the actionable request panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFormRequest {
    pub id: Value,
    pub method: String,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub message: String,
    pub fields: Vec<InputField>,
}

impl InputFormRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.method.as_str(),
            "item/tool/requestUserInput" | "mcpServer/elicitation/request" | "Elicitation"
        ) || (!self.id.is_string() && !self.id.is_number())
            || self
                .id
                .as_str()
                .is_some_and(|id| id.is_empty() || id.len() > 4096)
            || self.thread_id.is_empty()
            || self.turn_id.is_empty()
            || self.item_id.is_empty()
            || self.fields.is_empty()
            || self.fields.len() > 32
            || serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len()
                > 64 * 1024
        {
            return Err("invalid input request identity".into());
        }
        let mut ids = HashSet::new();
        for field in &self.fields {
            if field.id.is_empty()
                || field.id.len() > 256
                || field.label.len() > 4096
                || field.options.len() > 32
                || field.options.iter().any(|option| option.len() > 4096)
                || !ids.insert(&field.id)
                || !matches!(
                    field.kind.as_str(),
                    "answer" | "string" | "number" | "integer" | "boolean"
                )
            {
                return Err("invalid input field".into());
            }
        }
        Ok(())
    }

    /// A strict subset of MCP JSON Schema; unsupported constraints stay native.
    pub fn parse_schema(schema: &Value) -> Option<Vec<InputField>> {
        if schema["type"] != "object"
            || schema
                .as_object()?
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "properties" | "required" | "$schema"))
        {
            return None;
        }
        let properties = schema["properties"].as_object()?;
        let required = match schema.get("required") {
            Some(required) => required
                .as_array()?
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?,
            None => Vec::new(),
        };
        if required.iter().any(|name| !properties.contains_key(*name)) {
            return None;
        }
        let mut fields = Vec::new();
        for (name, property) in properties {
            if property.as_object()?.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "title" | "description" | "enum" | "default"
                )
            }) {
                return None;
            }
            let kind = property["type"].as_str()?;
            if !matches!(kind, "string" | "number" | "integer" | "boolean") {
                return None;
            }
            let options = if let Some(values) = property.get("enum") {
                if kind != "string" || values.as_array()?.is_empty() {
                    return None;
                }
                values
                    .as_array()?
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?
            } else if kind == "boolean" {
                vec!["true".into(), "false".into()]
            } else {
                Vec::new()
            };
            fields.push(InputField {
                id: name.clone(),
                label: property["title"].as_str().unwrap_or(name).into(),
                kind: kind.into(),
                required: required.contains(&name.as_str()),
                options,
            });
        }
        Some(fields)
    }

    pub fn field_value(&self, index: usize, text: &str) -> Result<Value, String> {
        let field = self.fields.get(index).ok_or("input field not found")?;
        if text.len() > 4096 {
            return Err("answer exceeds 4096 bytes".into());
        }
        if field.required && text.is_empty() {
            return Err("answer is required".into());
        }
        if !field.required && text.is_empty() {
            return Ok(Value::Null);
        }
        if !field.options.is_empty()
            && field.kind != "answer"
            && !field.options.iter().any(|option| option == text)
        {
            return Err("answer must match an available option".into());
        }
        match field.kind.as_str() {
            "answer" | "string" => Ok(json!(text)),
            "boolean" => match text {
                "true" => Ok(json!(true)),
                "false" => Ok(json!(false)),
                _ => Err("expected true or false".into()),
            },
            "integer" => text
                .parse::<i64>()
                .map(|value| json!(value))
                .map_err(|_| "expected an integer".into()),
            "number" => text
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .map(|value| json!(value))
                .ok_or_else(|| "expected a finite number".into()),
            _ => Err("unsupported field type".into()),
        }
    }

    pub fn response_content(&self, text: &str) -> Result<Value, String> {
        self.validate()?;
        if text.len() > 64 * 1024 {
            return Err("answers exceed 64 KiB".into());
        }
        let answers: serde_json::Map<String, Value> =
            serde_json::from_str(text).map_err(|_| "answers must be an object".to_owned())?;
        if answers.values().any(|value| !value.is_string()) {
            return Err("answer values must be strings".into());
        }
        if answers
            .keys()
            .any(|key| !self.fields.iter().any(|field| &field.id == key))
        {
            return Err("unknown answer field".into());
        }
        let mut content = serde_json::Map::new();
        for (index, field) in self.fields.iter().enumerate() {
            let answer = answers.get(&field.id).and_then(Value::as_str).unwrap_or("");
            let value = self.field_value(index, answer)?;
            if field.kind == "answer" {
                content.insert(field.id.clone(), json!({"answers": [answer]}));
            } else if !value.is_null() {
                content.insert(field.id.clone(), value);
            }
        }
        Ok(Value::Object(content))
    }

    pub fn response(&self, text: Option<&str>) -> Result<Value, String> {
        let result = match (self.method.as_str(), text) {
            ("item/tool/requestUserInput", Some(text)) => {
                json!({"answers": self.response_content(text)?})
            }
            ("item/tool/requestUserInput", None) => json!({"answers": {}}),
            (_, Some(text)) => {
                json!({"action": "accept", "content": self.response_content(text)?, "_meta": null})
            }
            (_, None) => json!({"action": "cancel", "content": null, "_meta": null}),
        };
        if self.method == "Elicitation" {
            let mut output = result.as_object().expect("result is an object").clone();
            output.remove("_meta");
            output.insert("hookEventName".into(), json!("Elicitation"));
            Ok(json!({"hookSpecificOutput": output}))
        } else {
            Ok(json!({"id": self.id, "result": result}))
        }
    }
}
