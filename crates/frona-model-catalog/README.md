# Frona model catalog

External model metadata shared by the server and image build. This crate owns model and parameter records, schema validation, source digests, bounded HTTP downloads, conditional refresh, atomic persistence, source status and immutable snapshots.

It does not own credentials, provider execution, application configuration, Rig usage conversion or server metrics. Protocol identities and provider route labels remain upstream strings. Consumers decide which protocols they implement.

```sh
cargo test -p frona-model-catalog
cargo run -p frona-model-catalog -- download --output data/system/cache
```

The download command requires both sources to succeed and fully validate. The server uses `CatalogSources::load_with_bundled` to choose image or cached files, then `download_missing` before compiling inference configuration. The scheduler calls `refresh_due`; failures retain valid data. Normal inference only reads snapshots.

Runtime cache and image documents have the same format. Checksums detect corruption, not publisher authenticity. Downloads use HTTPS and bounded response sizes. A valid local snapshot, even stale, needs no network request during startup. Each source refreshes independently.

Catalog files are generated artifacts, never Rust `include_str!` inputs or committed snapshots. The published validation schema is versioned and embedded so schema validation is local.
