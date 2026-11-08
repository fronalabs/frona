//! Application validation over shared, successful tool observations.
use super::*;
use frona_harness::Decision;

pub(super) struct PlaybookInspection {
    pub ctx: Arc<ConsolidationContext>,
    pub ids: PromptIds,
    pub allowed: BTreeSet<String>,
    pub existing_by_path: BTreeMap<String, EntityCategory>,
    pub playbook_lookup: Arc<super::super::tools::PlaybookLookup>,
    pub staged_paths: BTreeSet<String>,
}

impl PlaybookInspection {
    pub fn inspect(
        &self,
        mut verdict: Verdict,
    ) -> Result<Decision<Vec<ResolvedPlaybook>>, AppError> {
        let mut errors = Vec::new();
        let mut claimed = BTreeSet::new();
        for target in &mut verdict.playbooks {
            if let Err(error) = self.ids.expand_all(&mut target.memory_ids) {
                errors.push(error.to_string());
            }
            target.memory_ids.sort();
            target.memory_ids.dedup();
            let Some(path) = normalize_path(&target.path) else {
                errors.push(format!("invalid target path `{}`", target.path));
                continue;
            };
            target.path = path;
            if target.name.trim().is_empty() || target.description.trim().is_empty() {
                errors.push(format!(
                    "target `{}` needs name and description",
                    target.path
                ));
            }
            if let Some(existing) = target.existing_path.as_mut() {
                let Some(path) = normalize_path(existing) else {
                    errors.push(format!("invalid existing path `{existing}`"));
                    continue;
                };
                *existing = path.clone();
                if self.existing_by_path.get(&path) != Some(&EntityCategory::Playbook) {
                    errors.push(format!("existing target `{path}` is not a Playbook"));
                } else if !self.staged_paths.contains(&path)
                    && !self
                        .playbook_lookup
                        .read_paths
                        .read()
                        .expect("playbook lookup poisoned")
                        .contains(&path)
                {
                    errors.push(format!(
                        "existing target `{path}` must be found and inspected with read_playbook first"
                    ));
                }
            } else if let Some(category) = self.existing_by_path.get(&target.path) {
                if *category != EntityCategory::Playbook {
                    errors.push(format!(
                        "target `{}` collides with a non-Playbook entity",
                        target.path
                    ));
                } else {
                    if !self.staged_paths.contains(&target.path)
                        && !self
                            .playbook_lookup
                            .read_paths
                            .read()
                            .expect("playbook lookup poisoned")
                            .contains(&target.path)
                    {
                        errors.push(format!(
                            "existing target `{}` must be found and inspected with read_playbook first",
                            target.path
                        ));
                    }
                    target.existing_path = Some(target.path.clone());
                }
            }
            for source in &mut target.merge_from {
                let Some(path) = normalize_path(source) else {
                    errors.push(format!("invalid merge source `{source}`"));
                    continue;
                };
                *source = path.clone();
                if self.existing_by_path.get(&path) != Some(&EntityCategory::Playbook) {
                    errors.push(format!("merge source `{path}` is not a Playbook"));
                } else if !self.staged_paths.contains(&path)
                    && !self
                        .playbook_lookup
                        .read_paths
                        .read()
                        .expect("playbook lookup poisoned")
                        .contains(&path)
                {
                    errors.push(format!(
                        "merge source `{path}` must be found and inspected with read_playbook first"
                    ));
                }
            }
            target.merge_from.sort();
            target.merge_from.dedup();
            for memory_id in &target.memory_ids {
                if !self.allowed.contains(memory_id) {
                    errors.push(format!(
                        "memory `{memory_id}` does not belong to this candidate"
                    ));
                } else if !claimed.insert(memory_id.clone()) {
                    errors.push(format!(
                        "memory `{memory_id}` is claimed by multiple targets"
                    ));
                }
            }
        }
        if errors.is_empty() {
            Ok(Decision::accept(verdict.playbooks))
        } else {
            let feedback = self.ctx.llm.reject(
                PromptSpec::PLAYBOOK_RESOLVE,
                &[("rejections", &errors.join("\n"))],
            )?;
            Ok(Decision::revise(feedback))
        }
    }
}

#[cfg(test)]
mod tests;
