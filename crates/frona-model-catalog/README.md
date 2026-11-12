# frona-model-catalog

Load model capabilities, pricing, routes, and parameter metadata independently of
model execution. The crate provides validated snapshots, bounded HTTP downloads,
conditional refresh, atomic cache persistence, and per-source status.

## Install

```toml
[dependencies]
frona-model-catalog = "0.1"
```

## Read a local catalogue

```no_run
use frona_model_catalog::{ModelCatalogStore, loader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let json = std::fs::read_to_string("catalog.json")?;
    let store = ModelCatalogStore::new(loader::parse(&json)?);
    let snapshot = store.current();
    if let Some(model) = snapshot.lookup_for_provider("openai", "gpt-5-mini") {
        println!("Tool calling: {}", model.supports_function_calling());
    }
    Ok(())
}
```

The parser accepts the models.dev catalogue format with a top-level `providers`
map. Consumers retain immutable snapshots while `ModelCatalogStore::swap`
replaces the current snapshot.

## Downloads and refresh

`CatalogSources::load` selects local cached data; `load_with_bundled` additionally
accepts a directory of bundled documents. `download_missing` fetches absent data,
`download_all` requires both sources to validate successfully, and `refresh_due`
refreshes sources independently while retaining valid data on failure.

From a source checkout, download validated documents with the included binary:

```sh
cargo run -p frona-model-catalog -- download --output data/system/cache
```

A valid local snapshot, even stale, needs no network request during startup.
Checksums detect corruption, not publisher authenticity. Runtime and image cache
documents use the same format. The parameter validation schema is embedded;
model catalogues are downloaded or supplied by the application.

The caller owns credentials, provider execution, local model selection, and
scheduling refresh. Protocol identities and route labels remain upstream strings;
consumers choose which protocols they implement. There are no optional Cargo features.

See the [API reference](https://docs.rs/frona-model-catalog) for source status,
parameter validation, snapshots, and model cost helpers.

## License

MIT. See `LICENSE`. Dependencies and downloaded data have their own licenses.
