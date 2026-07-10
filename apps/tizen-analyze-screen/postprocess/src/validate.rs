//! `validate_json` step: pure-code schema validation of the VLM output.
//! No LLM involved here — the repair pass lives in the pipeline and is only
//! taken when both strict parsing and fence/JSON extraction fail.

use serde_json::Value;

/// Human-readable schema summary, reused in the repair prompt.
pub const SCHEMA_HINT: &str = r#"{
  "screen_type": "string",
  "title": "string",
  "summary": "string",
  "detected_elements": [ { "name": "string", "description": "string", "confidence": 0.0 } ],
  "suggested_actions": [ "string" ],
  "error": null or { "code": "string", "message": "string" }
}"#;

/// Strict parse + schema validation. Ok(Value) is guaranteed schema-conformant.
pub fn parse_and_validate(text: &str) -> Result<Value, Vec<String>> {
    let value: Value =
        serde_json::from_str(text.trim()).map_err(|e| vec![format!("not valid JSON: {e}")])?;
    let errors = schema_errors(&value);
    if errors.is_empty() { Ok(value) } else { Err(errors) }
}

pub fn schema_errors(v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let obj = match v.as_object() {
        Some(o) => o,
        None => return vec!["top-level value is not an object".into()],
    };

    for key in ["screen_type", "title", "summary"] {
        match obj.get(key) {
            Some(Value::String(_)) => {}
            Some(_) => errs.push(format!("{key} must be a string")),
            None => errs.push(format!("missing required field {key}")),
        }
    }

    match obj.get("detected_elements") {
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let Some(el) = item.as_object() else {
                    errs.push(format!("detected_elements[{i}] is not an object"));
                    continue;
                };
                for key in ["name", "description"] {
                    if !matches!(el.get(key), Some(Value::String(_))) {
                        errs.push(format!("detected_elements[{i}].{key} must be a string"));
                    }
                }
                match el.get("confidence").and_then(Value::as_f64) {
                    Some(c) if (0.0..=1.0).contains(&c) => {}
                    Some(c) => errs.push(format!(
                        "detected_elements[{i}].confidence {c} out of range 0..1"
                    )),
                    None => errs.push(format!(
                        "detected_elements[{i}].confidence must be a number"
                    )),
                }
            }
        }
        Some(_) => errs.push("detected_elements must be an array".into()),
        None => errs.push("missing required field detected_elements".into()),
    }

    match obj.get("suggested_actions") {
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                if !item.is_string() {
                    errs.push(format!("suggested_actions[{i}] must be a string"));
                }
            }
        }
        Some(_) => errs.push("suggested_actions must be an array".into()),
        None => errs.push("missing required field suggested_actions".into()),
    }

    match obj.get("error") {
        None => errs.push("missing required field error (use null on success)".into()),
        Some(Value::Null) => {}
        Some(Value::Object(e)) => {
            for key in ["code", "message"] {
                if !matches!(e.get(key), Some(Value::String(_))) {
                    errs.push(format!("error.{key} must be a string"));
                }
            }
        }
        Some(_) => errs.push("error must be null or an object".into()),
    }

    errs
}

/// Second-chance extraction: strip markdown fences, or pull the first
/// balanced top-level JSON object out of surrounding prose.
pub fn extract_json_candidate(text: &str) -> Option<String> {
    let trimmed = text.trim();

    // ```json ... ``` (or bare ```)
    if let Some(fence_start) = trimmed.find("```") {
        let after = &trimmed[fence_start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(fence_end) = after.find("```") {
            let inner = after[..fence_end].trim();
            if !inner.is_empty() {
                return Some(inner.to_string());
            }
        }
    }

    // First balanced {...}, respecting strings/escapes.
    let bytes = trimmed.as_bytes();
    let start = trimmed.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escape {
            escape = false;
            continue;
        }
        match b {
            b'\\' if in_str => escape = true,
            b'"' => in_str = !in_str,
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(trimmed[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Schema-shaped structured error object (the contract's `error` field set).
pub fn error_object(code: &str, message: &str) -> Value {
    serde_json::json!({
        "screen_type": "unknown",
        "title": "Analysis failed",
        "summary": message,
        "detected_elements": [],
        "suggested_actions": [],
        "error": { "code": code, "message": message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"screen_type":"menu","title":"t","summary":"s",
        "detected_elements":[{"name":"n","description":"d","confidence":0.5}],
        "suggested_actions":["a"],"error":null}"#;

    #[test]
    fn valid_passes() {
        assert!(parse_and_validate(VALID).is_ok());
    }

    #[test]
    fn missing_field_fails() {
        let v: Value = serde_json::from_str(r#"{"screen_type":"menu"}"#).unwrap();
        assert!(!schema_errors(&v).is_empty());
    }

    #[test]
    fn confidence_out_of_range_fails() {
        let bad = VALID.replace("0.5", "1.5");
        assert!(parse_and_validate(&bad).is_err());
    }

    #[test]
    fn error_variant_passes() {
        let v = error_object("VLM_INVALID", "oops");
        assert!(schema_errors(&v).is_empty());
    }

    #[test]
    fn extracts_from_fences() {
        let fenced = format!("Sure!\n```json\n{VALID}\n```\nDone.");
        let got = extract_json_candidate(&fenced).unwrap();
        assert!(parse_and_validate(&got).is_ok());
    }

    #[test]
    fn extracts_first_object_from_prose() {
        let prose = format!("The answer is {VALID} hope that helps");
        let got = extract_json_candidate(&prose).unwrap();
        assert!(parse_and_validate(&got).is_ok());
    }

    #[test]
    fn extraction_handles_braces_in_strings() {
        let tricky = r#"note {"screen_type":"menu","title":"a { b } c","summary":"s",
            "detected_elements":[],"suggested_actions":[],"error":null} tail"#;
        let got = extract_json_candidate(tricky).unwrap();
        assert!(parse_and_validate(&got).is_ok());
    }
}
