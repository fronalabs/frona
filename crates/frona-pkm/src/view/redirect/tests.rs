use super::*;
use std::collections::BTreeMap;
#[test]
fn chains_end_at_a_terminal_path_and_cycles_fail() {
    let redirects = BTreeMap::from([("a", "b"), ("b", "c")]);
    assert_eq!(
        resolve_path("a", |path| Ok(redirects.get(path).copied())).unwrap(),
        "c"
    );
    assert_eq!(
        resolve_path("missing", |path| Ok(redirects.get(path).copied())).unwrap(),
        "missing"
    );
    let cycle = BTreeMap::from([("a", "b"), ("b", "a")]);
    assert!(resolve_path("a", |path| Ok(cycle.get(path).copied())).is_err());
}
