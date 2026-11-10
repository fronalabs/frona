use super::FileError;
use super::{
    FileOutput as ToolOutput,
    image::{is_supported_image, prepare_image},
};
use std::{io, path::Path};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

// Scan only a small directory before sorting. If it exceeds either bound,
// omit the listing rather than display an unstable filesystem-order subset.
const MAX_DIRECTORY_ENTRIES: usize = 128;
const MAX_DIRECTORY_NAME_BYTES: usize = 16 * 1024;
const MAX_DIRECTORY_DISPLAY_ENTRIES: usize = 50;
const MAX_DIRECTORY_OUTPUT_BYTES: usize = 8 * 1024;

pub async fn read(
    resolved: &Path,
    path_arg: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ToolOutput, FileError> {
    let metadata = match tokio::fs::metadata(resolved).await {
        Ok(metadata) => metadata,
        Err(error) => return read_error(path_arg, error),
    };
    if metadata.is_dir() {
        return read_directory(resolved).await;
    }

    let bytes = match tokio::fs::read(resolved).await {
        Ok(bytes) => bytes,
        Err(error) => return read_error(path_arg, error),
    };
    let size = bytes.len();

    let mime = infer::get(&bytes).map(|t| t.mime_type().to_string());
    if let Some(ref m) = mime
        && is_supported_image(m)
    {
        return read_image(&bytes, m, path_arg);
    }

    match std::str::from_utf8(&bytes) {
        Ok(text) => Ok(read_text(text, offset, limit)),
        Err(_) => Ok(ToolOutput::Binary {
            media_type: mime.unwrap_or_else(|| "application/octet-stream".into()),
            size,
            path: path_arg.into(),
        }),
    }
}

fn read_error(path: &str, error: io::Error) -> Result<ToolOutput, FileError> {
    match error.kind() {
        io::ErrorKind::NotFound => Ok(ToolOutput::error(format!("file not found: {path}"))),
        io::ErrorKind::PermissionDenied
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::InvalidData
        | io::ErrorKind::NotADirectory
        | io::ErrorKind::IsADirectory => {
            Ok(ToolOutput::error(format!("Could not read {path}: {error}")))
        }
        _ => Err(FileError::Io(format!("read {path}: {error}"))),
    }
}

async fn read_directory(path: &Path) -> Result<ToolOutput, FileError> {
    const GUIDANCE: &str =
        "Read got a directory. Choose a file to read; directory names end with '/'.";
    const OMITTED: &str = "[Listing omitted: directory exceeds the scan limit (128 entries or 16 KiB of names). Use Glob with a narrower pattern to find a file.]";
    const TRUNCATED: &str =
        "\n[Listing truncated. Use Glob with a narrower pattern to find a file.]";

    let mut directory = match tokio::fs::read_dir(path).await {
        Ok(directory) => directory,
        Err(error) => return read_error("directory (could not list entries)", error),
    };
    let mut names = Vec::new();
    let mut name_bytes = 0;
    loop {
        let entry = match directory.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => return read_error("directory (could not list entries)", error),
        };
        if names.len() == MAX_DIRECTORY_ENTRIES {
            return Ok(ToolOutput::error(format!("{GUIDANCE}\n{OMITTED}")));
        }
        let mut name = entry.file_name().to_string_lossy().into_owned();
        let file_type = match entry.file_type().await {
            Ok(file_type) => file_type,
            Err(error) => return read_error("directory (could not list entries)", error),
        };
        if file_type.is_dir() {
            name.push('/');
        }
        name_bytes += name.len();
        if name_bytes > MAX_DIRECTORY_NAME_BYTES {
            return Ok(ToolOutput::error(format!("{GUIDANCE}\n{OMITTED}")));
        }
        names.push(name);
    }
    if names.is_empty() {
        return Ok(ToolOutput::error(format!(
            "{GUIDANCE}\n[Directory is empty.]"
        )));
    }
    names.sort();
    let mut output = GUIDANCE.to_string();
    for (index, name) in names.iter().enumerate() {
        // Quote/escape names so embedded newlines cannot masquerade as entries.
        let line = format!("\n{name:?}");
        if index == MAX_DIRECTORY_DISPLAY_ENTRIES
            || output.len() + line.len() + TRUNCATED.len() > MAX_DIRECTORY_OUTPUT_BYTES
        {
            output.push_str(TRUNCATED);
            break;
        }
        output.push_str(&line);
    }
    Ok(ToolOutput::error(output))
}

fn read_image(bytes: &[u8], mime: &str, path_arg: &str) -> Result<ToolOutput, FileError> {
    Ok(match prepare_image(bytes, mime) {
        Ok(image) => ToolOutput::mixed(format!("Read image file [{mime}]"), vec![image]),
        Err(reason) => ToolOutput::error(format!("{reason} at {path_arg} ({mime})")),
    })
}

fn read_text(text: &str, offset: Option<usize>, limit: Option<usize>) -> ToolOutput {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let start = offset.map(|n| n.saturating_sub(1)).unwrap_or(0);
    if start >= total_lines && total_lines > 0 {
        return ToolOutput::error(format!(
            "offset {} is past end of file ({} lines)",
            start + 1,
            total_lines
        ));
    }

    let start = start.min(total_lines);
    let end_unlimited = match limit {
        Some(l) => start.saturating_add(l).min(total_lines),
        None => total_lines,
    };

    let mut byte_count = 0usize;
    let mut last_line = start;
    for (i, line) in lines[start..end_unlimited].iter().enumerate() {
        let next = byte_count + line.len() + 1; // +1 for the trailing newline
        if i >= MAX_LINES || next > MAX_BYTES {
            break;
        }
        byte_count = next;
        last_line = start + i + 1;
    }

    let mut body = lines[start..last_line].join("\n");
    let truncated_by_cap = last_line < end_unlimited;
    let more_after_limit = end_unlimited < total_lines;

    if truncated_by_cap || more_after_limit {
        let shown_first = start + 1;
        let shown_last = last_line.max(start + 1);
        let next_offset = last_line + 1;
        body.push_str(&format!(
            "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
            shown_first, shown_last, total_lines, next_offset
        ));
    }

    if body.is_empty() {
        ToolOutput::text(String::new())
    } else {
        ToolOutput::text(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unexpected_io_failures_remain_internal() {
        assert!(matches!(
            read_error("file", io::Error::other("unexpected I/O failure")),
            Err(FileError::Io(_))
        ));
    }

    #[test]
    fn read_text_no_truncation() {
        let ToolOutput::Content { text, .. } = read_text("hello\nworld", None, None) else {
            panic!("expected text")
        };
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn read_text_offset_limit() {
        let text = "a\nb\nc\nd\ne";
        let ToolOutput::Content { text, .. } = read_text(text, Some(2), Some(2)) else {
            panic!("expected text")
        };
        // start at line 2 ("b"), 2 lines → "b\nc"
        assert!(text.contains("b\nc"));
        // more remain, so continuation hint
        assert!(text.contains("Use offset=4"));
    }

    #[test]
    fn read_text_offset_past_end() {
        let out = read_text("a\nb", Some(99), None);
        assert!(matches!(out, ToolOutput::Content { success: false, .. }));
    }

    #[test]
    fn is_image_branch() {
        assert!(is_supported_image("image/png"));
        assert!(is_supported_image("image/jpeg"));
        assert!(!is_supported_image("application/pdf"));
    }
}
