use super::FileError;
use super::FileOutput as ToolOutput;
use frona_text::walk_with_ignore;
use std::path::Path;
const MAX_RESULTS: usize = 1000;

pub fn glob(resolved: &Path, scope_input: &str, pattern: &str) -> Result<ToolOutput, FileError> {
    if !resolved.is_dir() {
        return Ok(ToolOutput::error(format!(
            "glob scope must be a directory; got file {}",
            scope_input
        )));
    }

    let matcher = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| FileError::Validation(format!("invalid glob pattern: {e}")))?
        .compile_matcher();

    let mut results = Vec::new();
    let mut truncated = false;
    for path in walk_with_ignore(resolved) {
        let rel = path.strip_prefix(resolved).unwrap_or(&path);
        if matcher.is_match(rel) {
            results.push(rel.display().to_string());
            if results.len() >= MAX_RESULTS {
                truncated = true;
                break;
            }
        }
    }

    let mut body = if results.is_empty() {
        format!("No matches for pattern {} under {}.", pattern, scope_input)
    } else {
        results.join("\n")
    };
    if truncated {
        body.push_str(&format!(
            "\n\n[truncated at {} matches; narrow your pattern]",
            MAX_RESULTS
        ));
    }
    Ok(ToolOutput::text(body))
}
