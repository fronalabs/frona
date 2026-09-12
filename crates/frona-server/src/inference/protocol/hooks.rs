use serde_json::{Map, Value};

#[derive(Clone)]
pub struct RequestParams {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub additional_params: Option<Value>,
}

pub type RequestHook = fn(RequestParams) -> RequestParams;

/// gpt-5/o-series reject `max_tokens` outright; only `max_completion_tokens`
/// is accepted across all current OpenAI models.
pub fn openai(mut p: RequestParams) -> RequestParams {
    if let Some(mt) = p.max_tokens.take() {
        let mut root = take_object(&mut p.additional_params);
        root.entry("max_completion_tokens".to_string())
            .or_insert_with(|| Value::Number(mt.into()));
        p.additional_params = Some(Value::Object(root));
    }
    p
}

/// Legacy hook for callers without Frona's native transport. Rig now handles
/// common Ollama parameters itself; retain this hook for its existing callers.
pub fn ollama(mut p: RequestParams) -> RequestParams {
    if let Some(mt) = p.max_tokens.take() {
        let mut root = take_object(&mut p.additional_params);
        let mut options = match root.remove("options") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        options
            .entry("num_predict".to_string())
            .or_insert_with(|| Value::Number(mt.into()));
        root.insert("options".to_string(), Value::Object(options));
        p.additional_params = Some(Value::Object(root));
    }
    p
}

fn take_object(slot: &mut Option<Value>) -> Map<String, Value> {
    match slot.take() {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(max_tokens: Option<u64>, additional: Option<Value>) -> RequestParams {
        RequestParams {
            max_tokens,
            temperature: None,
            additional_params: additional,
        }
    }

    #[test]
    fn openai_moves_max_tokens_to_max_completion_tokens() {
        let p = openai(params(Some(64000), None));
        assert!(p.max_tokens.is_none(), "max_tokens should be cleared");
        assert_eq!(
            p.additional_params,
            Some(json!({"max_completion_tokens": 64000})),
        );
    }

    #[test]
    fn openai_merges_with_existing_additional_params() {
        let p = openai(params(
            Some(64000),
            Some(json!({"reasoning_effort": "high"})),
        ));
        assert!(p.max_tokens.is_none());
        assert_eq!(
            p.additional_params,
            Some(json!({
                "reasoning_effort": "high",
                "max_completion_tokens": 64000
            })),
        );
    }

    #[test]
    fn openai_preserves_user_supplied_max_completion_tokens() {
        let p = openai(params(
            Some(8000),
            Some(json!({"max_completion_tokens": 64000})),
        ));
        assert!(p.max_tokens.is_none());
        assert_eq!(
            p.additional_params,
            Some(json!({"max_completion_tokens": 64000})),
        );
    }

    #[test]
    fn openai_skips_when_max_tokens_is_none() {
        let p = openai(params(None, Some(json!({"reasoning_effort": "low"}))));
        assert!(p.max_tokens.is_none());
        assert_eq!(
            p.additional_params,
            Some(json!({"reasoning_effort": "low"}))
        );
    }

    #[test]
    fn ollama_nests_max_tokens_as_num_predict_under_options() {
        let p = ollama(params(Some(8192), None));
        assert!(p.max_tokens.is_none());
        assert_eq!(
            p.additional_params,
            Some(json!({"options": {"num_predict": 8192}})),
        );
    }

    #[test]
    fn ollama_merges_with_existing_options() {
        let p = ollama(params(
            Some(8192),
            Some(json!({"options": {"num_ctx": 32768}, "think": true})),
        ));
        assert!(p.max_tokens.is_none());
        assert_eq!(
            p.additional_params,
            Some(json!({
                "options": {"num_ctx": 32768, "num_predict": 8192},
                "think": true,
            })),
        );
    }

    #[test]
    fn ollama_preserves_user_supplied_num_predict() {
        let p = ollama(params(
            Some(8192),
            Some(json!({"options": {"num_predict": 4096}})),
        ));
        assert!(p.max_tokens.is_none());
        assert_eq!(
            p.additional_params,
            Some(json!({"options": {"num_predict": 4096}})),
        );
    }
}
