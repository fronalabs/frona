//! Complete, runnable examples.
//!
//! Each section renders the example source. Run examples from a source checkout
//! with `cargo run -p frona-sandbox --example NAME`. These listings are compiled
//! as Cargo examples rather than standalone doctests.
//!
//! The standalone example requires `sh` and `sleep`. Pass `-- --disabled`
//! to run without isolation.

//!
//! ## standalone
//!
//! A consumer with no application services, identities, policy engine or database.
//!
//! ```rust,ignore
#![doc = include_str!("../examples/standalone.rs")]
//! ```
