use crate::page::{
    PageLayout,
    projection::{MarkdownPage, canonicalize_wikilinks, sha256_hex},
};
#[test]
fn copied_cleanup_preserves_markdown_semantics() {
    assert_eq!(
        MarkdownPage::parse("```markdown\n# Title\nbody\n```").body,
        "# Title\nbody"
    );
    assert_eq!(
        MarkdownPage::parse("---\nkey: value\n---\n\n# Title\nbody").body,
        "# Title\nbody"
    );
    assert!(MarkdownPage::parse("## Title\nbody").has_title);
    assert!(!MarkdownPage::parse("prose\n# later").has_title);
    assert_ne!(sha256_hex("text"), sha256_hex("text\n"));
}
#[test]
fn redirects_keep_labels_fragments_and_vault_prefixes() {
    let redirects = [
        ("old".into(), "middle".into()),
        ("middle".into(), "new".into()),
    ]
    .into();
    let result = canonicalize_wikilinks(
        "[[old|label]] [[Memory/old#section|label]] [[old-extra]]",
        &redirects,
        &PageLayout::default(),
    );
    assert_eq!(
        result,
        "[[new|label]] [[Memory/new#section|label]] [[old-extra]]"
    );
}

#[test]
fn cyclic_redirects_leave_original_links_intact() {
    let redirects = std::collections::BTreeMap::from([
        ("a".into(), "b".into()),
        ("b".into(), "c".into()),
        ("c".into(), "b".into()),
    ]);
    assert_eq!(
        super::super::projection::canonical_path("a", &redirects),
        "a"
    );
    assert_eq!(
        super::super::projection::canonicalize_wikilinks(
            "[[a]]",
            &redirects,
            &super::super::PageLayout::default()
        ),
        "[[a]]"
    );
}
