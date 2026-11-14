//! Complete, runnable examples.
//!
//! Each section renders the example source. Run examples from a source checkout
//! with `cargo run -p frona-harness --example NAME`. These listings are compiled
//! as Cargo examples rather than standalone doctests.
//!
//! Scripted examples use the shared [support](#support) module and need no provider
//! credentials. The `rig` example uses `OPENAI_API_KEY`; `jsonl` needs
//! `--features jsonl`.

//!
//! ## basic
//!
//! Minimal text and typed runs. No session, store, events, identities, or tools to set up.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/basic.rs")]
//! ```

//!
//! ## budgets
//!
//! Reuse one allowance across outer runs and nested model calls inside a tool.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/budgets.rs")]
//! ```

//!
//! ## jsonl
//!
//! Save application state beside a stopped dialogue; load first, then explicitly resume.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/jsonl.rs")]
//! ```

//!
//! ## manual
//!
//! Choose individual actions and the next model while the SDK keeps history and pairing.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/manual.rs")]
//! ```

//!
//! ## progress
//!
//! Optional observation and a spawned owned operation. The final result is authoritative.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/progress.rs")]
//! ```

//!
//! ## refinement
//!
//! Borrow mutable inspector state; accept a different type or return unfinished retention.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/refinement.rs")]
//! ```

//!
//! ## rig
//!
//! Use an existing Rig client with Frona's loops. Requires OPENAI_API_KEY.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/rig.rs")]
//! ```

//!
//! ## tools
//!
//! Annotate ordinary async functions and register them by name.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/tools.rs")]
//! ```

//!
//! ## workspace
//!
//! Opt into the read/glob bundle for one directory; default runs have no application tools.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/workspace.rs")]
//! ```

//!
//! ## support
//!
//! A deterministic application-owned client, used only by these examples.
//! Replace `model(...)` with `Model::builder(...)` to use a built-in provider.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/support/mod.rs")]
//! ```
