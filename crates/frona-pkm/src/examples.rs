//! Complete, runnable examples.
//!
//! Each section renders the example source. Run examples from a source checkout
//! with `cargo run -p frona-pkm --example NAME`. These listings are compiled
//! as Cargo examples rather than standalone doctests.
//!
//! Examples require `OPENAI_API_KEY`. The `consolidate` example also requires
//! `PKM_ONTOLOGY_DIR`.

//!
//! ## consolidate
//!
//! Run ontology-backed phases without a Frona server or database. Requires
//! OPENAI_API_KEY and PKM_ONTOLOGY_DIR (a directory containing ontology files).
//!
//! ```rust,ignore
#![doc = include_str!("../examples/consolidate.rs")]
//! ```

//!
//! ## ingest
//!
//! Extract a transcript without a Frona server or database. Requires OPENAI_API_KEY.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/ingest.rs")]
//! ```

//!
//! ## page_edit
//!
//! Author a page and interpret a human edit. Requires OPENAI_API_KEY.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/page_edit.rs")]
//! ```

//!
//! ## playbook
//!
//! Resolve and author a playbook without server storage. Requires OPENAI_API_KEY.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/playbook.rs")]
//! ```
