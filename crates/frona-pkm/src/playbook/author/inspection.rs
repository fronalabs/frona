use super::*;
/// Domain content inspection; only explicit acceptance can enter page projection.
pub(super) fn inspect_submission(
    candidate: AuthoredPlaybook,
    prompts: &PlaybookAuthorPrompts,
) -> Result<Decision<AuthoredPlaybook>, Error> {
    let mut missing = Vec::new();
    if candidate.name.trim().is_empty() {
        missing.push("name");
    }
    if candidate.description.trim().is_empty() {
        missing.push("description");
    }
    if candidate.body.trim().is_empty() {
        missing.push("body");
    }
    if missing.is_empty() {
        Ok(Decision::accept(candidate))
    } else {
        let feedback =
            prompts.reject(&[("rejections", &format!("missing: {}", missing.join(", ")))])?;
        Ok(Decision::revise(feedback))
    }
}

pub(super) fn require_requested_path(requested: &str, resolved: &str) -> Result<(), Error> {
    if requested == resolved {
        return Ok(());
    }
    Err(Error::Internal(format!(
        "playbook author: requested `{requested}` resolved to `{resolved}`; refusing to write the canonical page under a stale path",
    )))
}
