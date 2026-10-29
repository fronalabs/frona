//! What PKM contributes to a turn's prompt.
//!
//! Supplies rendered, keyed sections for immutable agent-context snapshots.
//! Selection and token budgets remain live; history replay uses the saved text.
//!
//! Concept pages stay pull-only - reached on demand through the tools. Only the playbook
//! *index* is pushed, so the agent knows which procedures exist without a blind search.

use crate::core::error::AppError;
use crate::inference::context::estimate_tokens;

use chrono::Utc;

use super::PkmService;
use super::model::{self, EntityCategory};
use super::vault::VaultScope;
use crate::agent::context::{AgentContextKind, AgentContextScope, AgentContextSection};

impl PkmService {
    pub(super) async fn instructions_for(
        &self,
        user: &crate::auth::User,
    ) -> Result<String, AppError> {
        let vault =
            VaultScope::resolve(&self.user_service, &self.storage, &user.id, &user.handle).await?;
        Ok(self
            .prompts
            .try_read_with_vars(
                "pkm/agent_section.md",
                &[
                    ("memory_root", &vault.root().to_string_lossy()),
                    ("directory", vault.directory()),
                ],
            )?
            .unwrap_or_default())
    }

    async fn short_memory_block(
        &self,
        user_id: &str,
    ) -> Result<Option<AgentContextSection>, AppError> {
        let rows = self.repo.list_short_memory(user_id).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let now = Utc::now();
        let mut scored: Vec<(f32, model::KnowledgeShortMemory)> = rows
            .into_iter()
            .map(|r| {
                let age = (now - r.last_accessed_at).num_seconds().max(0) as f32;
                let half_life = self.memory_config.pkm_short_memory_half_life_secs as f32;
                (model::decay_score(age, half_life), r)
            })
            .filter(|(s, _)| *s >= self.memory_config.pkm_short_memory_demote_threshold)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        scored.truncate(self.memory_config.pkm_short_memory_top_n);

        let lines: Vec<String> = scored
            .iter()
            .map(|(_, r)| format!("- {}\n", r.content))
            .collect();
        // The dropped count is discarded on purpose. Decay and `top_n` have already bound
        // this block, and unlike the playbook index it makes no completeness claim there
        // would be anything to mark against - `agent_section.md` describes it as
        // time-decayed hot context, not the whole of what the agent remembers.
        let (body, dropped) =
            take_within_budget(&lines, self.memory_config.pkm_short_memory_token_cap);
        Ok((!body.is_empty()).then(|| {
            AgentContextSection::tagged(
                AgentContextKind::ShortMemory,
                "short_memory",
                scored
                    .iter()
                    .zip(lines.iter())
                    .take(lines.len() - dropped)
                    .map(|((_, row), line)| (row.id.clone(), line.clone()))
                    .collect(),
            )
        }))
    }

    /// Build the `<available_playbooks>` index: name + description + absolute file path per
    /// playbook, so the agent *knows a playbook exists* without a blind `memory_search` (the
    /// pull-only gap). Only the one-line index is injected; the body is pulled on demand via
    /// `read(<path>)`. Token-capped - most-cited playbooks (`use_count`) win the budget; when
    /// the cap drops entries, a trailing marker says how many, so truncation is never silent.
    async fn playbook_index_block(
        &self,
        user_id: &str,
        vault: &VaultScope,
    ) -> Result<Option<AgentContextSection>, AppError> {
        let mut pages = self
            .repo
            .list_entities_by_category(user_id, EntityCategory::Playbook)
            .await?;
        if pages.is_empty() {
            return Ok(None);
        }
        pages.sort_by(|a, b| b.use_count.cmp(&a.use_count).then_with(|| a.id.cmp(&b.id)));

        let lines: Vec<String> = pages
            .iter()
            .map(|p| {
                format!(
                    "- {} — {}\n  {}\n",
                    p.name,
                    p.description,
                    vault.abs_page_file(&p.path)
                )
            })
            .collect();
        let (body, dropped) =
            take_within_budget(&lines, self.memory_config.pkm_playbook_index_token_cap);
        if body.is_empty() {
            return Ok(None);
        }
        let mut section = AgentContextSection::tagged(
            AgentContextKind::Playbooks,
            "available_playbooks",
            pages
                .iter()
                .zip(lines.iter())
                .take(lines.len() - dropped)
                .map(|(page, line)| (page.id.clone(), line.clone()))
                .collect(),
        );
        if dropped > 0 {
            section.suffix = format!(
                "- (+{dropped} more playbook(s) not shown — memory_search by task to find them)\n{}",
                section.suffix
            );
        }
        Ok(Some(section))
    }

    /// Collect the whole contribution at a new turn or compaction boundary.
    pub(super) async fn collect_context(
        &self,
        scope: &AgentContextScope<'_>,
    ) -> Result<Vec<AgentContextSection>, AppError> {
        let user = scope.user;
        let mut sections = Vec::new();
        let vault =
            VaultScope::resolve(&self.user_service, &self.storage, &user.id, &user.handle).await?;

        // <user_profile>: always injected. Header comes from the resolved session User;
        // enrichment is the self-page's learned
        // attributes. Placed before <short_memory> so the mostly-static part stays
        // in the cache prefix.
        // Everything is keyed by its ontology CURIE (`schema:name`, `schema:timezone`,
        // …) so the profile the agent reads mirrors the self-page's RDF-keyed
        // attributes rather than ad-hoc labels.
        let u = user;
        let mut profile = format!("schema:name: {} (@{})\n", u.name, u.handle);
        if let Some(tz) = u.timezone.as_deref().filter(|s| !s.is_empty()) {
            profile.push_str(&format!("schema:timezone: {tz}\n"));
        }
        if let Some(page) = self.repo.self_entity(&u.id).await?
            && let Some(map) = page.attributes.as_object()
        {
            // Name/timezone come authoritatively from the live `User` record above -
            // skip the plain keys and their CURIE forms so the block never repeats them.
            let live = ["name", "timezone", "schema:name", "schema:timezone"];
            let mut keys: Vec<&String> =
                map.keys().filter(|k| !live.contains(&k.as_str())).collect();
            keys.sort();
            for k in keys {
                let v = match &map[k] {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                profile.push_str(&format!("{k}: {v}\n"));
            }
        }
        sections.push(AgentContextSection::prose(
            AgentContextKind::UserProfile,
            "user_profile",
            profile.trim_end().into(),
        ));
        if let Some(section) = self.short_memory_block(&user.id).await? {
            sections.push(section);
        }
        if let Some(section) = self.playbook_index_block(&user.id, &vault).await? {
            sections.push(section);
        }
        Ok(sections)
    }
}

/// Take whole lines while they fit `cap` (estimated tokens), in order. Returns the body
/// and how many lines did not fit.
///
/// Both callers pre-order their lines by what they most want kept - short memory by decay
/// score, the playbook index by `use_count` - so "in order, until it stops fitting" is the
/// whole rule, and it belongs in one place. What to do about the remainder is the
/// caller's, and the two answers genuinely differ: the playbook index owes the agent a
/// marker because `agent_section.md` promises it one, and short memory owes nothing
/// because decay already means it was never the whole set.
fn take_within_budget(lines: &[String], cap: usize) -> (String, usize) {
    let mut body = String::new();
    let mut used = 0usize;
    let mut shown = 0usize;
    for line in lines {
        let cost = estimate_tokens(line);
        if used + cost > cap {
            break;
        }
        used += cost;
        shown += 1;
        body.push_str(line);
    }
    (body, lines.len() - shown)
}

/// Assemble the `<available_playbooks>` body from pre-rendered, use_count-ordered lines:
/// take lines until the token `cap`, then - if any were dropped - append a **non-silent**
/// marker so the agent knows the list is a hot subset (not the whole set) and falls back to
/// `memory_search` rather than assuming an unlisted playbook doesn't exist. `None` if empty
/// or nothing fits.
#[cfg(test)]
fn render_playbook_index(lines: &[String], cap: usize) -> Option<String> {
    let (mut body, dropped) = take_within_budget(lines, cap);
    if body.is_empty() {
        return None;
    }
    if dropped > 0 {
        body.push_str(&format!(
            "- (+{dropped} more playbook(s) not shown — memory_search by task to find them)\n"
        ));
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_prompt_routes_every_foreground_graph_tool() {
        let prompt = include_str!("../../../../../resources/prompts/pkm/agent_section.md");
        for tool in ["memory_search", "memory_graph_get", "memory_graph_sparql"] {
            assert!(prompt.contains(tool), "agent prompt does not route {tool}");
        }
        for removed in [
            "memory_graph_find",
            "memory_schema_search",
            "memory_schema_inspect",
            "memory_graph_query",
        ] {
            assert!(
                !prompt.contains(removed),
                "agent prompt still routes {removed}"
            );
        }
        assert!(prompt.contains("last completed consolidation"));
        assert!(prompt.contains("Only returned entity paths are user facts"));
    }

    #[test]
    fn playbook_index_no_marker_when_all_fit() {
        let lines = vec![
            "- A — do a\n  /a.md\n".to_string(),
            "- B — do b\n  /b.md\n".to_string(),
        ];
        let out = render_playbook_index(&lines, 10_000).unwrap();
        assert!(out.contains("- A — do a"));
        assert!(out.contains("- B — do b"));
        assert!(
            !out.contains("more playbook"),
            "no truncation marker when all fit"
        );
    }

    #[test]
    fn playbook_index_marks_truncation_non_silently() {
        let line = "- Some Playbook — a description here\n  /data/x/y.md\n".to_string();
        let lines = vec![line.clone(); 5];
        let per = estimate_tokens(&line);
        let out = render_playbook_index(&lines, per * 2 + 1).unwrap();
        assert!(
            out.contains("(+3 more playbook(s) not shown"),
            "marker with dropped count:\n{out}"
        );
        assert!(
            out.contains("memory_search"),
            "marker points at the fallback:\n{out}"
        );
    }

    #[test]
    fn playbook_index_none_when_empty() {
        assert!(render_playbook_index(&[], 1_000).is_none());
    }

    fn lines(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("- line {i}\n")).collect()
    }

    /// Lines are taken whole and in order - a line that does not fit ends the take rather
    /// than being split or skipped over in favour of a later one that would fit. Both
    /// callers order their input by what they most want kept, so "stop" and "skip" are not
    /// interchangeable: skipping would silently prefer a low-scoring short memory over the
    /// high-scoring one that overflowed.
    #[test]
    fn taking_stops_at_the_first_line_that_does_not_fit() {
        let ls = lines(5);
        let per = estimate_tokens(&ls[0]);
        let (body, dropped) = take_within_budget(&ls, per * 2 + 1);
        assert_eq!(body, "- line 0\n- line 1\n", "the two that fit, in order");
        assert_eq!(dropped, 3);
    }

    #[test]
    fn everything_fitting_drops_nothing() {
        let (body, dropped) = take_within_budget(&lines(3), 10_000);
        assert_eq!(dropped, 0);
        assert_eq!(body.lines().count(), 3);
    }

    /// A cap too small for even the first line yields an empty body, not a partial one -
    /// which is what lets both callers treat "empty" as "omit the block entirely".
    #[test]
    fn cap_below_the_first_line_yields_nothing() {
        let (body, dropped) = take_within_budget(&lines(3), 1);
        assert!(body.is_empty());
        assert_eq!(
            dropped, 3,
            "all of them, so a marker would report the full count"
        );
    }

    #[test]
    fn no_lines_is_empty_and_drops_nothing() {
        assert_eq!(take_within_budget(&[], 1_000), (String::new(), 0));
    }
}
