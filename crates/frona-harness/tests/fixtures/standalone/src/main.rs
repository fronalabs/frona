use frona_harness::{
    Decision, Harness, Model, ModelOutput, ModelRequest, Progress, RunOutcome, ToolContext,
    ToolDecision, ToolRegistry, UsageReport, tool, workspace_read_tools,
};
use frona_standalone_consumer::{Answer as Arguments, ApplicationClient};
use rig_core::completion::{
    AssistantContent,
    message::{ToolCall, ToolFunction},
};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The production builder prepares a client without network calls or identities.
    let _prepared = Model::builder("openai", "application-model")
        .api_key("fixture-key")
        .build()?;
    let application_state = String::from("application response");
    let model = Model::from_adapter(
        "application",
        "local",
        ApplicationClient(&application_state),
    )?;
    // Owned conversation handoff keeps borrowed model clients usable on resume.
    let mut saved = Harness::new(model.clone()).run("resume input").manual();
    saved.call_model().await?;
    let resumed = Harness::new(model.clone())
        .resume(saved.into_conversation())
        .await?;
    assert_eq!(resumed, application_state);

    let output = model.complete(ModelRequest::new("hello")).await?;
    assert!(matches!(output.usage, UsageReport::Unavailable));
    assert_eq!(output.content.len(), 1);

    // Pure compaction planning and view construction need no database or identity.
    let history = vec![
        rig_core::completion::Message::user("old request ".repeat(100)),
        rig_core::completion::Message::assistant("old answer ".repeat(100)),
        rig_core::completion::Message::user("active request"),
    ];
    let view = frona_harness::HistoryView::new(&history);
    let plan = view
        .plan(
            "",
            &[],
            frona_harness::CompactionLimits {
                available_input_tokens: 200,
                target_input_tokens: 50,
            },
        )?
        .expect("older history can be summarized");
    let compacted = plan.finish("old facts", Some("frozen context"))?;
    let view = frona_harness::HistoryView::from_compacted(&history, &compacted)?;
    assert_eq!(view.original_positions(), [2]);
    assert_eq!(view.messages().last(), history.last());
    assert_eq!(view.originals(), history.as_slice());

    let harness = Harness::new(model);
    // Default memory transitions need no history/checkpoint configuration.
    let mut memory = harness.run("history without storage setup").manual();
    memory.call_model().await?;
    assert_eq!(memory.finish()?, "application response");
    assert!(matches!(
        memory.conversation().history().last().map(|r| &r.event),
        Some(frona_harness::HistoryEvent::Finished { .. })
    ));
    assert!(memory.conversation().history_failure().is_none());
    // Pure JSONL reconstruction works in this separate application with no
    // SurrealDB feature, credentials, storage backend, or runtime restoration.
    let encoded = frona_harness::encode_jsonl(memory.conversation().history())?;
    let replayed = frona_harness::decode_jsonl(&encoded)?;
    assert_eq!(
        replayed.replay.conversation.messages(),
        memory.conversation().messages()
    );
    assert_eq!(
        replayed.replay.conversation.status(),
        frona_harness::ConversationStatus::Finished
    );
    assert!(replayed.incomplete_tail.is_none());
    assert_eq!(
        frona_harness::encode_jsonl(replayed.replay.conversation.history())?,
        encoded
    );

    let run = harness.run("observed standalone call");
    let report = run.report();
    let mut progress = run.subscribe();
    let (result, updates) = tokio::join!(async { run.await }, async {
        let mut updates = Vec::new();
        while let Some(update) = progress.recv().await {
            updates.push(update);
        }
        updates
    });
    assert_eq!(result?, application_state);
    assert!(matches!(&updates[..], [Progress::Text { text, .. }] if text == &application_state));
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));
    assert_eq!(
        harness.run("automatic standalone call").await?,
        application_state
    );
    let answer: Arguments = harness.run("typed standalone call").structured().await?;
    assert_eq!(answer.value, application_state);
    let mut inspections = 0;
    let run = harness
        .run("inspected standalone call")
        .structured::<Arguments>();
    let report = run.report();
    let accepted: String = run
        .inspect(async |answer| {
            inspections += 1;
            assert_eq!(report.snapshot().logical_calls(), 1);
            assert_eq!(report.snapshot().provider_attempts(), 1);
            assert_eq!(report.snapshot().outcome, None);
            tokio::task::yield_now().await;
            Ok::<_, ()>(Decision::accept(answer.value.to_uppercase()))
        })
        .await?;
    assert_eq!(accepted, application_state.to_uppercase());
    assert_eq!(inspections, 1);
    assert_eq!(report.snapshot().outcome, Some(RunOutcome::Completed));
    assert_eq!(
        report.snapshot().attempts[0].usage,
        Some(UsageReport::Unavailable)
    );
    let mut invoked = false;
    assert_eq!(
        harness
            .run("hooked standalone call")
            .tool(tool("echo", "Echo a value", |args: Arguments| {
                invoked = true;
                async move { Ok(args.value) }
            })?)
            .before_tool(|call| async move {
                assert_eq!(call.function.name, "echo");
                Ok::<_, ()>(ToolDecision::Reject("Not needed for this fixture".into()))
            })
            .await?,
        application_state
    );
    assert!(!invoked);
    let mut session = harness.run("manual standalone call").manual();
    assert!(session.call_model().await?.actions.is_empty());
    assert_eq!(session.finish()?, application_state);

    // An actual standalone run discovers and reads files with only an explicit root.
    let workspace = tempfile::tempdir()?;
    std::fs::write(workspace.path().join("notes.txt"), "standalone notes")?;
    let bundle_run = harness
        .run("Find and read the notes")
        .tools(workspace_read_tools(workspace.path())?);
    let bundle_report = bundle_run.report();
    assert_eq!(bundle_run.await?, application_state);
    assert_eq!(bundle_report.snapshot().actions.len(), 2);

    // An actual borrowed adapter runs summaries under the same fluent refinement
    // loop; no durable backend or identity setup is needed.
    let run = harness
        .run("Refine the evidence")
        .structured::<Arguments>()
        .compaction(frona_harness::CompactionConfig {
            context_window: 1200,
            max_output_tokens: Some(32),
            ..Default::default()
        });
    let report = run.report();
    let mut rounds = 0;
    let result = run
        .inspect(async |answer| {
            rounds += 1;
            Ok::<_, ()>(if rounds < 4 {
                Decision::revise("more evidence ".repeat(150))
            } else {
                Decision::accept(answer.value)
            })
        })
        .await?;
    assert_eq!(result, application_state);
    assert_eq!(rounds, 4);
    assert!(report.snapshot().provider_attempts() > rounds);

    let mut tools = ToolRegistry::default();
    tools.register(tool(
        "echo",
        "Echo an input",
        |args: Arguments| async move { Ok(json!({"echo": args.value})) },
    )?)?;
    let result = tools
        .get_mut("echo")
        .unwrap()
        .call(json!({"value": "hello"}), &ToolContext::default())
        .await?;
    assert!(!result.content.is_empty());
    let mut interrupted = frona_harness::Conversation::new("recover this");
    let action = interrupted
        .record_response(
            ModelOutput::new(
                vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    "recovery-echo",
                    ToolFunction::new("echo".into(), json!({"value": "recover"})),
                ))],
                UsageReport::Unavailable,
            ),
            tools.definitions().cloned().collect(),
        )?
        .actions
        .remove(0);
    interrupted.start(&action)?;
    let bytes = frona_harness::encode_jsonl(interrupted.history())?;
    let mut restored = frona_harness::decode_jsonl(&bytes)?.replay.conversation;
    let budget = frona_harness::DefaultBudget::default();
    assert!(
        restored
            .recover_tools(&mut tools, frona_harness::ExecutionControl::new(&budget))
            .await?
            .actions
            .is_empty()
    );
    assert_eq!(restored.responses()[0].actions()[0].invocation_ordinal(), 2);
    assert_eq!(budget.snapshot().tool_calls, 1);
    #[cfg(feature = "jsonl")]
    {
        use frona_harness::{Conversation, Jsonl};
        use rig_core::completion::message::UserContent;
        let dir = tempfile::tempdir()?;
        let resumed_path = dir.path().join("fluent.jsonl");
        let mut saved_run = harness
            .run("saved typed result")
            .jsonl(Jsonl::create(&resumed_path)?)
            .structured::<Arguments>()
            .manual();
        saved_run.call_model().await?;
        saved_run.save_checkpoint("consumer", 1, &"application state")?;
        drop(saved_run);
        let loaded_run = Jsonl::open(&resumed_path)?;
        assert_eq!(
            loaded_run
                .load_checkpoint::<String>("consumer", 1)?
                .unwrap()
                .payload,
            "application state"
        );
        let resumed_result = harness.resume(loaded_run).structured::<Arguments>().await?;
        assert_eq!(resumed_result.value, application_state);

        let path = dir.path().join("history.jsonl");
        let mut conversation = Conversation::new("save this");
        conversation.attach_jsonl(Jsonl::create(&path)?)?;
        conversation.push_user(vec![UserContent::text("explicit feedback")])?;
        let saved = conversation.save_checkpoint("consumer-loop", 1, &vec!["candidate"])?;
        drop(conversation);
        let loaded = Jsonl::open(&path)?;
        assert_eq!(loaded.conversation().messages().len(), 2);
        let checkpoint = loaded
            .load_checkpoint::<Vec<String>>("consumer-loop", 1)?
            .unwrap();
        assert_eq!(checkpoint.seq, saved);
        assert_eq!(checkpoint.payload, ["candidate"]);
        let mut writable = loaded.into_writable()?;
        writable.finish()?;
        assert!(matches!(
            writable.load_checkpoint::<Vec<String>>("consumer-loop", 1),
            Err(frona_harness::CheckpointLoadError::Stale { .. })
        ));
        writable.save_checkpoint(
            "consumer-loop",
            1,
            &vec!["application state after completion"],
        )?;
        drop(writable);
        let completed = Jsonl::open(&path)?;
        assert_eq!(completed.conversation().history().len(), 5);
        assert_eq!(
            completed
                .load_checkpoint::<Vec<String>>("consumer-loop", 1)?
                .unwrap()
                .payload,
            ["application state after completion"]
        );
    }
    Ok(())
}
