use std::path::Path;

pub fn detect_content_type(filename: &str) -> &'static str {
    let basename = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .to_ascii_lowercase();
    match basename.as_str() {
        "dockerfile" | "containerfile" => return "text/x-dockerfile",
        "makefile" | "gnumakefile" => return "text/x-makefile",
        ".env" | ".gitignore" | ".gitattributes" | ".gitmodules" | ".dockerignore"
        | ".editorconfig" | ".npmrc" | ".yarnrc" => return "text/plain",
        name if name.starts_with(".env.") => return "text/plain",
        _ => {}
    }

    let ext = Path::new(&basename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "tar" => "application/x-tar",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "rb" => "text/x-ruby",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "cxx" | "hpp" => "text/x-c++",
        "md" | "markdown" => "text/markdown",
        "txt" | "log" | "conf" | "cfg" | "ini" | "env" | "properties" => "text/plain",
        "csv" => "text/csv",
        "yaml" | "yml" => "text/yaml",
        "toml" => "text/toml",
        "sh" | "bash" | "zsh" => "text/x-shellscript",
        "sql" => "text/x-sql",
        "dockerfile" => "text/x-dockerfile",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "mkv" => "video/matroska",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

pub fn is_image_content_type(content_type: &str) -> bool {
    content_type.starts_with("image/")
}

pub fn is_text_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type == "application/xml"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_common_types() {
        assert_eq!(detect_content_type("photo.png"), "image/png");
        assert_eq!(detect_content_type("photo.jpg"), "image/jpeg");
        assert_eq!(detect_content_type("doc.pdf"), "application/pdf");
        assert_eq!(detect_content_type("code.rs"), "text/x-rust");
        assert_eq!(detect_content_type("data.json"), "application/json");
        assert_eq!(detect_content_type("readme.md"), "text/markdown");
        assert_eq!(
            detect_content_type("unknown.xyz"),
            "application/octet-stream"
        );
    }

    #[test]
    fn text_config_and_build_names_match_frontend_classification() {
        for (filename, expected) in [
            ("server.log", "text/plain"),
            ("app.conf", "text/plain"),
            ("app.cfg", "text/plain"),
            ("app.ini", "text/plain"),
            ("app.env", "text/plain"),
            ("app.properties", "text/plain"),
            (".env", "text/plain"),
            (".env.production", "text/plain"),
            (".gitignore", "text/plain"),
            (".gitattributes", "text/plain"),
            (".gitmodules", "text/plain"),
            (".dockerignore", "text/plain"),
            (".editorconfig", "text/plain"),
            (".npmrc", "text/plain"),
            (".yarnrc", "text/plain"),
            ("Dockerfile", "text/x-dockerfile"),
            ("Containerfile", "text/x-dockerfile"),
            ("Makefile", "text/x-makefile"),
            ("GNUmakefile", "text/x-makefile"),
            ("CMakeLists.txt", "text/plain"),
        ] {
            for name in [
                filename.to_string(),
                format!("workspace/{}", filename.to_ascii_uppercase()),
                format!("workspace\\{}", filename.to_ascii_uppercase()),
            ] {
                assert_eq!(detect_content_type(&name), expected, "{name}");
                assert!(is_text_content_type(detect_content_type(&name)), "{name}");
            }
        }
    }

    #[test]
    fn additional_media_mime_types_match_frontend_without_implying_codec_support() {
        for (extension, expected) in [
            ("ogg", "audio/ogg"),
            ("oga", "audio/ogg"),
            ("opus", "audio/ogg"),
            ("flac", "audio/flac"),
            ("aac", "audio/aac"),
            ("m4a", "audio/mp4"),
            ("m4v", "video/mp4"),
            ("ogv", "video/ogg"),
            ("mov", "video/quicktime"),
            ("mkv", "video/matroska"),
            ("avi", "video/x-msvideo"),
        ] {
            for filename in [
                format!("media.{extension}"),
                format!("folder/MEDIA.{}", extension.to_ascii_uppercase()),
                format!("folder\\MEDIA.{}", extension.to_ascii_uppercase()),
            ] {
                assert_eq!(detect_content_type(&filename), expected, "{filename}");
                assert!(!is_text_content_type(expected));
            }
        }
    }

    #[test]
    fn existing_source_documents_and_binary_fallbacks_are_preserved() {
        for (filename, expected) in [
            ("code.ts", "text/typescript"),
            ("code.tsx", "text/typescript"),
            ("code.rs", "text/x-rust"),
            ("code.py", "text/x-python"),
            ("script.mjs", "text/javascript"),
            ("README.MD", "text/markdown"),
            ("README.markdown", "text/markdown"),
            ("dashboard.HTML", "text/html"),
            ("dashboard.htm", "text/html"),
            ("config.JSON", "application/json"),
            ("schema.xml", "application/xml"),
            ("settings.yaml", "text/yaml"),
            ("package.toml", "text/toml"),
            ("archive.zip", "application/zip"),
            ("unknown.bin", "application/octet-stream"),
            ("unknown", "application/octet-stream"),
            (".unknown", "application/octet-stream"),
            ("config.env.bin", "application/octet-stream"),
            (".env/unknown.bin", "application/octet-stream"),
            ("folder.txt/unknown", "application/octet-stream"),
            ("folder.txt\\unknown", "application/octet-stream"),
        ] {
            assert_eq!(detect_content_type(filename), expected, "{filename}");
        }
    }

    #[test]
    fn image_and_text_detection() {
        assert!(is_image_content_type("image/png"));
        assert!(is_image_content_type("image/jpeg"));
        assert!(!is_image_content_type("text/plain"));

        assert!(is_text_content_type("text/plain"));
        assert!(is_text_content_type("text/x-rust"));
        assert!(is_text_content_type("application/json"));
        assert!(!is_text_content_type("image/png"));
    }
}
