# frona-text

Unicode-aware matching and editing, quote grounding with original-source spans,
ignore-aware filesystem traversal, prompt templates, and configurable redaction.

## Install

```toml
[dependencies]
frona-text = "0.1"
```

`NormalizedString` keeps original byte alignment through normalization.
`GroundingText` finds supporting text while preserving the matched source span.
`apply_edit` edits matched text; `walk_with_ignore` searches filesystem trees while
respecting ignore rules.

```rust
use frona_text::NormalizedString;

let mut text = NormalizedString::from("Straße");
text.nfkc().case_fold();
assert_eq!(text.get(), "strasse");
assert_eq!(text.get_original(), "Straße");
```

## Features

All features are opt-in.

| Feature | Purpose |
|---|---|
| `prompts` | Embedded/file prompt loading, rendering, and variable contracts |
| `redaction` | Text and JSON redaction with caller-selected policy |
| `serde` | Enables the optional `serde` dependency; adds no public API |

For prompt templates:

```toml
frona-text = { version = "0.1", features = ["prompts"] }
```

```rust
# #[cfg(feature = "prompts")]
# {
use frona_text::prompt::{PromptLoader, PromptTemplate};

let loader = PromptLoader::embedded([("greeting", "Hello {{name}}")]);
let template = PromptTemplate::new("greeting", &["name"]);
assert_eq!(template.render(&loader, &[("name", "Casey")]).unwrap(), "Hello Casey");
# }
```

`PromptTemplate` checks required invocation variables, including missing, extra,
and duplicate names. Additional defaults can provide context but cannot satisfy a
missing required argument; invocation values override defaults case-insensitively.
Redaction is best effort and requires the caller to choose how sensitive fields
and configured secrets are handled.

See the [API reference](https://docs.rs/frona-text) for editing, grounding, prompt,
and redaction interfaces.

## License

MIT. See `LICENSE`. Dependencies have their own licenses.
