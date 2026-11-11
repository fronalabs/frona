use std::collections::BTreeSet;

fn resource(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join(name),
    )
    .unwrap_or_else(|error| panic!("missing prompt {name}: {error}"))
}

#[test]
fn every_structured_stage_explicitly_calls_submit() {
    for stage in [
        "ingest",
        "reconcile",
        "playbook-resolve",
        "playbook-author",
        "classify",
        "assemble",
        "resolve",
        "writeback",
    ] {
        let system = resource(&format!("{stage}/system.md"));
        assert!(
            system.contains("Call `submit`"),
            "{stage} must require structured submission"
        );
        assert!(
            !system.contains("STRICT JSON"),
            "{stage} must not request raw JSON prose"
        );
    }
}

#[test]
fn classify_corrections_request_the_complete_submission() {
    let corrections = [
        ("reject.md", resource("classify/reject.md")),
        ("bad_term.md", resource("classify/bad_term.md")),
    ];
    for (file, correction) in corrections {
        for field in [
            "entity",
            "classes",
            "relations",
            "attributes",
            "new_entities",
            "declarations",
            "has_keys",
            "inverse_functional_properties",
        ] {
            assert!(
                correction.contains(&format!("`{field}`")),
                "classify/{file} must request `{field}` again"
            );
        }
        assert!(
            correction.contains("all fields"),
            "classify/{file} must request the complete submission"
        );
    }
}

#[test]
fn assemble_example_decides_each_term_once() {
    let system = resource("assemble/system.md");
    let example = system.split("```json").nth(1).expect("JSON example");
    let mut seen = BTreeSet::new();
    for line in example.lines().filter(|line| line.contains("\"term\":")) {
        let term = line
            .split("\"term\":")
            .nth(1)
            .and_then(|tail| tail.split('"').nth(1))
            .expect("term value");
        assert!(
            seen.insert(term),
            "assemble example decides `{term}` more than once"
        );
    }
}

#[test]
fn ingest_and_reconcile_require_attributes_to_describe_the_underlying_entity() {
    let extract = resource("ingest/system.md");
    let reconcile = resource("reconcile/system.md");
    let reconcile_input = resource("reconcile/input.md");

    for (name, prompt) in [
        ("extract", extract.as_str()),
        ("reconcile", reconcile.as_str()),
    ] {
        assert!(
            prompt.contains("underlying-entity test"),
            "{name} omits the subject test"
        );
        assert!(
            prompt.contains("related entity"),
            "{name} omits related-entity guidance"
        );
        assert!(prompt.contains("event"), "{name} omits event guidance");
    }
    assert!(
        reconcile_input.contains("underlying")
            && reconcile_input.contains("entity")
            && reconcile_input.contains("events")
            && reconcile_input.contains("related entities"),
        "reconcile input must repeat the attribute boundary at the decision point"
    );
}

#[test]
fn reconcile_requires_attributes_to_have_the_exact_entity_as_subject() {
    let reconcile = resource("reconcile/system.md");
    let reconcile_input = resource("reconcile/input.md");

    for required in [
        "exact entity as the subject",
        "not proof that every value",
        "variant",
        "configuration",
        "component",
        "provider",
        "offer",
        "market observation",
        "group that contains",
        "do not store it as an unqualified attribute",
    ] {
        assert!(
            reconcile.contains(required),
            "reconcile omits `{required}` scope guidance"
        );
    }
    assert!(reconcile.contains("only Pro has 64 GB"));
    assert!(reconcile.contains("the core count describes the processor, not the computer"));
    assert!(
        reconcile_input.contains("exact-subject tests")
            && reconcile_input.contains("supplies provenance")
            && reconcile_input.contains("does not prove"),
        "reconcile input must repeat the exact-subject boundary at the decision point"
    );
}

#[test]
fn ingest_requires_relationship_memories_in_addition_to_entity_metadata() {
    let extract = resource("ingest/system.md");

    assert!(extract.contains("Entity metadata never preserves a relationship"));
    assert!(extract.contains("does not replace a memory"));
    assert!(extract.contains("emit a separate atomic"));
    assert!(extract.contains("include every participant in its `entities` array"));
    assert!(extract.contains("Project Aurora uses PostgreSQL"));
    assert!(
        extract.contains("\"entities\": [\"projects/project-aurora\", \"software/postgresql\"]")
    );
}

#[test]
fn ingest_requires_playbook_candidates_to_cover_existing_procedure_steps() {
    let extract = resource("ingest/system.md");

    assert!(extract.contains("cover all existing\nsupported procedure steps"));
    assert!(extract.contains("does not provide procedure coverage"));
    assert!(
        extract.contains("preserve every\nearlier part that the correction does not contradict")
    );
    assert!(extract.contains("using only its assigned Procedural memories"));
    assert!(extract.contains("narrow the candidate to the outcome"));
}

#[test]
fn ingest_requires_evidence_search_before_agent_submission() {
    let extract = resource("ingest/system.md");
    let reject = resource("ingest/reject.md");

    assert!(extract.contains("Before submitting any memory or candidate"));
    assert!(extract.contains("call `search_tool_evidence`"));
    assert!(extract.contains("prompt-local ID"));
    assert!(extract.contains("separate `tool_evidence` array"));
    assert!(extract.contains("`evidence_id`"));
    assert!(extract.contains("single contiguous exact span"));
    assert!(extract.contains("Do not use ellipses"));
    assert!(extract.contains("never from the Agent transcript"));
    assert!(reject.contains("successful non-recall tool execution"));
    assert!(reject.contains("Accepted memories and their stable evidence references"));
    assert!(reject.contains("Memories that need correction or removal"));
    assert!(reject.contains("omitting an accepted ID does not delete it"));
    assert!(reject.contains("Do not convert `20k` to `20,000`"));
    assert!(reject.contains("two evidence channels must remain separate"));
    assert!(!reject.contains("read_tool_execution"));
    assert!(reject.contains("Memory-vault reads"));
    assert!(reject.contains("evidence_id"));
    assert!(reject.contains("single contiguous exact span"));
    assert!(reject.contains("Do not use ellipses"));
    assert!(!reject.contains("\"support\""));
}

#[test]
fn ingest_requires_stable_entity_ids_during_correction() {
    let extract = resource("ingest/system.md");
    let reject = resource("ingest/reject.md");

    assert!(extract.contains("new entity, and candidate attribute a short request-local `id`"));
    assert!(extract.contains("its ID must not change"));
    assert!(extract.contains("\"id\": \"page1\""));
    assert!(reject.contains("new-entity"));
    assert!(reject.contains("a proposed entity path are not identity"));
}

#[test]
fn ingest_defines_task_lifecycle_as_episodic_evidence() {
    let extract = resource("ingest/system.md");

    assert!(extract.contains("`task scheduled` supports a `planned` episode"));
    assert!(extract.contains("`task completed` supports an `occurred` episode"));
    assert!(extract.contains("Do not treat `task completed` as proof"));
    assert!(extract.contains("`event_at`"));
    assert!(extract.contains("`target_at`"));
    assert!(extract.contains("must never return `absolute: null`"));
    assert!(extract.contains("For a `planned` episode, copy `target_at`"));
    assert!(extract.contains("copy `event_at`"));
    assert!(extract.contains("recurring schedule with no stated end is a durable `Fact`"));
    assert!(extract.contains("outcome is a new append-only Episodic memory"));
    assert!(extract.contains(
        "For a task lifecycle source, use the task lifecycle handle and an empty anchor quote"
    ));
}
