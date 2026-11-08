use frona_harness as sdk;

/// Double a value using an annotated function through the renamed SDK.
#[sdk::tool]
async fn doubled(value: i64, _context: sdk::ToolContext<'_>) -> Result<i64, sdk::ToolError> {
    Ok(value * 2)
}

#[tokio::test]
async fn annotated_function_uses_the_renamed_dependency() {
    assert_eq!(doubled(3, sdk::ToolContext::default()).await.unwrap(), 6);
    let mut registry = sdk::ToolRegistry::default();
    registry.register(doubled).unwrap();
    let definition = registry.definitions().next().unwrap();
    assert_eq!(definition.name, "doubled");
    assert_eq!(
        definition.parameters["properties"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
}
