//! Shared structured-payload decoding and repair, without runtime services.

/// Preserve invalid arguments as submitted instead of executing Rig's partial
/// object projection. Typed tools and submissions can then reject or repair them.
pub fn call_arguments(function: &rig_core::completion::message::ToolFunction) -> serde_json::Value {
    match &function.invalid_arguments {
        Some(raw) => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.clone()))
        }
        None => function.arguments_value(),
    }
}

/// Turn a `submit` call's arguments into `T`, tolerating one spurious wrapper level, and
/// on failure describing the payload rather than just naming a field.
///
/// **The wrapper.** Models sometimes nest the answer one level down -
/// `{"result": {"classes": [...]}}` - measured at 4 of 43 submits against DeepSeek. The
/// schema is not consulted to detect it: `T` is already known, so each candidate is simply
/// handed to serde and the first that deserializes wins. That needs no list of blessed
/// wrapper names and cannot mis-read a real payload, because the only thing deciding is
/// `T`'s own impl. Only a **single-key** object is a candidate, so an envelope is
/// distinguishable from an answer that happens to have one field.
///
/// **The message.** `missing field \`classes\`` is true of the value serde was handed and
/// actively misleading about the value the model *sent*: it did include `classes`, one
/// level down. Observed consequence - the model answers
/// *"the error says missing field `classes` but I clearly included `classes`"*, goes looking
/// for a fault inside `classes`, finds none, and burns the turn budget. Naming the keys
/// that were actually present is what makes the failure self-diagnosing.
pub fn deserialize_submission<T>(args: serde_json::Value) -> Result<T, String>
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned,
{
    let validator = submission_validator::<T>();
    decode_submission(&args, validator.as_ref(), false)
}

/// The SDK requires its declared schema as well as Rust decoding. Legacy
/// callers retain their existing serde-only acceptance through the same repairs.
pub(crate) fn decode_submission<T: serde::de::DeserializeOwned>(
    args: &serde_json::Value,
    validator: Option<&jsonschema::Validator>,
    enforce_schema: bool,
) -> Result<T, String> {
    let decode = |value: &serde_json::Value| -> Result<T, String> {
        let decoded = serde_json::from_value::<T>(value.clone()).map_err(|e| e.to_string())?;
        if enforce_schema && let Some(error) = validator.and_then(|v| v.iter_errors(value).next()) {
            return Err(error.to_string());
        }
        Ok(decoded)
    };
    let first = match decode(args) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    if let Some(inner) = args
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.values().next())
        && let Ok(v) = decode(inner)
    {
        let key = args
            .as_object()
            .and_then(|o| o.keys().next())
            .cloned()
            .unwrap_or_default();
        tracing::debug!(wrapper = %key, "unwrapped a submission nested one level down");
        return Ok(v);
    }
    if let Some(repaired) = validator.and_then(|validator| repair_json_text_fields(args, validator))
        && let Ok(v) = decode(&repaired)
    {
        tracing::debug!("decoded JSON text in structured submission fields");
        return Ok(v);
    }
    Err(format!("{first}{}", describe_payload(args, validator)))
}

fn submission_validator<T: schemars::JsonSchema>() -> Option<jsonschema::Validator> {
    let schema = serde_json::to_value(schemars::schema_for!(T)).ok()?;
    jsonschema::validator_for(&schema).ok()
}

/// Decode model-authored arrays or objects that arrived as JSON inside a string. A schema type
/// error selects the field, and the complete repaired payload must still deserialize as `T`.
fn repair_json_text_fields(
    args: &serde_json::Value,
    validator: &jsonschema::Validator,
) -> Option<serde_json::Value> {
    let repairs = validator
        .iter_errors(args)
        .filter_map(|error| {
            let jsonschema::error::ValidationErrorKind::Type { kind } = error.kind() else {
                return None;
            };
            let serde_json::Value::String(text) = error.instance().as_ref() else {
                return None;
            };
            let parsed = serde_json::from_str::<serde_json::Value>(text).ok()?;
            let parsed_type = match parsed {
                serde_json::Value::Array(_) => jsonschema::JsonType::Array,
                serde_json::Value::Object(_) => jsonschema::JsonType::Object,
                _ => return None,
            };
            let expected_type = match kind {
                jsonschema::error::TypeKind::Single(expected) => *expected == parsed_type,
                jsonschema::error::TypeKind::Multiple(expected) => expected.contains(parsed_type),
            };
            expected_type.then(|| (error.instance_path().to_string(), parsed))
        })
        .collect::<Vec<_>>();
    if repairs.is_empty() {
        return None;
    }

    let mut repaired = args.clone();
    for (path, value) in repairs {
        *repaired.pointer_mut(&path)? = value;
    }
    Some(repaired)
}

/// A short, factual description of what the model actually sent, appended to a serde error.
fn describe_payload(args: &serde_json::Value, validator: Option<&jsonschema::Validator>) -> String {
    match args {
        serde_json::Value::Object(o) if o.is_empty() => " - you sent an empty object".into(),
        serde_json::Value::Object(o) => {
            let keys: Vec<&str> = o.keys().map(String::as_str).collect();
            let Some(error) = validator.and_then(|validator| validator.iter_errors(args).next())
            else {
                return format!(
                    " - the object you sent has these keys: [{}].",
                    keys.join(", ")
                );
            };
            let path = error.instance_path().to_string();
            if path.is_empty()
                && matches!(
                    error.kind(),
                    jsonschema::error::ValidationErrorKind::Required { .. }
                )
            {
                format!(
                    " - the object you sent has these keys: [{}]. Put the required fields at the \
                     TOP level of the `submit` arguments, not nested inside another key.",
                    keys.join(", ")
                )
            } else if path.is_empty() {
                format!(
                    " - the submit arguments do not match the schema: {error}. The object you \
                     sent has these keys: [{}].",
                    keys.join(", ")
                )
            } else {
                format!(" - field `{path}` does not match the submit schema: {error}.")
            }
        }
        other => format!(
            " - you sent a {}, but `submit` takes an object whose keys are the required fields.",
            match other {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => unreachable!("handled above"),
            }
        ),
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Deserialize, schemars::JsonSchema)]
    struct Classification {
        classes: Vec<String>,
        #[serde(default)]
        relations: Vec<String>,
    }

    /// The measured failure: the answer is one level down under a key the model invented.
    #[test]
    fn a_submission_nested_under_a_wrapper_key_is_unwrapped() {
        for key in [
            "result",
            "data",
            "output",
            "classification",
            "anything_at_all",
        ] {
            let args = serde_json::json!({ key: { "classes": ["schema:Person"] } });
            let got: Classification = deserialize_submission(args).expect(key);
            assert_eq!(
                got.classes,
                ["schema:Person"],
                "wrapper `{key}` not unwrapped"
            );
        }
    }

    /// No allow-list of wrapper names: `T` deciding is the whole mechanism, so a key nobody
    /// anticipated works exactly as well as `result`.
    #[test]
    fn a_correct_submission_is_taken_as_is() {
        let args = serde_json::json!({ "classes": ["schema:Person"], "relations": ["a"] });
        let got: Classification = deserialize_submission(args).unwrap();
        assert_eq!(
            got,
            Classification {
                classes: vec!["schema:Person".into()],
                relations: vec!["a".into()]
            }
        );
    }

    /// A single-key object that *is* the answer must not be mistaken for an envelope. Here
    /// the top level deserializes, so unwrapping is never attempted.
    #[test]
    fn a_one_field_answer_is_not_mistaken_for_a_wrapper() {
        let args = serde_json::json!({ "classes": ["schema:Person"] });
        let got: Classification = deserialize_submission(args).unwrap();
        assert_eq!(got.classes, ["schema:Person"]);
        assert!(got.relations.is_empty());
    }

    /// Unwrapping is one level only, and only when the inner value actually deserializes -
    /// otherwise a wrong guess would be reported as success.
    #[test]
    fn a_wrapper_whose_contents_are_wrong_is_still_an_error() {
        let args = serde_json::json!({ "result": { "not_classes": [] } });
        let err = deserialize_submission::<Classification>(args).expect_err("must not succeed");
        assert!(err.contains("classes"), "{err}");
    }

    #[test]
    fn double_wrapping_is_not_unwrapped() {
        let args = serde_json::json!({ "a": { "b": { "classes": [] } } });
        assert!(deserialize_submission::<Classification>(args).is_err());
    }

    /// The diagnosis. `missing field \`classes\`` alone sent the model hunting inside
    /// `classes`; the keys it actually sent are what let it find the real fault.
    #[test]
    fn the_error_names_the_keys_the_model_actually_sent() {
        let args = serde_json::json!({ "result": { "not_classes": [] } });
        let err = deserialize_submission::<Classification>(args).unwrap_err();
        assert!(err.contains("[result]"), "names the offending key: {err}");
        assert!(err.contains("TOP level"), "says what to do about it: {err}");
    }

    #[test]
    fn the_error_calls_out_a_non_object_payload_by_type() {
        let err = deserialize_submission::<Classification>(serde_json::json!([1, 2])).unwrap_err();
        assert!(err.contains("array"), "{err}");
        let err = deserialize_submission::<Classification>(serde_json::json!("hi")).unwrap_err();
        assert!(err.contains("string"), "{err}");
    }

    #[test]
    fn an_empty_object_is_reported_as_empty() {
        let err = deserialize_submission::<Classification>(serde_json::json!({})).unwrap_err();
        assert!(err.contains("empty object"), "{err}");
    }
}
