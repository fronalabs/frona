use super::*;
use crate::model::{EntityCategory, KnowledgeConsolidationEntity};

fn entity(path: &str, name: &str) -> KnowledgeEntity {
    let mut row = KnowledgeConsolidationEntity::pending(
        "run",
        "owner",
        path,
        EntityCategory::Concept,
        vec![],
        Default::default(),
    );
    row.name = name.into();
    row.as_knowledge_entity()
}
fn memory(id: &str, content: &str) -> KnowledgeMemory {
    serde_json::from_value(serde_json::json!({
        "id":id,"user_id":"owner","created_at":"2026-11-11T00:00:00Z",
        "kind":"fact","content":content,"relations":[],"disposition":"none","evidence":[]
    }))
    .unwrap()
}

#[test]
fn search_matches_names_aliases_and_descriptions_with_exact_aliases_first() {
    let mut alias = entity("people/casey", "Casey Example");
    alias.aliases.insert("Operator".into());
    let mut described = entity("teams/ops", "Operations");
    described.description = "An operator team".into();
    let entities = vec![
        entity("people/assistant", "Operator assistant"),
        described,
        alias,
    ];
    let hits = search(&entities, "OPERATOR");
    assert_eq!(
        hits.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec!["people/casey", "people/assistant", "teams/ops"]
    );
    assert!(search(&entities, "unrelated").is_empty());
}

#[test]
fn search_output_has_a_shared_hit_cap_and_untyped_marker() {
    let entities = (0..12)
        .map(|i| entity(&format!("people/{i}"), "Casey"))
        .collect::<Vec<_>>();
    let output = render_search(&entities, "Casey", &PrefixMap::standard());
    assert_eq!(output.lines().count(), 10);
    assert!(output.contains("[untyped]"));
    assert!(
        render_search(&entities, "missing", &PrefixMap::standard()).contains("No entity matches")
    );
}

#[test]
fn read_output_deduplicates_memories_sorts_aliases_and_preserves_evidence_rules() {
    let mut entity = entity("people/casey", "Casey");
    entity.aliases.extend(["Zulu".into(), "Alpha".into()]);
    let a = memory("a", "Uses port 8080.");
    let b = memory("b", "Uses Postgres.");
    let output = render(&entity, [&b, &a, &a], &PrefixMap::standard());
    assert!(output.contains("aliases=Alpha, Zulu"));
    assert_eq!(output.matches("Uses port 8080.").count(), 1);
    assert!(output.find("Uses port 8080.").unwrap() < output.find("Uses Postgres.").unwrap());
    assert!(output.contains("body:\n(not authored yet)"));
}
