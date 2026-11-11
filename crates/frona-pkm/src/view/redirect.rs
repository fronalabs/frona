use std::collections::HashSet;

use crate::Error;

/// Follow a caller's effective redirect view. Missing terminal entities are decided
/// by the caller; a malformed redirect or cycle is never a successful lookup.
pub(crate) fn resolve_path<'a>(
    path: &'a str,
    mut next: impl FnMut(&str) -> Result<Option<&'a str>, Error>,
) -> Result<&'a str, Error> {
    let mut current = path;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return Err(Error::Validation(format!(
                "entity redirect cycle at `{current}`"
            )));
        }
        match next(current)? {
            Some(target) => current = target,
            None => return Ok(current),
        }
    }
}

#[cfg(test)]
mod tests;
