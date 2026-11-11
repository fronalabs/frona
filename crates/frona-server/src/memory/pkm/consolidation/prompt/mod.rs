//! Prompt files and variable contracts for each model conversation.
//!
//! ```text
//! resources/prompts/pkm/ingest/
//! ├── system.md   standing instructions - constant across invocations
//! ├── input.md    this invocation's payload - the {{vars}} the stage renders
//! ├── reject.md   the correction fed back when a submission fails validation
//! └── bad_term.md the correction for a CURIE that cannot be written down at all
//!                 (only conversations that mint terms have one)
//! ```
//!
//! Missing files, empty files, render failures, and mismatched variable sets are errors;
//! an incomplete prompt must not reach the model.

use crate::core::error::AppError;
use frona_text::prompt::{PromptLoader, PromptTemplate};

mod ids;

pub(crate) use ids::{PromptIds, prompt_evidence};

/// A rendered stage prompt: the system instructions and this invocation's input.
pub struct RenderedPrompt {
    pub system: String,
    pub input: String,
}

/// One stage's prompt directory, and the variables each file requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptSpec {
    /// Directory under `resources/prompts/pkm/`.
    pub dir: &'static str,
    /// Exactly the variables `input.md` requires. Enforced on render.
    pub vars: &'static [&'static str],
    /// Variables `reject.md` requires, or `None` for a stage that never pushes back.
    pub reject_vars: Option<&'static [&'static str]>,
    /// Variables `promote.md` requires for the optional reconcile advisory turn.
    pub advisory_vars: Option<&'static [&'static str]>,
    /// Variables `bad_term.md` requires for a stage that mints CURIEs.
    ///
    /// This is separate from `reject.md`: rejection reports a wrong decision, while this
    /// reports a decision that could not be parsed as a schema term.
    pub bad_term_vars: Option<&'static [&'static str]>,
}

impl PromptSpec {
    pub const INGEST: Self = Self {
        dir: "ingest",
        vars: &[
            "owner_name",
            "handle",
            "self_path",
            "existing_entities",
            "research_messages",
            "transcript",
        ],
        reject_vars: Some(&["rejected", "accepted_memories", "memory_repairs"]),
        advisory_vars: None,
        bad_term_vars: None,
    };
    pub const RECONCILE: Self = Self {
        dir: "reconcile",
        vars: &[
            "path",
            "kind",
            "name",
            "description",
            "attributes",
            "relations",
            "memories",
        ],
        reject_vars: Some(&["rejections"]),
        advisory_vars: Some(&["suggestions"]),
        bad_term_vars: None,
    };
    pub const PAGE_AUTHOR: Self = Self {
        dir: "page-author",
        vars: &[
            "name",
            "kind",
            "path",
            "description",
            "current",
            "superseded",
            "attributes",
            "related",
        ],
        reject_vars: None,
        advisory_vars: None,
        bad_term_vars: None,
    };
    pub const PLAYBOOK_RESOLVE: Self = Self {
        dir: "playbook-resolve",
        vars: &["candidate", "memories", "existing_playbooks"],
        reject_vars: Some(&["rejections"]),
        advisory_vars: None,
        bad_term_vars: None,
    };
    pub const PLAYBOOK_AUTHOR: Self = Self {
        dir: "playbook-author",
        vars: &[
            "path",
            "name",
            "description",
            "body",
            "memories",
            "transcript",
            "invocations",
        ],
        reject_vars: Some(&["rejections"]),
        advisory_vars: None,
        bad_term_vars: None,
    };
    pub const WRITEBACK: Self = Self {
        dir: "writeback",
        vars: &["memories", "diff"],
        reject_vars: None,
        advisory_vars: None,
        bad_term_vars: None,
    };
    pub const CLASSIFY: Self = Self {
        dir: "classify",
        vars: &[
            "name",
            "description",
            "contributions",
            "identity_evidence",
            "facts",
            "relations",
            "attributes",
            "minted",
            "evidence",
        ],
        reject_vars: Some(&["violations", "class"]),
        advisory_vars: None,
        bad_term_vars: Some(&["terms"]),
    };
    pub const ASSEMBLE: Self = Self {
        dir: "assemble",
        vars: &["proposals", "axioms"],
        reject_vars: Some(&["rejections"]),
        advisory_vars: None,
        bad_term_vars: Some(&["terms"]),
    };
    pub const RESOLVE: Self = Self {
        dir: "resolve",
        vars: &[
            "path",
            "name",
            "aliases",
            "description",
            "kind",
            "identity_evidence",
            "assertions",
            "candidates",
        ],
        reject_vars: Some(&["proposed", "subject", "candidates"]),
        advisory_vars: None,
        bad_term_vars: None,
    };

    /// Every stage prompt. Test-only: its purpose is the coverage checks below
    /// (all render; no directory is stray), not dispatch.
    #[cfg(test)]
    pub const ALL: &'static [Self] = &[
        Self::INGEST,
        Self::RECONCILE,
        Self::PAGE_AUTHOR,
        Self::PLAYBOOK_RESOLVE,
        Self::PLAYBOOK_AUTHOR,
        Self::WRITEBACK,
        Self::CLASSIFY,
        Self::ASSEMBLE,
        Self::RESOLVE,
    ];

    pub fn system_path(&self) -> String {
        format!("pkm/{}/system.md", self.dir)
    }

    pub fn input_path(&self) -> String {
        format!("pkm/{}/input.md", self.dir)
    }

    pub fn reject_path(&self) -> String {
        format!("pkm/{}/reject.md", self.dir)
    }

    pub fn bad_term_path(&self) -> String {
        format!("pkm/{}/bad_term.md", self.dir)
    }

    pub fn advisory_path(&self) -> String {
        format!("pkm/{}/promote.md", self.dir)
    }

    /// Render the system instructions and this invocation's input, or fail loudly.
    ///
    /// Never returns an empty prompt: a missing file, an unrenderable template, or a
    /// variable set that does not match [`vars`](Self::vars) is an `Err`. A mismatched
    /// variable set is a programming error, so it is reported as such rather than left to
    /// surface as an opaque render failure.
    pub fn render(
        &self,
        prompts: &PromptLoader,
        vars: &[(&str, &str)],
    ) -> Result<RenderedPrompt, AppError> {
        let system = PromptTemplate::new(&self.system_path(), &[]).render(prompts, &[])?;
        let input = PromptTemplate::new(&self.input_path(), self.vars).render(prompts, vars)?;
        // Ingest must contain the actual transcript, not merely render a valid
        // template. Keep this check at rendering so every SDK entry preserves it.
        if *self == Self::INGEST
            && let Some((_, transcript)) = vars
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("transcript"))
            && !input.contains(transcript.trim())
        {
            return Err(AppError::Internal(
                "extract: rendered prompt does not contain the transcript — refusing to call the model with no input".into(),
            ));
        }
        Ok(RenderedPrompt { system, input })
    }

    /// Render this stage's correction prompt - fed back into an in-flight conversation
    /// when a submission fails validation.
    ///
    /// Same strictness as [`render`](Self::render): an unrenderable reject prompt is an
    /// error, not an empty nudge the model would read as "say that again".
    pub fn reject(
        &self,
        prompts: &PromptLoader,
        vars: &[(&str, &str)],
    ) -> Result<String, AppError> {
        let declared = self.reject_vars.ok_or_else(|| {
            AppError::Internal(format!(
                "prompt `{}`: stage declares no reject prompt",
                self.dir
            ))
        })?;
        Ok(PromptTemplate::new(&self.reject_path(), declared).render(prompts, vars)?)
    }

    pub fn advisory(
        &self,
        prompts: &PromptLoader,
        vars: &[(&str, &str)],
    ) -> Result<String, AppError> {
        let declared = self.advisory_vars.ok_or_else(|| {
            AppError::Internal(format!(
                "prompt `{}`: stage declares no advisory prompt",
                self.dir
            ))
        })?;
        Ok(PromptTemplate::new(&self.advisory_path(), declared).render(prompts, vars)?)
    }

    /// Render this stage's **unreadable-term** correction: one or more CURIEs it sent that
    /// cannot be written to the schema (see `PrefixMap::validate_term`).
    ///
    /// Separate from [`reject`](Self::reject) because the two say different things and are
    /// reached differently: `reject` reports what the reasoner concluded about a decision,
    /// this reports that a decision could not be read at all, and it fires before any
    /// reasoning is attempted.
    pub fn bad_term(
        &self,
        prompts: &PromptLoader,
        vars: &[(&str, &str)],
    ) -> Result<String, AppError> {
        let declared = self.bad_term_vars.ok_or_else(|| {
            AppError::Internal(format!("prompt `{}`: stage mints no terms", self.dir))
        })?;
        Ok(PromptTemplate::new(&self.bad_term_path(), declared).render(prompts, vars)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn prompt_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("resources")
            .join("prompts")
    }

    fn prompts() -> PromptLoader {
        PromptLoader::new(prompt_root())
    }

    fn sample(vars: &[&'static str]) -> Vec<(&'static str, &'static str)> {
        vars.iter().map(|v| (*v, "x")).collect()
    }

    #[test]
    fn phase_contract_uses_shared_embedded_templates_and_rejects_duplicate_arguments() {
        let loader = PromptLoader::embedded([
            ("pkm/writeback/system.md", "Interpret the page edit"),
            ("pkm/writeback/input.md", "{{memories}} / {{diff}}"),
        ]);
        let rendered = PromptSpec::WRITEBACK
            .render(&loader, &[("memories", "facts"), ("diff", "changes")])
            .unwrap();
        assert_eq!(rendered.input, "facts / changes");
        assert!(
            PromptSpec::WRITEBACK
                .render(
                    &loader,
                    &[
                        ("memories", "facts"),
                        ("diff", "changes"),
                        ("diff", "replacement"),
                    ]
                )
                .is_err()
        );
    }

    #[test]
    fn phase_contract_preserves_named_render_errors() {
        let loader = PromptLoader::embedded([
            ("pkm/writeback/system.md", "Interpret the page edit"),
            ("pkm/writeback/input.md", "{{undefined_variable}}"),
        ]);
        let error = PromptSpec::WRITEBACK
            .render(&loader, &[("memories", "facts"), ("diff", "changes")])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("pkm/writeback/input.md"), "{error}");
        assert!(error.contains("undefined"), "{error}");
    }

    #[test]
    fn ingest_refuses_a_template_that_omits_its_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("pkm/ingest");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("system.md"), "Extract grounded facts").unwrap();
        std::fs::write(stage.join("input.md"), "No transcript here").unwrap();
        let error = PromptSpec::INGEST
            .render(
                &PromptLoader::new(dir.path()),
                &sample(PromptSpec::INGEST.vars),
            )
            .err()
            .expect("missing payload must fail before a model call");
        assert!(
            error
                .to_string()
                .contains("does not contain the transcript")
        );
        let mut vars = sample(PromptSpec::INGEST.vars);
        vars.iter_mut()
            .find(|(key, _)| *key == "transcript")
            .unwrap()
            .0 = "TRANSCRIPT";
        let error = PromptSpec::INGEST
            .render(&PromptLoader::new(dir.path()), &vars)
            .err()
            .expect("case-insensitive arguments must preserve the payload guard");
        assert!(
            error
                .to_string()
                .contains("does not contain the transcript")
        );
    }

    /// Every stage's files exist and render with exactly its declared variables. Add a
    /// `{{placeholder}}` to any of them without declaring it and this fails here, instead
    /// of blanking that stage's prompt in production.
    #[test]
    fn every_stage_prompt_renders_with_exactly_its_declared_vars() {
        let loader = prompts();
        for p in PromptSpec::ALL {
            let r = p
                .render(&loader, &sample(p.vars))
                .unwrap_or_else(|e| panic!("stage `{}` must render: {e}", p.dir));
            assert!(!r.system.trim().is_empty(), "`{}` system is empty", p.dir);
            assert!(!r.input.trim().is_empty(), "`{}` input is empty", p.dir);

            match p.reject_vars {
                Some(rv) => assert!(
                    p.reject(&loader, &sample(rv)).is_ok(),
                    "`{}` declares a reject prompt, so it must render",
                    p.dir
                ),
                None => assert!(
                    p.reject(&loader, &[]).is_err(),
                    "`{}` declares no reject prompt, so asking for one is an error",
                    p.dir
                ),
            }
            match p.bad_term_vars {
                Some(bv) => assert!(
                    p.bad_term(&loader, &sample(bv)).is_ok(),
                    "`{}` mints terms, so its bad-term correction must render",
                    p.dir
                ),
                None => assert!(
                    p.bad_term(&loader, &[]).is_err(),
                    "`{}` mints no terms, so asking for that correction is an error",
                    p.dir
                ),
            }
            match p.advisory_vars {
                Some(av) => assert!(
                    p.advisory(&loader, &sample(av)).is_ok(),
                    "`{}` declares an advisory prompt, so it must render",
                    p.dir
                ),
                None => assert!(
                    p.advisory(&loader, &[]).is_err(),
                    "`{}` declares no advisory prompt, so asking for one is an error",
                    p.dir
                ),
            }
        }
    }

    /// The guard itself: a variable set that does not match is refused, so a stage can
    /// never reach the model with a half-rendered prompt.
    #[test]
    fn mismatched_variable_set_is_refused() {
        let loader = prompts();
        assert!(
            PromptSpec::INGEST
                .render(&loader, &[("owner_name", "Casey Owner")])
                .is_err(),
            "an incomplete variable set must fail, not render empty"
        );
        assert!(
            PromptSpec::WRITEBACK
                .render(&loader, &[("memories", "m"), ("diff", "d"), ("extra", "?")])
                .is_err(),
            "an undeclared extra variable must fail too — it means the caller and the \
             file disagree about the contract"
        );
        assert!(
            PromptSpec::INGEST
                .reject(&loader, &[("wrong", "x")])
                .is_err(),
            "the reject prompt's variables are checked like any other"
        );
    }

    /// The root contains only declared directories, and each directory contains exactly
    /// the files its specification names.
    #[test]
    fn every_stage_directory_holds_exactly_what_it_declares() {
        let loader = prompts();
        let declared: BTreeSet<String> =
            PromptSpec::ALL.iter().map(|p| p.dir.to_string()).collect();
        let on_disk: BTreeSet<String> = std::fs::read_dir(prompt_root().join("pkm"))
            .expect("PKM prompt root")
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        assert_eq!(on_disk, declared, "PKM prompt directories");

        for p in PromptSpec::ALL {
            let mut want: BTreeSet<String> = ["system.md", "input.md"]
                .iter()
                .map(|f| format!("pkm/{}/{f}", p.dir))
                .collect();
            if p.reject_vars.is_some() {
                want.insert(format!("pkm/{}/reject.md", p.dir));
            }
            if p.bad_term_vars.is_some() {
                want.insert(format!("pkm/{}/bad_term.md", p.dir));
            }
            if p.advisory_vars.is_some() {
                want.insert(format!("pkm/{}/promote.md", p.dir));
            }
            let on_disk: BTreeSet<String> = loader
                .list_dir(&format!("pkm/{}", p.dir))
                .into_iter()
                .collect();
            assert_eq!(on_disk, want, "stage `{}` directory contents", p.dir);
        }
    }

    /// Every caller using structured inference terminates through the generated submit
    /// tool so a schema-shaped text response cannot bypass structured submission.
    #[test]
    fn every_structured_stage_explicitly_calls_submit() {
        let loader = prompts();
        let structured = [
            PromptSpec::INGEST,
            PromptSpec::RECONCILE,
            PromptSpec::PLAYBOOK_RESOLVE,
            PromptSpec::PLAYBOOK_AUTHOR,
            PromptSpec::CLASSIFY,
            PromptSpec::ASSEMBLE,
            PromptSpec::RESOLVE,
            PromptSpec::WRITEBACK,
        ];
        for p in structured {
            let system = loader
                .read(&format!("pkm/{}/system.md", p.dir))
                .unwrap_or_else(|| panic!("missing system prompt for `{}`", p.dir));
            assert!(
                system.contains("Call `submit`"),
                "structured stage `{}` must tell the model to call submit",
                p.dir
            );
            assert!(
                !system.contains("STRICT JSON"),
                "structured stage `{}` must not ask for raw JSON prose",
                p.dir
            );
        }
    }

    /// A rejected structured answer is discarded wholesale. Correction prompts therefore
    /// have to request every required field, or a revision silently loses whatever field
    /// the correction forgot to mention.
    #[test]
    fn classify_corrections_request_the_complete_submission() {
        let loader = prompts();
        let corrections = [
            (
                "reject.md",
                PromptSpec::CLASSIFY
                    .reject(&loader, &sample(PromptSpec::CLASSIFY.reject_vars.unwrap()))
                    .expect("classify reject"),
            ),
            (
                "bad_term.md",
                PromptSpec::CLASSIFY
                    .bad_term(
                        &loader,
                        &sample(PromptSpec::CLASSIFY.bad_term_vars.unwrap()),
                    )
                    .expect("classify bad term"),
            ),
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

    /// Assemble requires exactly one decision per term; examples are part of the
    /// contract and must not demonstrate the duplicate-term shape the prose rejects.
    #[test]
    fn assemble_example_decides_each_term_once() {
        let loader = prompts();
        let system = loader
            .read("pkm/assemble/system.md")
            .expect("assemble system");
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
    fn canonical_stage_ownership_is_present() {
        let loader = prompts();
        let contract = loader.read("pkm/README.md").expect("PKM prompt contract");
        for stage in [
            "Ingest",
            "Classify",
            "Resolve",
            "Reconcile",
            "Assemble",
            "Playbook Resolve",
            "Playbook Author",
            "Page Author",
            "Writeback",
        ] {
            assert!(contract.contains(stage), "stage ownership omits `{stage}`");
        }
    }

    #[test]
    fn ingest_and_reconcile_require_attributes_to_describe_the_underlying_entity() {
        let loader = prompts();
        let extract = loader.read("pkm/ingest/system.md").expect("extract system");
        let reconcile = loader
            .read("pkm/reconcile/system.md")
            .expect("reconcile system");
        let reconcile_input = PromptSpec::RECONCILE
            .render(&loader, &sample(PromptSpec::RECONCILE.vars))
            .expect("render reconcile input")
            .input;

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
        let loader = prompts();
        let reconcile = loader
            .read("pkm/reconcile/system.md")
            .expect("reconcile system");
        let reconcile_input = PromptSpec::RECONCILE
            .render(&loader, &sample(PromptSpec::RECONCILE.vars))
            .expect("render reconcile input")
            .input;

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
        let loader = prompts();
        let extract = loader.read("pkm/ingest/system.md").expect("extract system");

        assert!(extract.contains("Entity metadata never preserves a relationship"));
        assert!(extract.contains("does not replace a memory"));
        assert!(extract.contains("emit a separate atomic"));
        assert!(extract.contains("include every participant in its `entities` array"));
        assert!(extract.contains("Project Aurora uses PostgreSQL"));
        assert!(
            extract
                .contains("\"entities\": [\"projects/project-aurora\", \"software/postgresql\"]")
        );
    }

    #[test]
    fn ingest_requires_playbook_candidates_to_cover_existing_procedure_steps() {
        let loader = prompts();
        let extract = loader.read("pkm/ingest/system.md").expect("extract system");

        assert!(extract.contains("cover all existing\nsupported procedure steps"));
        assert!(extract.contains("does not provide procedure coverage"));
        assert!(
            extract
                .contains("preserve every\nearlier part that the correction does not contradict")
        );
        assert!(extract.contains("using only its assigned Procedural memories"));
        assert!(extract.contains("narrow the candidate to the outcome"));
    }

    #[test]
    fn ingest_requires_evidence_search_before_agent_submission() {
        let loader = prompts();
        let extract = loader.read("pkm/ingest/system.md").expect("extract system");
        let reject = PromptSpec::INGEST
            .reject(&loader, &sample(PromptSpec::INGEST.reject_vars.unwrap()))
            .expect("extract rejection");

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
        let loader = prompts();
        let extract = loader.read("pkm/ingest/system.md").expect("extract system");
        let reject = PromptSpec::INGEST
            .reject(&loader, &sample(PromptSpec::INGEST.reject_vars.unwrap()))
            .expect("extract rejection");

        assert!(extract.contains("new entity, and candidate attribute a short request-local `id`"));
        assert!(extract.contains("its ID must not change"));
        assert!(extract.contains("\"id\": \"page1\""));
        assert!(reject.contains("new-entity"));
        assert!(reject.contains("a proposed entity path are not identity"));
    }

    #[test]
    fn ingest_defines_task_lifecycle_as_episodic_evidence() {
        let loader = prompts();
        let extract = loader.read("pkm/ingest/system.md").expect("extract system");

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
}
