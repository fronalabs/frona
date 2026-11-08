//! An external renamed consumer with no direct serde/schemars/proc-macro dependencies.

pub mod application {
    pub type F = String;
    pub struct Box;
    pub struct FnMut;
    pub struct Send;

    #[sdk::tool]
    #[cfg_attr(all(), inline)]
    pub async fn echo(value: F) -> Result<F, sdk::ToolError> {
        Ok(value)
    }

    #[sdk::tool]
    #[cfg_attr(all(), cfg_attr(all(), cfg(any()), inline))]
    pub async fn disabled(value: Missing) -> Result<Missing, sdk::ToolError> {
        Ok(value)
    }

    /// Add two values, with context supplied by the harness.
    #[sdk::tool]
    pub async fn add(
        left: i64,
        execution: sdk::ToolContext<'_>,
        right: i64,
    ) -> Result<i64, sdk::ToolError> {
        let _ = execution.call_id;
        Ok(left + right)
    }
}

#[cfg(test)]
mod tests {
    use super::application::add;

    #[tokio::test]
    async fn renamed_macro_and_derived_arguments_need_only_the_harness_dependency() {
        assert_eq!(add(2, sdk::ToolContext::default(), 3).await.unwrap(), 5);
        let mut hygiene = sdk::ToolRegistry::default();
        hygiene.register(super::application::echo).unwrap();
        assert_eq!(
            super::application::echo("same type".into()).await.unwrap(),
            "same type"
        );
        let mut tools = sdk::ToolRegistry::default();
        tools.register(add).unwrap();
        let schema = &tools.definitions().next().unwrap().parameters;
        assert_eq!(schema["properties"].as_object().unwrap().len(), 2);
        let output = tools
            .get_mut("add")
            .unwrap()
            .call(
                "{\"left\":2,\"right\":3}".parse().unwrap(),
                &sdk::ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(output.content.len(), 1);
    }
}
