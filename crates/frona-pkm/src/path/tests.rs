use super::{normalize_path, slugify};

/// A name written in any script has to reach a usable path. Before transliteration,
/// every non-ASCII character was replaced by a space - so a wholly non-Latin name
/// slugged to the empty string, `normalize_path` returned `None`, and `extract`
/// skipped the entity without a word. It only looked fine because the model tends to
/// romanise on its own.
#[test]
fn name_in_any_script_slugs_to_something_usable() {
    for (raw, want) in [
        ("田中", "tian-zhong"),
        ("Ренат", "renat"),
        ("Ελένη", "elene"),
        ("محمد", "mhmd"),
    ] {
        let got = slugify(raw);
        assert_eq!(got, want, "{raw}");
        assert!(
            normalize_path(&format!("people/{raw}")).is_some(),
            "{raw} must reach a path rather than being dropped"
        );
    }
}

/// The commoner half of the same bug: accented Latin was mangled rather than lost.
/// `"José García"` used to slug to `"jos-garc"`, because every accented letter became
/// a word break.
#[test]
fn accents_are_transliterated_rather_than_cut_out() {
    assert_eq!(slugify("José García"), "jose-garcia");
    assert_eq!(slugify("Renée"), "renee");
    assert_eq!(slugify("Ångström"), "angstrom");
}

/// Everything the slug already guaranteed still holds - it is a filesystem path and
/// a wikilink target, so it stays lowercase ASCII kebab with no traversal.
#[test]
fn slugs_stay_ascii_kebab_and_bounded() {
    assert_eq!(slugify("PostgreSQL 15 (prod)"), "postgresql-15-prod");
    assert_eq!(slugify("  spaced   out  "), "spaced-out");
    assert!(slugify("日本").is_ascii(), "always ASCII");
    assert!(
        slugify(&"あ".repeat(200)).chars().count() <= 60,
        "still capped"
    );
    assert_eq!(
        normalize_path("/a/../b.md"),
        Some("a/b".into()),
        "no traversal survives"
    );
}

/// A path with no alphanumeric content anywhere still yields `None` - the caller
/// logs and drops, which is the honest outcome when there is nothing to name a page.
#[test]
fn path_with_nothing_nameable_is_still_rejected() {
    assert_eq!(normalize_path("///"), None);
    assert_eq!(normalize_path("   "), None);
}
