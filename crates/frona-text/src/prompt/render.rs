use std::collections::HashMap;
use std::sync::Arc;

use minijinja::value::{Enumerator, Object, ObjectRepr, Value};
use minijinja::{Environment, UndefinedBehavior};

use super::PromptError;

#[derive(Debug)]
struct CaseInsensitiveContext {
    data: HashMap<String, Value>,
}

impl Object for CaseInsensitiveContext {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let key_str = key.as_str()?.to_lowercase();
        self.data.get(&key_str).cloned()
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let keys: Vec<Value> = self.data.keys().map(|k| Value::from(k.clone())).collect();
        Enumerator::Values(keys)
    }
}

/// Render a template with strict, case-insensitive variable lookup.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> Result<String, PromptError> {
    if vars.is_empty()
        && !template.contains("{{")
        && !template.contains("{%")
        && !template.contains("{#")
    {
        return Ok(template.to_string());
    }

    let data: HashMap<String, Value> = vars
        .iter()
        .map(|(k, v)| (k.to_lowercase(), Value::from(*v)))
        .collect();

    let ctx = CaseInsensitiveContext { data };

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);

    env.render_str(template, Value::from_object(ctx))
        .map_err(|e| PromptError::Render {
            name: None,
            source: e,
        })
}
