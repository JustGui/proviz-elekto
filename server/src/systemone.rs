//! TypeSafe System One adapter for flat finite JSON-schema decisions.
//! Jev cannot generate text or call tools. Explicit `decision` category opt-in is required.
use super::{ChatMessage, CompleteRequest, ParsedCompletion, ProviderError};
use serde_json::{json, Map, Value};

// Respect configured/mock base URLs rather than hard-coding a vendor host.
pub(super) fn url(brand: &str, base_url: &Option<String>) -> Option<String> {
    let base = proviz_elekto_core::env_expand::expand_env_placeholders(base_url.as_deref()?);
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    if brand.split('-').next() == Some("openrouter") {
        Some(format!(
            "{}/alpha/decisions",
            base.strip_suffix("/v1").unwrap_or(base)
        ))
    } else {
        Some(format!("{base}/systemone"))
    }
}

fn error(message: &str) -> ProviderError {
    ProviderError {
        retry_after_ms: None,
        quota_scope_brand: false,
        message: message.into(),
        is_rate_limit: false,
    }
}

pub(super) fn payload(req: &CompleteRequest, model: &str) -> Result<Value, ProviderError> {
    if req.tools.is_some() || req.requires_fn_call {
        return Err(error("TypeSafe supports finite decisions, not tool calls"));
    }
    let schema = req
        .response_format
        .as_ref()
        .and_then(|v| v.pointer("/json_schema/schema"))
        .ok_or_else(|| {
            error("TypeSafe requires response_format.json_schema with finite choices/booleans")
        })?;
    let props = schema["properties"]
        .as_object()
        .ok_or_else(|| error("TypeSafe schema must be a flat object"))?;
    if schema["type"] != "object" || props.is_empty() || props.len() > 64 {
        return Err(error("TypeSafe schema must have 1..64 finite properties"));
    }
    let required = schema["required"]
        .as_array()
        .ok_or_else(|| error("TypeSafe schema must require every property"))?;
    if required.len() != props.len() || props.keys().any(|k| !required.contains(&json!(k))) {
        return Err(error("TypeSafe schema must require every property"));
    }
    let instructions = req
        .messages
        .iter()
        .filter(|m| m.role == "system" || m.role == "developer")
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let state: Vec<_> = req
        .messages
        .iter()
        .filter(|m| m.role != "system" && m.role != "developer")
        .map(|ChatMessage { role, content, .. }| json!({"role":role,"content":content}))
        .collect();
    let mut questions = Map::new();
    for (name, property) in props {
        // Question IDs are not sent to inference. Include the field's meaning explicitly.
        let question = json!({"task":instructions,"field":name,"meaning":property["description"]});
        let value = if property["type"] == "boolean" {
            json!({"type":"noul","instructions":question,"criteria":{"true":"The proposition is true.","false":"The proposition is false or unsupported."}})
        } else if let Some(options) = property["enum"].as_array() {
            if options.is_empty()
                || options.len() > 255
                || options.iter().any(|v| v.is_array() || v.is_object())
            {
                return Err(error(
                    "TypeSafe choice cardinality must be 1..255 scalar options",
                ));
            }
            let criteria: Map<String, Value> = options
                .iter()
                .enumerate()
                .map(|(i, value)| (format!("option_{i}"), json!({"value":value})))
                .collect();
            json!({"type":"choice","instructions":question,"criteria":criteria})
        } else {
            return Err(error(
                "TypeSafe cannot generate strings/numbers: use enum or boolean properties",
            ));
        };
        questions.insert(name.clone(), value);
    }
    Ok(json!({"model":model,"state":state,"questions":questions}))
}

pub(super) fn parse(body: &Value, payload: &Value) -> Result<ParsedCompletion, ProviderError> {
    let questions = payload["questions"]
        .as_object()
        .ok_or_else(|| error("invalid TypeSafe questions"))?;
    let answers = body["answers"]
        .as_object()
        .ok_or_else(|| error("TypeSafe response missing answers"))?;
    let mut values = Map::new();
    for (name, q) in questions {
        let a = answers
            .get(name)
            .ok_or_else(|| error("TypeSafe response missing a required answer"))?;
        let value = if q["type"] == "noul" {
            let p = a["noul"]
                .as_f64()
                .filter(|p| (0.0..=1.0).contains(p))
                .ok_or_else(|| error("TypeSafe response contains invalid probability"))?;
            json!(p >= 0.5)
        } else {
            let choice = a["choice"]
                .as_str()
                .ok_or_else(|| error("TypeSafe response missing choice"))?;
            q["criteria"]
                .get(choice)
                .and_then(|v| v.get("value"))
                .cloned()
                .ok_or_else(|| error("TypeSafe returned a choice outside the offered options"))?
        };
        values.insert(name.clone(), value);
    }
    let prompt_tokens = body["usage"]["input_tokens"]
        .as_u64()
        .ok_or_else(|| error("TypeSafe response missing input token usage"))?;
    let completion_tokens = body["usage"]["output_tokens"].as_u64().unwrap_or(0);
    Ok(ParsedCompletion {
        text: Value::Object(values).to_string(),
        tool_calls: None,
        prompt_tokens,
        completion_tokens,
        cached_tokens: 0,
        remaining_requests: None,
        remaining_tokens: None,
        provider_cost_usd: None,
        finish_reason: Some("stop".into()),
        decision_probabilities: Some(Value::Object(answers.clone())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> CompleteRequest {
        serde_json::from_value(json!({"step":"ricochet_decide","categories":["decision"],
            "messages":[{"role":"system","content":"Select the next citation."},{"role":"user","content":"article"}],
            "response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object","properties":{
                "next_step":{"type":"string","enum":["OPEN:4","SEARCH","STOP"]},"enough_evidence":{"type":"boolean"}
            },"required":["next_step","enough_evidence"]}}}})).unwrap()
    }
    #[test]
    fn finite_decisions_round_trip_and_preserve_probabilities() {
        let p = payload(&request(), "jev-latest").unwrap();
        assert_eq!(
            p["questions"]["next_step"]["criteria"]["option_0"]["value"],
            "OPEN:4"
        );
        assert!(p["questions"]["next_step"]["instructions"]
            .to_string()
            .contains("citation"));
        let body = json!({"answers":{"next_step":{"type":"choice","choice":"option_0","confidence":0.9,"probabilities":{"option_0":0.9}},"enough_evidence":{"type":"noul","noul":0.2}},"usage":{"input_tokens":100,"output_tokens":4}});
        let parsed = parse(&body, &p).unwrap();
        let out: Value = serde_json::from_str(&parsed.text).unwrap();
        assert_eq!(out, json!({"next_step":"OPEN:4","enough_evidence":false}));
        assert_eq!(parsed.prompt_tokens, 100);
        assert!(parsed.decision_probabilities.is_some());
    }
    #[test]
    fn rejects_free_text_tools_and_unknown_choices() {
        let mut r = request();
        r.response_format.as_mut().unwrap()["json_schema"]["schema"]["properties"]["next_step"] =
            json!({"type":"string"});
        assert!(payload(&r, "jev-latest").is_err());
        let mut r = request();
        r.tools = Some(vec![]);
        assert!(payload(&r, "jev-latest").is_err());
        let p = payload(&request(), "jev-latest").unwrap();
        assert!(parse(&json!({"answers":{"next_step":{"choice":"made-up"}}}), &p).is_err());
    }
}
