#[path = "../examples/support/mod.rs"]
mod support;
use frona_harness::tools::{
    ToolContext, ToolDispatchError, ToolError, ToolOutput, ToolRegistry, tool,
};
use frona_harness::{
    ActionResolution, BudgetLimits, Conversation, DefaultBudget, ExecutionControl, ExecutionStop,
    ModelOutput, UsageReport,
};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

static CALLS: AtomicUsize = AtomicUsize::new(0);

/// Add two numbers.
#[tool]
async fn add(left: i64, right: i64) -> Result<i64, ToolError> {
    Ok(left + right)
}

/// Return the current call identifier.
#[tool]
async fn identify(
    prefix: String,
    execution: frona_harness::ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    assert!(execution.execution().is_ok());
    Ok(ToolOutput::text(format!(
        "{prefix}:{}",
        execution.call_id.as_deref().unwrap()
    )))
}

#[tool(name = "counter", description = "Increment the invocation counter")]
async fn count() -> Result<usize, ToolError> {
    Ok(CALLS.fetch_add(1, Ordering::SeqCst))
}

#[tool]
async fn stop(execution: &ToolContext<'_>, reason: String) -> Result<(), ToolError> {
    assert!(execution.execution().is_ok());
    Err(ExecutionStop::Stopped(reason).into())
}

fn record(
    conversation: &mut Conversation,
    registry: &ToolRegistry<'_>,
    name: &str,
    args: Value,
) -> frona_harness::PendingAction {
    conversation
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    "actual-call",
                    ToolFunction::new(name.try_into().unwrap(), args),
                ))],
                UsageReport::Unavailable,
            ),
            registry.definitions().cloned().collect(),
        )
        .unwrap()
        .actions
        .remove(0)
}

#[tokio::test]
async fn function_items_keep_normal_calls_and_named_object_schemas() {
    assert_eq!(add(2, 3).await.unwrap(), 5);
    let mut tools = ToolRegistry::default();
    tools.register(add).unwrap();
    let definition = tools.definitions().next().unwrap();
    assert_eq!(definition.name, "add");
    assert_eq!(definition.description, "Add two numbers.");
    assert_eq!(definition.parameters["type"], "object");
    assert!(definition.parameters["properties"].get("left").is_some());
    assert!(definition.parameters["properties"].get("right").is_some());
    let mut c = Conversation::new("add");
    let a = record(&mut c, &tools, "add", json!({"left":3,"right":4}));
    let b = DefaultBudget::default();
    let ActionResolution::Result(output) = c
        .execute_tool(&a, &mut tools, ExecutionControl::new(&b))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        output.content[0],
        frona_harness::ToolContent::Json { value: json!(7) }
    );
    assert_eq!(b.snapshot().tool_calls, 1);
}

#[tokio::test]
async fn injected_context_is_excluded_and_is_the_actual_dispatch_context() {
    let mut tools = ToolRegistry::default();
    tools.register(identify).unwrap();
    assert_eq!(
        tools.definitions().next().unwrap().parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["prefix"]
    );
    let mut c = Conversation::new("identify");
    let a = record(&mut c, &tools, "identify", json!({"prefix":"hello"}));
    let b = DefaultBudget::default();
    let ActionResolution::Result(output) = c
        .execute_tool(&a, &mut tools, ExecutionControl::new(&b))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(output, ToolOutput::text("hello:actual-call"));
}

#[tokio::test]
async fn registration_validation_and_refused_admission_do_not_invoke_functions() {
    CALLS.store(0, Ordering::SeqCst);
    let mut tools = ToolRegistry::default();
    tools.register(count).unwrap();
    assert!(tools.register(count).is_err());
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    let mut c = Conversation::new("count");
    let a = record(&mut c, &tools, "counter", json!({}));
    let b = DefaultBudget::new(BudgetLimits {
        tool_calls: Some(0),
        ..Default::default()
    });
    assert!(matches!(
        c.execute_tool(&a, &mut tools, ExecutionControl::new(&b))
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    let mut c = Conversation::new("invalid");
    let a = record(&mut c, &tools, "counter", json!("not an object"));
    assert!(matches!(
        c.execute_tool(
            &a,
            &mut tools,
            ExecutionControl::new(&DefaultBudget::default())
        )
        .await
        .unwrap(),
        ActionResolution::Failed(_)
    ));
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn borrowed_context_and_typed_stops_preserve_shared_execution_semantics() {
    let mut tools = ToolRegistry::default();
    tools.register(stop).unwrap();
    let mut c = Conversation::new("stop");
    let a = record(&mut c, &tools, "stop", json!({"reason":"done"}));
    assert!(
        matches!(c.execute_tool(&a,&mut tools,ExecutionControl::new(&DefaultBudget::default())).await,Err(ToolDispatchError::Stop(ExecutionStop::Stopped(reason))) if reason=="done")
    );
}

#[tokio::test]
async fn registration_calls_the_supplied_function_value_instead_of_a_static_original() {
    let invocations = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = invocations.clone();
    let actual = move |left, right| {
        seen.fetch_add(1, Ordering::SeqCst);
        add(left + 10, right)
    };
    let mut tools = ToolRegistry::default();
    tools.register(actual).unwrap();
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
    let mut c = Conversation::new("wrapper");
    let a = record(&mut c, &tools, "add", json!({"left":1,"right":2}));
    let ActionResolution::Result(output) = c
        .execute_tool(
            &a,
            &mut tools,
            ExecutionControl::new(&DefaultBudget::default()),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        output.content[0],
        frona_harness::ToolContent::Json { value: json!(13) }
    );
}

/// Research using the actual shared controls.
#[tool]
async fn nested(execution: ToolContext<'_>, question: String) -> Result<ToolOutput, ToolError> {
    let model = support::model(vec![support::text("nested answer")])?;
    execution
        .complete(&model, frona_harness::ModelRequest::new(question))
        .await?;
    Ok(ToolOutput::text("done"))
}

#[tokio::test]
async fn annotated_nested_calls_share_budget_and_cancellation_is_checked_before_invocation() {
    let mut tools = ToolRegistry::default();
    tools.register(nested).unwrap();
    let mut c = Conversation::new("nested");
    let a = record(&mut c, &tools, "nested", json!({"question":"q"}));
    let b = DefaultBudget::new(BudgetLimits {
        model_attempts: Some(0),
        ..Default::default()
    });
    assert!(matches!(
        c.execute_tool(&a, &mut tools, ExecutionControl::new(&b))
            .await,
        Err(ToolDispatchError::Stop(ExecutionStop::BudgetExhausted(_)))
    ));
    assert_eq!(b.snapshot().tool_calls, 1);
    assert_eq!(b.snapshot().model_attempts, 0);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let b = DefaultBudget::default();
    let mut c = Conversation::new("cancel");
    let a = record(&mut c, &tools, "nested", json!({"question":"q"}));
    assert!(matches!(
        c.execute_tool(
            &a,
            &mut tools,
            ExecutionControl::new(&b).with_cancellation(&token)
        )
        .await,
        Err(ToolDispatchError::Stop(ExecutionStop::Cancelled(_)))
    ));
    assert_eq!(b.snapshot().tool_calls, 0);
    assert_eq!(b.snapshot().model_attempts, 0);
}

#[tokio::test]
async fn function_references_and_pointers_register_without_generated_wrapper_names() {
    let pointer: fn(i64, i64) -> _ = add;
    let mut tools = ToolRegistry::default();
    tools.register(pointer).unwrap();
    let mut other = ToolRegistry::default();
    let reference = &add;
    other.register(reference).unwrap();
    let harness = frona_harness::Harness::new(
        support::model(vec![
            support::output(vec![support::call(
                "sum",
                "add",
                json!({"left":2,"right":3}),
            )]),
            support::text("five"),
        ])
        .unwrap(),
    );
    assert_eq!(harness.run("add").tool(add).await.unwrap(), "five");
}

#[tool(name = "submit")]
async fn reserved() -> Result<(), ToolError> {
    panic!("registration must not call the function")
}

#[tool]
async fn unavailable(path: String) -> std::io::Result<String> {
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, path))
}

#[tokio::test]
async fn registration_errors_and_application_error_types_keep_existing_meanings() {
    assert!(matches!(ToolRegistry::default().register(reserved),
        Err(frona_harness::ToolRegistrationError::ReservedName(name)) if name == "submit"));
    assert_eq!(
        unavailable("record".into()).await.unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let mut tools = ToolRegistry::default();
    tools.register(unavailable).unwrap();
    let mut conversation = Conversation::new("read");
    let action = record(
        &mut conversation,
        &tools,
        "unavailable",
        json!({"path":"record"}),
    );
    assert!(matches!(
        conversation
            .execute_tool(
                &action,
                &mut tools,
                ExecutionControl::new(&DefaultBudget::default())
            )
            .await
            .unwrap(),
        ActionResolution::Failed(_)
    ));
}

mod hygiene {
    use frona_harness::tool;

    pub type F = String;
    pub struct Box;
    pub struct FnMut;
    pub struct Send;

    /// Preserve names from the caller's lexical scope.
    #[tool]
    pub async fn echo(value: F) -> Result<F, frona_harness::ToolError> {
        Ok(value)
    }
}

/// Function-only conditional attributes must not decorate generated types.
#[tool]
#[cfg_attr(all(), inline)]
async fn conditionally_inlined(value: String) -> Result<String, ToolError> {
    Ok(value)
}

// Nested gating must also gate metadata and argument types. The unavailable
// type is deliberately absent from this crate when the function is disabled.
#[tool]
#[cfg_attr(all(), cfg_attr(all(), cfg(any()), inline))]
async fn conditionally_absent(value: UnavailableType) -> Result<UnavailableType, ToolError> {
    Ok(value)
}

#[tokio::test]
async fn generated_items_preserve_caller_names_and_function_attribute_targets() {
    let _shadowed_names = (hygiene::Box, hygiene::FnMut, hygiene::Send);
    assert_eq!(hygiene::echo("direct".into()).await.unwrap(), "direct");
    let mut tools = ToolRegistry::default();
    tools.register(hygiene::echo).unwrap();
    tools.register(conditionally_inlined).unwrap();
    let mut conversation = Conversation::new("echo");
    let action = record(
        &mut conversation,
        &tools,
        "echo",
        json!({"value":"dispatched"}),
    );
    let ActionResolution::Result(output) = conversation
        .execute_tool(
            &action,
            &mut tools,
            ExecutionControl::new(&DefaultBudget::default()),
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        output.content[0],
        frona_harness::ToolContent::Json {
            value: json!("dispatched")
        }
    );
}
