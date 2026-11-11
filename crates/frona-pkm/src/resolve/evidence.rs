use std::collections::{BTreeMap, BTreeSet, HashSet};

use frona_text::GroundingText;
use serde::{Deserialize, Serialize};

use super::prompt_evidence;
use crate::candidates::RankedCandidate;
use crate::classify::HasKeyMarker;
use crate::model::{EntityHit, KnowledgeConsolidationEntity, normalize_identity_name};

/// Resolve's output - the identity verdict for one mention.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct ResolveDecision {
    /// The entity that survives. It may be the current subject or an offered candidate;
    /// empty means this is a distinct entity.
    pub(super) canonical: String,
    /// Offered candidates that are also the same entity and must be folded into
    /// `canonical`. Required when the current subject itself is canonical.
    pub(super) same_as: Vec<String>,
    /// Evidence for every offered candidate included in the identity merge.
    pub(super) merge_because: Vec<MergeBecause>,
    /// Evidence for every strong offered candidate deliberately kept distinct.
    pub(super) distinct_because: Vec<DistinctBecause>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum EvidenceSide {
    Subject,
    Candidate,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum EvidenceField {
    Name,
    Aliases,
    Type,
    Description,
    IdentityEvidence,
    Attributes,
    Assertions,
    IdentifyingPropertyMatches,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct ResolutionEvidence {
    pub(super) side: EvidenceSide,
    pub(super) field: EvidenceField,
    pub(super) quote: Option<String>,
    pub(super) property: Option<String>,
    pub(super) value: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum MergeReason {
    SameUniqueIdentifier,
    SameInverseFunctionalValue,
    ExplicitSameIdentity,
    SameGroundedIdentity,
    SameEventIdentity,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct MergeBecause {
    pub(super) candidate: String,
    pub(super) reason: MergeReason,
    pub(super) evidence: Vec<ResolutionEvidence>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum DistinctReason {
    ConflictingUniqueIdentifier,
    ConflictingEventIdentity,
    ExplicitDistinctIdentity,
    RepresentationOrRole,
    DifferentEntityRole,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub(super) struct DistinctBecause {
    pub(super) candidate: String,
    pub(super) reason: DistinctReason,
    pub(super) evidence: Vec<ResolutionEvidence>,
}

#[derive(Debug)]
pub enum IdentityResolution {
    Merge {
        canonical: String,
        same_as: Vec<String>,
        evidence: serde_json::Value,
    },
    Distinct {
        evidence: serde_json::Value,
    },
    Unresolved {
        diagnostic: serde_json::Value,
        pair_count: usize,
    },
}

#[derive(Debug)]
pub struct IdentityConversation {
    pub decision: IdentityResolution,
    pub corrections: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdentityMatch {
    HasKey {
        class: String,
        properties: BTreeMap<String, Vec<String>>,
    },
    InverseFunctional {
        property: String,
        targets: Vec<String>,
    },
}

pub fn resolution_pair_fingerprint(
    subject: &KnowledgeConsolidationEntity,
    candidate: &EntityHit,
    matches: &[IdentityMatch],
) -> String {
    fn entity(
        path: &str,
        name: &str,
        aliases: &HashSet<String>,
        kinds: &[String],
    ) -> serde_json::Value {
        let mut kinds = kinds.to_vec();
        kinds.sort();
        kinds.dedup();
        let mut names: Vec<String> = std::iter::once(name)
            .chain(aliases.iter().map(String::as_str))
            .map(normalize_identity_name)
            .filter(|name| !name.is_empty())
            .collect();
        names.sort();
        names.dedup();
        serde_json::json!({
            "path": path,
            "names": names,
            "classes": kinds,
        })
    }
    let mut entities = vec![
        entity(
            &subject.path,
            &subject.name,
            &subject.aliases,
            &subject.kinds,
        ),
        entity(
            &candidate.path,
            &candidate.name,
            &candidate.aliases,
            &candidate.kinds,
        ),
    ];
    entities.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let mut matches = matches.to_vec();
    matches.sort();
    serde_json::json!({ "entities": entities, "identity_matches": matches }).to_string()
}

pub fn resolution_pair_key(a: &str, b: &str) -> String {
    let mut paths = [a, b];
    paths.sort_unstable();
    serde_json::to_string(&paths).unwrap_or_else(|_| format!("{a}|{b}"))
}

pub fn pair_change_requires_judgment(old: Option<&str>, new: &str) -> bool {
    let Some(old) = old else {
        return true;
    };
    if old == new {
        return false;
    }
    let (Ok(old), Ok(new)): (Result<serde_json::Value, _>, Result<serde_json::Value, _>) =
        (serde_json::from_str(old), serde_json::from_str(new))
    else {
        return true;
    };
    if old.get("entities") != new.get("entities") {
        return true;
    }

    fn atoms(value: &serde_json::Value) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for matched in value
            .get("identity_matches")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let kind = matched.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "has_key" => {
                    let class = matched.get("class").and_then(|v| v.as_str()).unwrap_or("");
                    for (property, values) in matched
                        .get("properties")
                        .and_then(|v| v.as_object())
                        .into_iter()
                        .flatten()
                    {
                        for value in values.as_array().into_iter().flatten() {
                            out.insert(
                                serde_json::json!([kind, class, property, value,]).to_string(),
                            );
                        }
                    }
                }
                "inverse_functional" => {
                    let property = matched
                        .get("property")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    for target in matched
                        .get("targets")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                    {
                        out.insert(serde_json::json!([kind, property, target,]).to_string());
                    }
                }
                _ => {}
            }
        }
        out
    }
    let old = atoms(&old);
    let new = atoms(&new);
    new.difference(&old).next().is_some()
}

#[derive(Default)]
pub(super) struct AssertionValues {
    pub(super) attributes: BTreeMap<String, BTreeSet<String>>,
    pub(super) relations: BTreeMap<String, BTreeSet<String>>,
}

impl AssertionValues {
    fn from_serialized(assertions: &[String]) -> Self {
        let mut out = Self::default();
        for assertion in assertions {
            let Ok(serde_json::Value::Array(parts)) = serde_json::from_str(assertion) else {
                continue;
            };
            let (Some(kind), Some(property), Some(value)) = (
                parts.first().and_then(serde_json::Value::as_str),
                parts.get(1).and_then(serde_json::Value::as_str),
                parts.get(2),
            ) else {
                continue;
            };
            let value = match value {
                serde_json::Value::String(value) => normalize_identity_name(value),
                value => value.to_string(),
            };
            if value.is_empty() {
                continue;
            }
            match kind {
                "attribute" => &mut out.attributes,
                "relation" => &mut out.relations,
                _ => continue,
            }
            .entry(property.to_string())
            .or_default()
            .insert(value);
        }
        out
    }

    fn all(&self, property: &str) -> BTreeSet<String> {
        self.attributes
            .get(property)
            .into_iter()
            .flatten()
            .chain(self.relations.get(property).into_iter().flatten())
            .cloned()
            .collect()
    }
}

pub fn resolution_identity_fingerprint(
    entity: &KnowledgeConsolidationEntity,
    keys: &[HasKeyMarker],
    inverse_functional_properties: &BTreeSet<String>,
    prefixes: &crate::ontology::PrefixMap,
) -> String {
    let values = AssertionValues::from_serialized(&entity.search_assertions);
    let mut classes = entity.kinds.clone();
    classes.sort();
    classes.dedup();
    let mut names: Vec<String> = std::iter::once(entity.name.as_str())
        .chain(entity.aliases.iter().map(String::as_str))
        .map(normalize_identity_name)
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    let expanded_classes: HashSet<String> =
        classes.iter().map(|class| prefixes.expand(class)).collect();
    let mut key_values = Vec::new();
    for key in keys {
        if !expanded_classes.contains(&prefixes.expand(&key.class)) {
            continue;
        }
        let properties: BTreeMap<String, Vec<String>> = key
            .properties
            .iter()
            .map(|property| property.trim())
            .filter(|property| !property.is_empty())
            .map(|property| {
                (
                    property.to_string(),
                    values.all(property).into_iter().collect(),
                )
            })
            .collect();
        key_values.push(serde_json::json!({
            "class": key.class,
            "properties": properties,
        }));
    }
    key_values.sort_by_key(serde_json::Value::to_string);
    let inverse_values: BTreeMap<String, Vec<String>> = inverse_functional_properties
        .iter()
        .map(|property| {
            (
                property.clone(),
                values
                    .relations
                    .get(property)
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    serde_json::json!({
        "path": entity.path,
        "names": names,
        "classes": classes,
        "has_keys": key_values,
        "inverse_functional_properties": inverse_values,
    })
    .to_string()
}

pub fn identity_matches(
    subject: &KnowledgeConsolidationEntity,
    candidate: &EntityHit,
    keys: &[HasKeyMarker],
    inverse_functional_properties: &BTreeSet<String>,
    prefixes: &crate::ontology::PrefixMap,
) -> Vec<IdentityMatch> {
    let subject_values = AssertionValues::from_serialized(&subject.search_assertions);
    let candidate_values = AssertionValues::from_serialized(&candidate.search_assertions);
    let has_class = |kinds: &[String], class: &str| {
        let class = prefixes.expand(class);
        kinds.iter().any(|kind| prefixes.expand(kind) == class)
    };
    let mut matches = Vec::new();
    for key in keys {
        if !has_class(&subject.kinds, &key.class) || !has_class(&candidate.kinds, &key.class) {
            continue;
        }
        let mut properties = BTreeMap::new();
        for property in key
            .properties
            .iter()
            .map(|property| property.trim())
            .filter(|property| !property.is_empty())
        {
            let ours = subject_values.all(property);
            let theirs = candidate_values.all(property);
            let shared: Vec<String> = ours.intersection(&theirs).cloned().collect();
            if shared.is_empty() {
                properties.clear();
                break;
            }
            properties.insert(property.to_string(), shared);
        }
        if !properties.is_empty() {
            matches.push(IdentityMatch::HasKey {
                class: key.class.clone(),
                properties,
            });
        }
    }
    for property in inverse_functional_properties {
        let Some(ours) = subject_values.relations.get(property) else {
            continue;
        };
        let Some(theirs) = candidate_values.relations.get(property) else {
            continue;
        };
        let targets: Vec<String> = ours.intersection(theirs).cloned().collect();
        if !targets.is_empty() {
            matches.push(IdentityMatch::InverseFunctional {
                property: property.clone(),
                targets,
            });
        }
    }
    matches.sort();
    matches.dedup();
    matches
}

#[derive(Clone)]
pub struct ResolutionDecisionContext {
    pub pair_fingerprints: Vec<(String, String)>,
    pub(super) candidate_block: String,
    pub(super) identity_evidence: String,
    pub(super) kinds_display: String,
    pub(super) subject_fields: ResolutionEvidenceFields,
    pub(super) candidate_fields: BTreeMap<String, ResolutionEvidenceFields>,
    pub(super) strong_candidates: BTreeSet<String>,
}

#[derive(Clone, Default)]
pub(super) struct ResolutionEvidenceFields {
    pub(super) name: String,
    pub(super) aliases: String,
    pub(super) kinds: String,
    pub(super) description: String,
    pub(super) identity_evidence: String,
    pub(super) attributes: String,
    pub(super) assertions: String,
    pub(super) identifying_property_matches: String,
}

impl ResolutionEvidenceFields {
    fn value(&self, field: EvidenceField) -> &str {
        match field {
            EvidenceField::Name => &self.name,
            EvidenceField::Aliases => &self.aliases,
            EvidenceField::Type => &self.kinds,
            EvidenceField::Description => &self.description,
            EvidenceField::IdentityEvidence => &self.identity_evidence,
            EvidenceField::Attributes => &self.attributes,
            EvidenceField::Assertions => &self.assertions,
            EvidenceField::IdentifyingPropertyMatches => &self.identifying_property_matches,
        }
    }
}

impl ResolutionDecisionContext {
    pub fn new(
        entity: &KnowledgeConsolidationEntity,
        candidates: &[RankedCandidate],
        candidate_identity_evidence: &BTreeMap<String, String>,
        identifying_matches: &BTreeMap<String, Vec<IdentityMatch>>,
        prefixes: &crate::ontology::PrefixMap,
    ) -> Self {
        let mut candidate_block = String::new();
        let mut candidate_fields = BTreeMap::new();
        let mut strong_candidates = BTreeSet::new();
        for candidate in candidates {
            let identity_evidence = candidate_identity_evidence
                .get(&candidate.entity.path)
                .map(String::as_str)
                .unwrap_or("(none)");
            let identifying_matches = identifying_matches
                .get(&candidate.entity.path)
                .cloned()
                .unwrap_or_default();
            let identifying_property_matches =
                serde_json::to_string(&identifying_matches).unwrap_or_else(|_| "[]".into());
            let candidate_aliases = candidate
                .entity
                .aliases
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let candidate_assertions = candidate.entity.search_assertions.join("; ");
            if candidate.evidence.forced
                || candidate.evidence.exact_name
                || candidate.evidence.token_containment
                || candidate.evidence.event_participants_match
                || !candidate.evidence.shared_assertions.is_empty()
                || !identifying_matches.is_empty()
                || ((candidate.evidence.ordered_similarity >= 0.92
                    || candidate.evidence.token_order_similarity >= 0.92)
                    && candidate.evidence.type_affinity > 0)
            {
                strong_candidates.insert(candidate.entity.path.clone());
            }
            candidate_fields.insert(
                candidate.entity.path.clone(),
                ResolutionEvidenceFields {
                    name: candidate.entity.name.clone(),
                    aliases: candidate
                        .entity
                        .aliases
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n"),
                    kinds: prefixes.display_joined(&candidate.entity.kinds),
                    description: candidate.entity.description.clone(),
                    identity_evidence: identity_evidence.to_string(),
                    attributes: String::new(),
                    assertions: candidate.entity.search_assertions.join("\n"),
                    identifying_property_matches: identifying_matches
                        .iter()
                        .map(|matched| serde_json::to_string(matched).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n"),
                },
            );
            candidate_block.push_str(&format!(
                "- path: {}\n  name: {}\n  aliases: {}\n  type: {}\n  assertions: {}\n  identity signals:\n    forced identity: {}\n    exact name: {}\n    token containment: {}\n    unordered event participants: {}\n    ordered similarity: {:.3}\n    token-order similarity: {:.3}\n    type affinity: {}\n    shared types: {}\n    shared assertions: {}\n    identifying property matches: {}\n  description: {}\n  identity evidence: {}\n",
                candidate.entity.path, candidate.entity.name,
                if candidate_aliases.is_empty() { "(none)" } else { &candidate_aliases },
                prefixes.display_joined(&candidate.entity.kinds),
                if candidate_assertions.is_empty() { "(none)" } else { &candidate_assertions },
                candidate.evidence.forced, candidate.evidence.exact_name,
                candidate.evidence.token_containment,
                candidate.evidence.event_participants_match,
                candidate.evidence.ordered_similarity,
                candidate.evidence.token_order_similarity,
                candidate.evidence.type_affinity,
                candidate.evidence.shared_kinds.join(", "),
                candidate.evidence.shared_assertions.join(", "),
                identifying_property_matches,
                candidate.entity.description,
                identity_evidence,
            ));
        }
        Self {
            pair_fingerprints: candidates
                .iter()
                .map(|candidate| {
                    let matches = identifying_matches
                        .get(&candidate.entity.path)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    (
                        resolution_pair_key(&entity.path, &candidate.entity.path),
                        resolution_pair_fingerprint(entity, &candidate.entity, matches),
                    )
                })
                .collect(),
            candidate_block,
            identity_evidence: prompt_evidence(&entity.identity_evidence),
            kinds_display: prefixes.display_joined(&entity.kinds),
            subject_fields: ResolutionEvidenceFields {
                name: entity.name.clone(),
                aliases: entity
                    .aliases
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
                kinds: prefixes.display_joined(&entity.kinds),
                description: entity.description.clone(),
                identity_evidence: prompt_evidence(&entity.identity_evidence),
                attributes: String::new(),
                assertions: entity.search_assertions.join("\n"),
                identifying_property_matches: identifying_matches
                    .values()
                    .flatten()
                    .map(|matched| serde_json::to_string(matched).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            candidate_fields,
            strong_candidates,
        }
    }

    pub(super) fn with_tool_visible_state(
        mut self,
        subject: &str,
        entities: &[crate::model::KnowledgeEntity],
    ) -> Self {
        for entity in entities {
            let fields = if entity.path == subject {
                Some(&mut self.subject_fields)
            } else {
                self.candidate_fields.get_mut(&entity.path)
            };
            let Some(fields) = fields else {
                continue;
            };
            fields.attributes =
                serde_json::to_string(&entity.attributes).unwrap_or_else(|_| "{}".into());
            fields.assertions = entity.search_assertions.join("\n");
        }
        self
    }
}

pub(super) fn attribute_value_contains(value: &serde_json::Value, submitted: &str) -> bool {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| attribute_value_contains(value, submitted)),
        serde_json::Value::String(value) => {
            GroundingText::new(value).resolve_value(submitted).is_ok()
        }
        serde_json::Value::Number(value) => GroundingText::new(&value.to_string())
            .resolve_value(submitted)
            .is_ok(),
        serde_json::Value::Bool(value) => GroundingText::new(&value.to_string())
            .resolve_value(submitted)
            .is_ok(),
        serde_json::Value::Null | serde_json::Value::Object(_) => false,
    }
}

pub(super) fn validate_evidence_reference(
    reference: &ResolutionEvidence,
    candidate: &str,
    context: &ResolutionDecisionContext,
    path: &str,
) -> Option<String> {
    let fields = match reference.side {
        EvidenceSide::Subject => &context.subject_fields,
        EvidenceSide::Candidate => match context.candidate_fields.get(candidate) {
            Some(fields) => fields,
            None => return Some(format!("{path}: candidate `{candidate}` was not offered")),
        },
    };
    let source = fields.value(reference.field);
    if source.trim().is_empty() {
        return Some(format!(
            "{path}: {:?}.{:?} is empty in the supplied Resolve input",
            reference.side, reference.field,
        ));
    }
    let submitted = reference.quote.as_deref().or(reference.value.as_deref());
    let Some(submitted) = submitted.filter(|value| !value.trim().is_empty()) else {
        return Some(format!("{path}: either `quote` or `value` is required"));
    };
    if reference.property.is_some()
        && !matches!(
            reference.field,
            EvidenceField::Attributes
                | EvidenceField::Assertions
                | EvidenceField::IdentifyingPropertyMatches
        )
    {
        return Some(format!(
            "{path}: `property` is valid only for attributes, assertions, or identifying_property_matches"
        ));
    }
    if let Some(property) = reference.property.as_deref() {
        if reference.field == EvidenceField::Attributes {
            let supported = serde_json::from_str::<serde_json::Value>(source)
                .ok()
                .and_then(|attributes| attributes.get(property).cloned())
                .is_some_and(|value| attribute_value_contains(&value, submitted));
            if !supported {
                return Some(format!(
                    "{path}: property {property:?} does not have value {submitted:?} in the tool-visible {:?}.attributes for `{candidate}`",
                    reference.side,
                ));
            }
            return None;
        }
        let same_assertion = source.lines().any(|assertion| {
            GroundingText::new(assertion)
                .resolve_value(property)
                .is_ok()
                && GroundingText::new(assertion)
                    .resolve_value(submitted)
                    .is_ok()
        });
        if !same_assertion {
            return Some(format!(
                "{path}: property {property:?} and value {submitted:?} do not occur in one supplied {:?}.{:?} assertion for `{candidate}`",
                reference.side, reference.field,
            ));
        }
    }
    if reference.property.is_none() && GroundingText::new(source).resolve_value(submitted).is_err()
    {
        return Some(format!(
            "{path}: quote {:?} does not occur in supplied {:?}.{:?} for `{candidate}`",
            submitted, reference.side, reference.field,
        ));
    }
    None
}

pub(super) fn evidence_uses_both_sides(evidence: &[ResolutionEvidence]) -> bool {
    evidence
        .iter()
        .any(|item| item.side == EvidenceSide::Subject)
        && evidence
            .iter()
            .any(|item| item.side == EvidenceSide::Candidate)
}

pub(super) fn merge_reason_supports(reason: &MergeReason, evidence: &[ResolutionEvidence]) -> bool {
    let has = |field| evidence.iter().any(|item| item.field == field);
    match reason {
        MergeReason::SameUniqueIdentifier | MergeReason::SameInverseFunctionalValue => {
            has(EvidenceField::Attributes)
                || has(EvidenceField::Assertions)
                || has(EvidenceField::IdentifyingPropertyMatches)
        }
        MergeReason::ExplicitSameIdentity => has(EvidenceField::IdentityEvidence),
        MergeReason::SameGroundedIdentity => {
            has(EvidenceField::Name)
                || has(EvidenceField::Aliases)
                || has(EvidenceField::IdentityEvidence)
        }
        MergeReason::SameEventIdentity => {
            has(EvidenceField::Name)
                || has(EvidenceField::Assertions)
                || has(EvidenceField::IdentityEvidence)
        }
    }
}

pub(super) fn distinct_reason_supports(
    reason: &DistinctReason,
    evidence: &[ResolutionEvidence],
) -> bool {
    let has = |field| evidence.iter().any(|item| item.field == field);
    match reason {
        DistinctReason::ConflictingUniqueIdentifier => {
            has(EvidenceField::Attributes)
                || has(EvidenceField::Assertions)
                || has(EvidenceField::IdentifyingPropertyMatches)
        }
        DistinctReason::ConflictingEventIdentity => {
            has(EvidenceField::Name)
                || has(EvidenceField::Assertions)
                || has(EvidenceField::IdentityEvidence)
        }
        DistinctReason::ExplicitDistinctIdentity => has(EvidenceField::IdentityEvidence),
        DistinctReason::RepresentationOrRole | DistinctReason::DifferentEntityRole => {
            has(EvidenceField::Type) || has(EvidenceField::Description)
        }
    }
}

pub(super) fn validate_resolution_evidence(
    decision: &ResolveDecision,
    subject: &str,
    offered: &HashSet<&str>,
    context: &ResolutionDecisionContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut merged = BTreeSet::new();
    if !decision.canonical.trim().is_empty() && decision.canonical.trim() != subject {
        merged.insert(decision.canonical.trim().to_string());
    }
    merged.extend(decision.same_as.iter().map(|path| path.trim().to_string()));

    let mut merge_evidence = BTreeMap::new();
    for (index, because) in decision.merge_because.iter().enumerate() {
        let candidate = because.candidate.trim();
        let base = format!("merge_because[{index}]");
        if !offered.contains(candidate) {
            errors.push(format!("{base}.candidate: `{candidate}` was not offered"));
            continue;
        }
        if merge_evidence
            .insert(candidate.to_string(), because)
            .is_some()
        {
            errors.push(format!(
                "{base}.candidate: duplicate evidence for `{candidate}`"
            ));
        }
        if because.evidence.is_empty() {
            errors.push(format!(
                "{base}.evidence: at least one evidence reference is required"
            ));
        } else if !evidence_uses_both_sides(&because.evidence) {
            errors.push(format!(
                "{base}.evidence: merge evidence must cite both subject and candidate"
            ));
        }
        if !merge_reason_supports(&because.reason, &because.evidence) {
            errors.push(format!(
                "{base}.evidence: the cited fields do not support reason `{:?}`",
                because.reason,
            ));
        }
        for (evidence_index, reference) in because.evidence.iter().enumerate() {
            if let Some(error) = validate_evidence_reference(
                reference,
                candidate,
                context,
                &format!("{base}.evidence[{evidence_index}]"),
            ) {
                errors.push(error);
            }
        }
    }
    for candidate in &merged {
        if !merge_evidence.contains_key(candidate) {
            errors.push(format!(
                "merge_because: missing evidence for merged candidate `{candidate}`"
            ));
        }
    }
    for candidate in merge_evidence.keys() {
        if !merged.contains(candidate) {
            errors.push(format!(
                "merge_because: `{candidate}` has merge evidence but is not in the proposed identity set"
            ));
        }
    }

    let mut distinct_evidence = BTreeMap::new();
    for (index, because) in decision.distinct_because.iter().enumerate() {
        let candidate = because.candidate.trim();
        let base = format!("distinct_because[{index}]");
        if !offered.contains(candidate) {
            errors.push(format!("{base}.candidate: `{candidate}` was not offered"));
            continue;
        }
        if distinct_evidence
            .insert(candidate.to_string(), because)
            .is_some()
        {
            errors.push(format!(
                "{base}.candidate: duplicate evidence for `{candidate}`"
            ));
        }
        if merged.contains(candidate) {
            errors.push(format!(
                "{base}.candidate: `{candidate}` is also proposed for merge"
            ));
        }
        if because.evidence.is_empty() {
            errors.push(format!(
                "{base}.evidence: at least one evidence reference is required"
            ));
        } else if !evidence_uses_both_sides(&because.evidence) {
            errors.push(format!(
                "{base}.evidence: distinction evidence must cite both subject and candidate"
            ));
        }
        if !distinct_reason_supports(&because.reason, &because.evidence) {
            errors.push(format!(
                "{base}.evidence: the cited fields do not support reason `{:?}`",
                because.reason,
            ));
        }
        for (evidence_index, reference) in because.evidence.iter().enumerate() {
            if let Some(error) = validate_evidence_reference(
                reference,
                candidate,
                context,
                &format!("{base}.evidence[{evidence_index}]"),
            ) {
                errors.push(error);
            }
        }
    }
    for candidate in context.strong_candidates.difference(&merged) {
        if !distinct_evidence.contains_key(candidate) {
            errors.push(format!(
                "distinct_because: strong declined candidate `{candidate}` requires evidence"
            ));
        }
    }
    errors
}
