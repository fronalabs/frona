mod core;
mod loading;
pub mod roots;
mod scope;
mod search;
pub use core::OntologyCatalogue;
pub use scope::{OntologyScope, VocabHit};

pub use loading::{format_of, ontology_files};

#[cfg(test)]
mod tests;
