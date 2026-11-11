//! Pure canonical page rendering copied from the server projection.
use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::Utc;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};

use crate::ontology::PrefixMap;

use super::PageLayout;
use crate::model::{EntityCategory, KnowledgeEntity, KnowledgeEntityLink};

pub fn canonicalize_wikilinks(
    markdown: &str,
    redirects: &BTreeMap<String, String>,
    scope: &PageLayout,
) -> String {
    if redirects.is_empty() || !markdown.contains("[[") {
        return markdown.to_string();
    }
    static WIKILINK: OnceLock<regex::Regex> = OnceLock::new();
    let wikilink = WIKILINK.get_or_init(|| {
        regex::Regex::new(r"\[\[([^\[\]|#]+)((?:#[^\[\]|]*)?(?:\|[^\[\]]*)?)\]\]")
            .expect("wikilink expression is valid")
    });
    let vault_prefix = format!("{}/", scope.directory());
    wikilink
        .replace_all(markdown, |captures: &regex::Captures<'_>| {
            let target = &captures[1];
            let (path, prefixed) = match target.strip_prefix(&vault_prefix) {
                Some(path) => (path, true),
                None => (target, false),
            };
            let canonical = canonical_path(path, redirects);
            if canonical == path {
                return captures[0].to_string();
            }
            let target = if prefixed {
                scope.vault_path(&canonical)
            } else {
                canonical
            };
            format!("[[{target}{}]]", &captures[2])
        })
        .into_owned()
}

pub fn canonical_path(path: &str, redirects: &BTreeMap<String, String>) -> String {
    crate::view::resolve_path(path, |path| Ok(redirects.get(path).map(String::as_str)))
        .unwrap_or(path)
        .to_string()
}

/// A parsed markdown page body: lenient, language-agnostic syntax cleanup already
/// applied, plus the structural facts `compose_page` needs. Produced by
/// [`MarkdownPage::parse`] in a single parser pass.
pub struct MarkdownPage {
    /// The cleaned body - whole-document code fence unwrapped, leading frontmatter
    /// dropped, trimmed.
    pub body: String,
    /// Whether `body` opens with an ATX heading (i.e. it carries its own title).
    pub has_title: bool,
}

impl MarkdownPage {
    /// Parse and clean a raw model (or deterministic) body. Single parser pass for the
    /// cleanup; `has_title` is a cheap syntax check on the result.
    pub fn parse(raw: &str) -> Self {
        let body = clean(raw);
        let has_title = opens_with_heading(&body);
        Self { body, has_title }
    }
}

/// Lenient, single-pass, language-agnostic cleanup of an authored body. Inspects the
/// first markdown block: if the whole document is one code fence, return its inner
/// text (the model wrapped everything in ```); if it's a leading YAML frontmatter
/// block, drop it and keep the rest. Otherwise leave it untouched. Keys on markdown
/// *syntax* only - never on heading text - so it is locale-independent.
fn clean(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
    let mut iter = Parser::new_ext(trimmed, opts).into_offset_iter();
    match iter.next() {
        Some((Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))), _)) => {
            let mut inner = String::new();
            for (ev, _) in iter.by_ref() {
                match ev {
                    Event::Text(t) => inner.push_str(&t),
                    Event::End(TagEnd::CodeBlock) => {
                        return if iter.next().is_none() {
                            inner.trim().to_string()
                        } else {
                            trimmed.to_string()
                        };
                    }
                    _ => {}
                }
            }
            trimmed.to_string()
        }
        Some((Event::Start(Tag::MetadataBlock(_)), _)) => {
            for (ev, range) in iter {
                if matches!(ev, Event::End(TagEnd::MetadataBlock(_))) {
                    return trimmed[range.end..].trim_start().to_string();
                }
            }
            trimmed.to_string()
        }
        _ => trimmed.to_string(),
    }
}

/// True if `body` opens with an ATX heading of any level (`#` … `######`) - i.e. it
/// already carries its own title. Pure markdown syntax (a run of 1–6 `#` followed by
/// whitespace), so it's language-independent and, unlike a bare `# ` check, accepts a
/// `##`-level lead heading without double-titling.
fn opens_with_heading(body: &str) -> bool {
    let Some(first) = body.lines().find(|l| !l.trim().is_empty()) else {
        return false;
    };
    let first = first.trim_start();
    let hashes = first.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && first[hashes..].starts_with(|c: char| c.is_whitespace())
}

/// Extract just the human-editable body of an existing page. Frontmatter is removed, as
/// is the legacy machine-generated `## History` ledger when reading an older projection.
/// Fed to the writer on a dirty page via sync `adopt` so user prose is preserved.
pub fn extract_body(page: &str) -> String {
    let after_fm = page
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n").map(|i| &rest[i + 5..]))
        .unwrap_or(page);
    let mut body = after_fm.trim_start();
    if body.starts_with("# ")
        && let Some(nl) = body.find('\n')
    {
        body = body[nl + 1..].trim_start();
    }
    let mut end = body.len();
    if let Some(i) = body.find("\n## History") {
        end = end.min(i);
    }
    body[..end].trim().to_string()
}

/// Compose the full page: YAML frontmatter (metadata + `attributes` + typed
/// `[[wikilinks]]` grouped by relation) plus the authored article. Superseded memories
/// remain durable in the database and are integrated selectively by Page Author.
pub fn compose_page(
    page: &KnowledgeEntity,
    article: &MarkdownPage,
    attributes: &serde_json::Value,
    links: &[KnowledgeEntityLink],
    prefixes: &PrefixMap,
    scope: &PageLayout,
) -> String {
    let category = match page.category {
        EntityCategory::Concept => "concept",
        EntityCategory::Playbook => "playbook",
    };
    let mut s = String::new();
    s.push_str("---\n");
    // The page's immutable record id - the file's durable identity, independent of
    // its (mutable) path. Lets recovery match a file to its DB page after a rename,
    // so a crash-orphaned file is relocated (not rebuilt). See `reconcile_files`.
    s.push_str(&format!("uid: {}\n", page.id));
    // CURIEs, in array form. Display-only: frontmatter is re-derived on every
    // write and `MarkdownPage::parse` reads only `body`/`has_title`, so it never
    // round-trips and a prefix shift is cosmetic.
    s.push_str(&format!(
        "type: [{}]\n",
        prefixes.display_joined(&page.kinds)
    ));
    s.push_str(&format!("title: {}\n", page.name));
    s.push_str(&format!(
        "description: {}\n",
        page.description.replace('\n', " ")
    ));
    s.push_str(&format!("path: {}\n", page.path));
    s.push_str(&format!("category: {category}\n"));
    s.push_str(&format!("use_count: {}\n", page.use_count));
    s.push_str(&format!("updated: {}\n", Utc::now().to_rfc3339()));
    if !page.related_playbooks.is_empty() {
        s.push_str("related_playbooks:\n");
        for path in &page.related_playbooks {
            s.push_str(&format!("  - \"[[{}]]\"\n", scope.vault_path(path)));
        }
    }

    // Attributes as machine-parseable frontmatter keys (JSON scalars are
    // valid YAML, so quoting/typing is preserved).
    if let Some(map) = attributes.as_object().filter(|m| !m.is_empty()) {
        s.push_str("attributes:\n");
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for k in keys {
            let v = serde_json::to_string(&map[k]).unwrap_or_else(|_| "null".into());
            s.push_str(&format!("  {}: {v}\n", yaml_key(k)));
        }
    }

    let mut by_rel: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for l in links {
        by_rel
            .entry(l.relation.as_str())
            .or_default()
            .push(l.to_entity_path.as_str());
    }
    // Links are emitted as absolute vault-root wikilinks - `[[<directory>/<to>]]`
    // - so they resolve in Obsidian and by the agent's root-prepend rule. The
    // stored `to_entity_path` is clean; the directory is the render prefix.
    for (rel, tos) in &by_rel {
        s.push_str(&format!("{}:\n", yaml_key(rel)));
        for to in tos {
            s.push_str(&format!("  - \"[[{}]]\"\n", scope.vault_path(to)));
        }
    }
    s.push_str("---\n\n");

    // The model authors its own title; if it opened with prose instead of a
    // heading, add one (`article.has_title` already accounts for any ATX level,
    // so a `##` lead heading isn't double-titled).
    if !article.has_title {
        s.push_str(&format!("# {}\n\n", page.name));
    }
    s.push_str(article.body.trim());
    s.push_str("\n\n");

    if !page.related_playbooks.is_empty() {
        s.push_str("## Related Playbooks\n\n");
        for path in &page.related_playbooks {
            s.push_str(&format!("- [[{}]]\n", scope.vault_path(path)));
        }
        s.push('\n');
    }

    s
}

/// Render a frontmatter mapping key safely. A CURIE key (`schema:name`,
/// `frona:worksFor`) is valid YAML-LD but its colon trips Obsidian's Properties UI
/// and naive parsers, so quote any key containing a `:`; plain keys stay bare (so
/// existing free-text attribute keys render unchanged).
fn yaml_key(key: &str) -> std::borrow::Cow<'_, str> {
    if key.contains(':') {
        std::borrow::Cow::Owned(format!("\"{key}\""))
    } else {
        std::borrow::Cow::Borrowed(key)
    }
}

/// The sync `rev` of a rendered page - `sha256` (hex) of its exact file bytes.
/// The CAS token on edit and the change-detector in the manifest; opaque to the
/// client (the server defines it, the client only compares).
pub fn sha256_hex(bytes: &str) -> String {
    let mut h = Sha256::new();
    h.update(bytes.as_bytes());
    hex::encode(h.finalize())
}
