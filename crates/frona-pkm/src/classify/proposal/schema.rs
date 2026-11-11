use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::model::KnowledgeConsolidationEntity;
use crate::ontology::{Characteristic, SchemaEdit, TermKind};
use crate::path::normalize_path;
use crate::prompt::PromptIds;

/// Classify's output - the Classify's classification and mapping of one mention.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Classification {
    /// Canonical entity shape authored from all grounded same-path contributions.
    pub entity: EntityShape,
    /// Every class this entity is an instance of. An entity is genuinely several
    /// things - a person who is also an employee - and asking for a single winner made
    /// the model discard the rest.
    pub classes: Vec<ClassChoice>,
    /// The "Map" half - the entity's stated relations, typed to CURIE object
    /// properties (existing standard terms or `frona:` mints).
    #[serde(default)]
    pub relations: Vec<RelationMapping>,
    /// The entity's free-text attributes, each typed to a CURIE **and** decided
    /// data-vs-object. See [`AttributeMapping`].
    #[serde(default)]
    pub attributes: Vec<AttributeMapping>,
    /// Entities an attribute value names that have **no entity yet**. See [`NewEntity`].
    #[serde(default)]
    pub new_entities: Vec<NewEntity>,
    /// Ontology declarations for every new `frona:` term referenced by this response.
    /// Mappings say how this entity uses a term; declarations say what the term means.
    #[serde(default)]
    pub declarations: Vec<OntologyDeclaration>,
    /// Class-scoped property groups the Classify stage considers useful for identifying
    /// duplicate entities. Provisional until adjudication; Resolve is the only stage that
    /// may turn a match into an entity merge.
    #[serde(default)]
    pub has_keys: Vec<HasKeyMarker>,
    /// Object properties whose shared target is useful identity evidence. These are
    /// retrieval markers, not reasoner axioms, until adjudication accepts them.
    #[serde(default)]
    pub inverse_functional_properties: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HasKeyMarker {
    pub class: String,
    pub properties: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntityShape {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// One centrally-authored declaration for a term minted by classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OntologyDeclaration {
    Class {
        term: String,
        description: String,
        parents: Vec<String>,
        #[serde(default)]
        equivalent_to: Vec<String>,
        #[serde(default)]
        disjoint_with: Vec<String>,
    },
    ObjectProperty {
        term: String,
        description: String,
        #[serde(default)]
        domain: Vec<String>,
        #[serde(default)]
        range: Vec<String>,
        subproperty_of: Option<String>,
        inverse: Option<String>,
        #[serde(default)]
        characteristics: Vec<Characteristic>,
    },
    DataProperty {
        term: String,
        description: String,
        datatype: Option<String>,
    },
}

impl OntologyDeclaration {
    pub fn term(&self) -> &str {
        match self {
            Self::Class { term, .. }
            | Self::ObjectProperty { term, .. }
            | Self::DataProperty { term, .. } => term.trim(),
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Class { description, .. }
            | Self::ObjectProperty { description, .. }
            | Self::DataProperty { description, .. } => description.trim(),
        }
    }

    pub fn edits(&self) -> Vec<SchemaEdit> {
        match self {
            Self::Class {
                term,
                parents,
                equivalent_to,
                disjoint_with,
                ..
            } => {
                let term = term.trim().to_string();
                let mut edits = if parents.is_empty() {
                    vec![SchemaEdit::DeclareClass {
                        class: term.clone(),
                    }]
                } else {
                    parents
                        .iter()
                        .filter_map(|parent| {
                            let parent = parent.trim();
                            (!parent.is_empty()).then(|| SchemaEdit::SubClassOf {
                                sub: term.clone(),
                                sup: parent.to_string(),
                            })
                        })
                        .collect()
                };
                edits.extend(equivalent_to.iter().filter_map(|other| {
                    let other = other.trim();
                    (!other.is_empty()).then(|| SchemaEdit::EquivalentClasses {
                        a: term.clone(),
                        b: other.to_string(),
                    })
                }));
                edits.extend(disjoint_with.iter().filter_map(|other| {
                    let other = other.trim();
                    (!other.is_empty()).then(|| SchemaEdit::DisjointClasses {
                        a: term.clone(),
                        b: other.to_string(),
                    })
                }));
                edits
            }
            Self::ObjectProperty {
                term,
                domain,
                range,
                subproperty_of,
                inverse,
                characteristics,
                ..
            } => {
                let term = term.trim().to_string();
                let mut edits = vec![SchemaEdit::DeclareObjectProperty {
                    property: term.clone(),
                }];
                edits.extend(domain.iter().filter_map(|class| {
                    let class = class.trim();
                    (!class.is_empty()).then(|| SchemaEdit::ObjectPropertyDomain {
                        property: term.clone(),
                        class: class.to_string(),
                    })
                }));
                edits.extend(range.iter().filter_map(|class| {
                    let class = class.trim();
                    (!class.is_empty()).then(|| SchemaEdit::ObjectPropertyRange {
                        property: term.clone(),
                        class: class.to_string(),
                    })
                }));
                if let Some(parent) = subproperty_of
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    edits.push(SchemaEdit::SubPropertyOf {
                        sub: term.clone(),
                        sup: parent.into(),
                    });
                }
                if let Some(other) = inverse.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let (a, b) = if term.as_str() <= other {
                        (term.clone(), other.to_string())
                    } else {
                        (other.to_string(), term.clone())
                    };
                    edits.push(SchemaEdit::InverseProperties { a, b });
                }
                for characteristic in characteristics {
                    let edit = SchemaEdit::PropertyCharacteristic {
                        property: term.clone(),
                        characteristic: *characteristic,
                    };
                    if !edits.contains(&edit) {
                        edits.push(edit);
                    }
                }
                edits
            }
            Self::DataProperty { term, datatype, .. } => {
                let term = term.trim().to_string();
                let mut edits = vec![SchemaEdit::DeclareDataProperty {
                    property: term.clone(),
                }];
                if let Some(datatype) = datatype.as_deref().map(str::trim).filter(|s| !s.is_empty())
                {
                    edits.push(SchemaEdit::RestrictDatatype {
                        property: term,
                        datatype: datatype.into(),
                        min: None,
                        max: None,
                        pattern: None,
                    });
                }
                edits
            }
        }
    }

    fn terms(&self) -> Vec<&str> {
        match self {
            Self::Class {
                term,
                parents,
                equivalent_to,
                disjoint_with,
                ..
            } => std::iter::once(term.as_str())
                .chain(parents.iter().map(String::as_str))
                .chain(equivalent_to.iter().map(String::as_str))
                .chain(disjoint_with.iter().map(String::as_str))
                .collect(),
            Self::ObjectProperty {
                term,
                domain,
                range,
                subproperty_of,
                inverse,
                ..
            } => std::iter::once(term.as_str())
                .chain(domain.iter().map(String::as_str))
                .chain(range.iter().map(String::as_str))
                .chain(subproperty_of.iter().map(String::as_str))
                .chain(inverse.iter().map(String::as_str))
                .collect(),
            Self::DataProperty { term, datatype, .. } => std::iter::once(term.as_str())
                .chain(datatype.iter().map(String::as_str))
                .collect(),
        }
    }

    fn repaired(&self, px: &crate::ontology::PrefixMap) -> Self {
        let fix = |value: &str, kind: TermKind| {
            px.repair_term(value, kind)
                .unwrap_or_else(|_| value.trim().to_string())
        };
        let fix_all = |values: &[String], kind: TermKind| {
            values.iter().map(|value| fix(value, kind)).collect()
        };
        let fix_optional = |value: &Option<String>, kind: TermKind| {
            value
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| fix(value, kind))
        };
        match self {
            Self::Class {
                term,
                description,
                parents,
                equivalent_to,
                disjoint_with,
            } => Self::Class {
                term: fix(term, TermKind::Class),
                description: description.clone(),
                parents: fix_all(parents, TermKind::Class),
                equivalent_to: fix_all(equivalent_to, TermKind::Class),
                disjoint_with: fix_all(disjoint_with, TermKind::Class),
            },
            Self::ObjectProperty {
                term,
                description,
                domain,
                range,
                subproperty_of,
                inverse,
                characteristics,
            } => Self::ObjectProperty {
                term: fix(term, TermKind::Property),
                description: description.clone(),
                domain: fix_all(domain, TermKind::Class),
                range: fix_all(range, TermKind::Class),
                subproperty_of: fix_optional(subproperty_of, TermKind::Property),
                inverse: fix_optional(inverse, TermKind::Property),
                characteristics: characteristics.clone(),
            },
            Self::DataProperty {
                term,
                description,
                datatype,
            } => Self::DataProperty {
                term: fix(term, TermKind::Property),
                description: description.clone(),
                datatype: fix_optional(datatype, TermKind::Class),
            },
        }
    }
}

/// One class the entity belongs to, with the parent to declare if it is a new mint.
///
/// The parent rides with its class rather than sitting in a parallel field: with
/// several classes at once, a single `new_class_parent` could not say which mint it
/// belonged to.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClassChoice {
    /// The class CURIE - a standard term or a `frona:` term.
    pub class: String,
    /// When `class` is a *new* `frona:` term, the standard parent class CURIE.
    pub new_class_parent: Option<String>,
}

impl Classification {
    pub(crate) fn expand_prompt_ids(&mut self, ids: &PromptIds) -> Result<(), Error> {
        for entity in &mut self.new_entities {
            ids.expand_all(&mut entity.from_facts)?;
        }
        Ok(())
    }

    pub fn declaration_feedback(
        &self,
        existing: &HashSet<String>,
        px: &crate::ontology::PrefixMap,
    ) -> Option<String> {
        let mut by_term: HashMap<&str, &OntologyDeclaration> = HashMap::new();
        let mut problems = Vec::new();
        for declaration in &self.declarations {
            let term = declaration.term();
            if term.is_empty() {
                problems.push("a declaration has an empty term".into());
                continue;
            }
            if declaration.description().is_empty() {
                problems.push(format!("new term {term} needs a semantic description"));
            }
            if by_term.insert(term, declaration).is_some() {
                problems.push(format!("{term} is declared more than once"));
            }
            if matches!(declaration, OntologyDeclaration::Class { parents, .. } if parents.is_empty())
            {
                problems.push(format!("new class {term} must name at least one parent"));
            }
        }
        let needs =
            |term: &str| px.expand(term).starts_with("urn:frona:") && !existing.contains(term);
        for class in self
            .classes
            .iter()
            .map(|c| c.class.as_str())
            .chain(self.new_entities.iter().map(|e| e.class.as_str()))
        {
            let class = class.trim();
            if needs(class)
                && !matches!(by_term.get(class), Some(OntologyDeclaration::Class { .. }))
            {
                problems.push(format!(
                    "new class {class} needs one central class declaration"
                ));
            }
        }
        for relation in &self.relations {
            let term = relation.to.trim();
            if needs(term)
                && !matches!(
                    by_term.get(term),
                    Some(OntologyDeclaration::ObjectProperty { .. })
                )
            {
                problems.push(format!(
                    "new relation {term} needs one object_property declaration"
                ));
            }
        }
        for attribute in &self.attributes {
            let term = attribute.to.trim();
            if !needs(term) {
                continue;
            }
            let valid = if attribute.targets.is_empty() {
                matches!(
                    by_term.get(term),
                    Some(OntologyDeclaration::DataProperty { .. })
                )
            } else {
                matches!(
                    by_term.get(term),
                    Some(OntologyDeclaration::ObjectProperty { .. })
                )
            };
            if !valid {
                let kind = if attribute.targets.is_empty() {
                    "data_property"
                } else {
                    "object_property"
                };
                problems.push(format!(
                    "attribute {term} needs one {kind} declaration matching its usage"
                ));
            }
        }
        (!problems.is_empty()).then(|| problems.join("\n"))
    }

    pub fn identity_marker_feedback(
        &self,
        entity: &KnowledgeConsolidationEntity,
    ) -> Option<String> {
        let classes: HashSet<&str> = self
            .classes
            .iter()
            .map(|choice| choice.class.trim())
            .filter(|class| !class.is_empty())
            .collect();
        let mut properties: HashSet<&str> = self
            .attributes
            .iter()
            .map(|mapping| mapping.to.trim())
            .chain(self.relations.iter().map(|mapping| mapping.to.trim()))
            .filter(|property| !property.is_empty())
            .collect();
        let mut object_properties: HashSet<&str> = self
            .relations
            .iter()
            .map(|mapping| mapping.to.trim())
            .chain(
                self.attributes
                    .iter()
                    .filter(|mapping| !mapping.targets.is_empty())
                    .map(|mapping| mapping.to.trim()),
            )
            .filter(|property| !property.is_empty())
            .collect();
        if let Some(attributes) = entity.attributes.as_object() {
            properties.extend(attributes.keys().map(String::as_str));
        }
        for link in &entity.outgoing_links {
            properties.insert(link.relation.as_str());
            object_properties.insert(link.relation.as_str());
        }

        let mut problems = Vec::new();
        for marker in &self.has_keys {
            let class = marker.class.trim();
            if class.is_empty() || !classes.contains(class) {
                problems.push(format!("{class} is not one of this entity's classes"));
            }
            if marker
                .properties
                .iter()
                .all(|property| property.trim().is_empty())
            {
                problems.push(format!(
                    "hasKey for {class} must contain at least one property"
                ));
            }
            for property in marker
                .properties
                .iter()
                .map(|property| property.trim())
                .filter(|property| !property.is_empty())
            {
                if !properties.contains(property) {
                    problems.push(format!(
                        "{property} is not mapped or asserted on this entity"
                    ));
                }
            }
        }
        for property in self
            .inverse_functional_properties
            .iter()
            .map(|property| property.trim())
            .filter(|property| !property.is_empty())
        {
            if !object_properties.contains(property) {
                problems.push(format!(
                    "{property} is not an object property on this entity"
                ));
            }
        }
        (!problems.is_empty()).then(|| problems.join("\n"))
    }

    /// The class CURIEs, trimmed, deduped, blanks dropped.
    ///
    /// First-seen order, not sorted: `KnowledgeEntity.kinds` is chronological, and
    /// "reject the newest on a clash" is only well defined while it stays that way.
    pub fn class_curies(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.classes
            .iter()
            .map(|c| c.class.trim().to_string())
            .filter(|c| !c.is_empty() && seen.insert(c.clone()))
            .collect()
    }

    /// The same classification with every CURIE in house spelling.
    ///
    /// One normalisation point, at the model boundary, because the alternative is what this
    /// replaced: proposals were repaired on their way to adjudicate while entity kinds kept
    /// classify's raw spelling, so an entity was stamped `frona:tool_maker` with
    /// `frona:ToolMaker` declared - the term used and the term declared were different
    /// terms, which is precisely the "used but undeclared" state the schema layer is for
    /// preventing.
    ///
    /// Only style is changed here; legality was already settled by
    /// [`crate::ontology::PrefixMap::validate_term`] before the candidate was accepted. A term repair cannot
    /// improve is left exactly as it is - it is legal, so it is usable.
    pub fn repaired(&self, px: &crate::ontology::PrefixMap) -> Self {
        let fix = |s: &str, kind: TermKind| {
            px.repair_term(s, kind)
                .unwrap_or_else(|_| s.trim().to_string())
        };
        let opt = |o: &Option<String>, kind: TermKind| {
            o.as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| fix(s, kind))
        };
        Self {
            entity: self.entity.clone(),
            classes: self
                .classes
                .iter()
                .map(|c| ClassChoice {
                    class: fix(&c.class, TermKind::Class),
                    new_class_parent: opt(&c.new_class_parent, TermKind::Class),
                })
                .collect(),
            relations: self
                .relations
                .iter()
                .map(|r| RelationMapping {
                    // `from` is the free-text relation as stated, not a term - it is what a
                    // re-key matches on, so repairing it would break the match.
                    from: r.from.clone(),
                    to: fix(&r.to, TermKind::Property),
                })
                .collect(),
            attributes: self
                .attributes
                .iter()
                .map(|a| AttributeMapping {
                    from: a.from.clone(),
                    to: fix(&a.to, TermKind::Property),
                    targets: a.targets.clone(),
                })
                .collect(),
            new_entities: self
                .new_entities
                .iter()
                .map(|e| NewEntity {
                    class: fix(&e.class, TermKind::Class),
                    new_class_parent: opt(&e.new_class_parent, TermKind::Class),
                    ..e.clone()
                })
                .collect(),
            declarations: self
                .declarations
                .iter()
                .map(|declaration| declaration.repaired(px))
                .collect(),
            has_keys: self
                .has_keys
                .iter()
                .map(|marker| HasKeyMarker {
                    class: fix(&marker.class, TermKind::Class),
                    properties: marker
                        .properties
                        .iter()
                        .map(|property| fix(property, TermKind::Property))
                        .collect(),
                })
                .collect(),
            inverse_functional_properties: self
                .inverse_functional_properties
                .iter()
                .map(|property| fix(property, TermKind::Property))
                .collect(),
        }
    }

    /// Every CURIE in the submission - the strings that become
    /// permanent identifiers if this is accepted.
    ///
    /// Collected in one place so validating "the terms" cannot silently miss a field: an
    /// unchecked one is not a rejected classification but a schema that stops parsing.
    pub fn terms(&self) -> Vec<&str> {
        self.classes
            .iter()
            .flat_map(|c| [Some(c.class.as_str()), c.new_class_parent.as_deref()])
            .chain(self.relations.iter().map(|r| Some(r.to.as_str())))
            .chain(self.attributes.iter().map(|a| Some(a.to.as_str())))
            .chain(
                self.new_entities
                    .iter()
                    .flat_map(|e| [Some(e.class.as_str()), e.new_class_parent.as_deref()]),
            )
            .chain(
                self.declarations
                    .iter()
                    .flat_map(OntologyDeclaration::terms)
                    .map(Some),
            )
            .chain(
                self.has_keys
                    .iter()
                    .flat_map(|marker| {
                        std::iter::once(marker.class.as_str())
                            .chain(marker.properties.iter().map(String::as_str))
                    })
                    .map(Some),
            )
            .chain(
                self.inverse_functional_properties
                    .iter()
                    .map(String::as_str)
                    .map(Some),
            )
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// One relation mapping: a free-text relation on the entity's links → a CURIE object
/// property. Semantic axioms live only in the corresponding declaration.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RelationMapping {
    /// The free-text relation as extracted (e.g. "works for").
    pub from: String,
    /// The object-property CURIE to re-key it to (e.g. `schema:worksFor`, `frona:runs`).
    pub to: String,
}

/// One attribute mapping: a free-text key on the entity → a CURIE, plus the call this
/// stage exists to make - is it a **data** property or an **object** property?
///
/// `target` carries both halves of that answer, because they are one answer: a property
/// whose value is another entity in this knowledge base is an object property, and the
/// entity it names is where the edge goes. Absent means the value is a literal and the key
/// stays an attribute.
///
/// Ingest cannot decide this. It sees entity names for identity reuse, but has no ontology
/// vocabulary, so `"employer": "Example Corp"` and `"port": "5432"` are indistinguishable to it
/// - both are a key and a string.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttributeMapping {
    /// The free-text key as extracted (e.g. "employer"), verbatim.
    pub from: String,
    /// The property CURIE it should be keyed by (e.g. `schema:worksFor`, `frona:port`).
    pub to: String,
    /// The entity paths this attribute's *value* names. Non-empty → object property, and the
    /// attribute becomes one edge per path.
    ///
    /// A **list**, because one attribute value can name several entities. Each named entity
    /// needs its own edge, and a single target would silently discard the other matches.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub targets: Vec<String>,
}

/// An entity an attribute value names that has **no entity yet** - so the Classify stage makes
/// one instead of concluding the value is a literal.
///
/// This is the difference between a hole in the data and a fact about the schema. Whether
/// `worksFor` relates a person to an organization is true whether or not this vault
/// happens to hold an `organizations/example-corp` entity. Without this the pipeline read "the
/// search found nothing" as "the value is a string", declared the property a data
/// property, and wrote that reading into the schema permanently - where it stayed, since
/// the entity the next pass would need to find was exactly the one nothing ever created.
///
/// `from_facts` is what keeps a minted entity from being an empty shell: the fact that
/// stated the attribute already exists as a memory on the *source* entity, and
/// `knowledge_entity_source` is many-to-many, so the new entity shares it rather than
/// starting with nothing to render.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NewEntity {
    /// Where the entity goes (e.g. `organizations/example-corp`). Slugged before use, and it must
    /// also appear in the naming attribute's `targets` for an edge to be written - this
    /// field creates the entity, `targets` is what points at it.
    pub path: String,
    /// Display name (e.g. `Example Corp`).
    pub name: String,
    /// What the entity is, in plain words. Not decoration: a pass that dies between the
    /// mint and the schema commit leaves the entity untyped, and this is what the next
    /// pass's classify reads to type it.
    pub description: String,
    /// The class CURIE, under the same rules as [`ClassChoice`].
    pub class: String,
    /// When `class` is a *new* `frona:` term, the standard parent class CURIE.
    pub new_class_parent: Option<String>,
    /// IDs of the facts on the source entity that speak about this entity, cited from the
    /// `Facts` block. Validated against what the model was actually shown - the same
    /// discipline reconcile applies to a quoted `was`/`now`.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub from_facts: Vec<String>,
}

/// One mint that survived [`accept_mints`], with everything the write needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedMint {
    pub path: String,
    pub name: String,
    pub description: String,
    pub classes: Vec<String>,
    pub edits: Vec<SchemaEdit>,
    pub from_facts: Vec<String>,
}

/// The mints worth writing, from what the model proposed - pure, so the rule is stated as
/// tests rather than inferred from a stage that needs a database and a model to run.
///
/// Rejects what cannot become an entity: an unslugagble path, the source entity itself (an
/// entity does not name itself into existence), a mint with no name or no class. Later
/// duplicates of a path are dropped rather than merged - two descriptions of one entity in
/// one submission is the model equivocating, and the first answer is no worse than the
/// second.
pub fn accept_mints(
    mints: &[NewEntity],
    entity_path: &str,
    px: &crate::ontology::PrefixMap,
) -> Vec<AcceptedMint> {
    let mut out: Vec<AcceptedMint> = Vec::new();
    for m in mints {
        let Some(path) = normalize_path(&m.path) else {
            continue;
        };
        let (name, class) = (m.name.trim(), m.class.trim());
        if path == entity_path || name.is_empty() || class.is_empty() {
            continue;
        }
        if out.iter().any(|a| a.path == path) {
            continue;
        }
        // A `frona:` class needs its parent axiom proposed alongside it, exactly as a
        // class minted for the entity being classified does. A standard class is already
        // declared by the bundled ontologies.
        let parent = m.new_class_parent.as_deref().map(str::trim).unwrap_or("");
        let edits = (px.expand(class).starts_with("urn:frona:") && !parent.is_empty())
            .then(|| SchemaEdit::SubClassOf {
                sub: class.to_string(),
                sup: parent.to_string(),
            })
            .into_iter()
            .collect();
        out.push(AcceptedMint {
            path,
            name: name.to_string(),
            description: m.description.trim().to_string(),
            classes: vec![class.to_string()],
            edits,
            from_facts: m
                .from_facts
                .iter()
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect(),
        });
    }
    out
}

/// Accept `"organizations/example-corp"` as well as `["organizations/example-corp"]`.
///
/// The schema asks for an array, and a model that has just decided on exactly one entity has an
/// obvious reason to write it bare. Rejecting that submission would cost a turn to learn
/// nothing - the intent is unambiguous either way.
fn string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}

/// What the attribute half of classify decided for one entity: the schema edits the
/// decisions imply, the `(free_text_key, curie)` re-keys for attributes that stay
/// literals, and the `(free_text_key, curie, target_path)` promotions for those that
/// turned out to name another entity.
pub type AttributeDecisions = (
    Vec<SchemaEdit>,
    Vec<(String, String)>,
    Vec<(String, String, String)>,
);

/// What classify proposed for one entity. Held in memory for the whole pass: no schema
/// is committed and no entity is mutated until `assemble`'s `assemble`.
pub fn attribute_edits(
    mappings: &[AttributeMapping],
    entity_path: &str,
    held: &HashSet<&str>,
    known: &HashSet<String>,
    px: &crate::ontology::PrefixMap,
) -> AttributeDecisions {
    let (mut edits, mut rekeys, mut promoted) = (Vec::new(), Vec::new(), Vec::new());
    for m in mappings {
        let (from, to) = (m.from.trim(), m.to.trim());
        // A key the entity does not carry is a key the model invented; re-keying or
        // promoting it would write an attribute nothing stated.
        if from.is_empty() || to.is_empty() || !held.contains(from) {
            continue;
        }
        let is_mint = px.expand(to).starts_with("urn:frona:");
        // Self-links say nothing, and a repeated path would be the same edge twice.
        //
        // A path nothing knows about is dropped rather than trusted: `to_entity_path` is a plain
        // string the commit writes without a lookup, so an invented one becomes an edge
        // pointing at nothing and no later stage can tell it apart from a real link.
        // Dropping every target of an attribute leaves it a literal, which is the honest
        // outcome - the value named nothing this knowledge base has.
        let mut targets: Vec<&str> = Vec::new();
        for t in m.targets.iter().map(|t| t.trim()) {
            if !t.is_empty() && t != entity_path && known.contains(t) && !targets.contains(&t) {
                targets.push(t);
            }
        }
        // The declaration kind follows the decision - that is the whole point of the
        // stage. A promoted attribute is an object property; one that stays is a data
        // property. A standard term is already declared by the bundled ontologies and
        // needs no mint.
        if targets.is_empty() {
            if is_mint {
                edits.push(SchemaEdit::DeclareDataProperty {
                    property: to.to_string(),
                });
            }
            if from != to {
                rekeys.push((from.to_string(), to.to_string()));
            }
            continue;
        }
        // One declaration however many targets - the property is an object property once,
        // not once per edge.
        if is_mint {
            edits.push(SchemaEdit::DeclareObjectProperty {
                property: to.to_string(),
            });
        }
        for target in targets {
            promoted.push((from.to_string(), to.to_string(), target.to_string()));
        }
    }
    (edits, rekeys, promoted)
}

/// Most entities to offer per attribute value. The model needs enough to recognise the
/// entity, not the whole search result - a value that matches a dozen entities is ambiguous
/// enough that the extra ones add noise rather than evidence.
pub const ATTRIBUTE_CANDIDATES: usize = 5;

/// Most standard terms precomputed for one evidence query. The model needs a shortlist,
/// not the catalogue tool's full diagnostic result.
pub const EVIDENCE_VOCAB_HITS: usize = 8;

/// Most elements of one array value to look up. A long list is a tag set, not a set of
/// relations, and searching each one costs a query.
const ATTRIBUTE_VALUE_TERMS: usize = 8;

/// An attribute value as the prompt shows it - an array stays an array, so the model can see
/// that it holds several names and give a target for each.
pub fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

/// The names to search for inside one attribute value: the string itself, or each string
/// element of an array. Non-string scalars yield nothing - a port or a boolean names no entity,
/// and searching for `true` returns noise.
pub fn search_terms(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s.to_string()]
            }
        }
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_string)
            .take(ATTRIBUTE_VALUE_TERMS)
            .collect(),
        _ => Vec::new(),
    }
}

/// The schema edits a classification implies - a subclass axiom when it mints a new
/// `frona:` class under a standard parent; empty when reusing an existing class.
pub fn classification_edits(c: &Classification) -> Vec<SchemaEdit> {
    let mut edits: Vec<SchemaEdit> = c
        .classes
        .iter()
        .filter_map(|choice| {
            let sub = choice.class.trim();
            let sup = choice
                .new_class_parent
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            (!sub.is_empty() && !sup.is_empty()).then(|| SchemaEdit::SubClassOf {
                sub: sub.to_string(),
                sup: sup.to_string(),
            })
        })
        .collect();
    for edit in c.declarations.iter().flat_map(OntologyDeclaration::edits) {
        if !edits.contains(&edit) {
            edits.push(edit);
        }
    }
    edits
}
