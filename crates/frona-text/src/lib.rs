//! Shared text/search primitives for frona.

mod edit;
mod grounding;
mod line_ending;
mod normalized_string;
mod search;

pub use edit::{EditOutcome, apply_edit};
pub use grounding::{GroundingError, GroundingMatch, GroundingText};
pub use line_ending::LineEnding;
pub use normalized_string::NormalizedString;
pub use search::walk_with_ignore;
