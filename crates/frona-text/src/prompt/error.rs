use std::{error::Error, fmt};

/// A prompt could not be loaded, rendered, or satisfy its declared contract.
#[derive(Debug)]
pub enum PromptError {
    Read {
        name: String,
        source: std::io::Error,
    },
    Render {
        name: Option<String>,
        source: minijinja::Error,
    },
    Missing {
        name: String,
    },
    Empty {
        name: String,
    },
    Variables {
        name: String,
        expected: Vec<String>,
        supplied: Vec<String>,
    },
}
impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { name, source } => write!(f, "Read prompt {name}: {source}"),
            Self::Render {
                name: Some(name),
                source,
            } => write!(f, "Render prompt {name}: {source}"),
            Self::Render { name: None, source } => write!(f, "Template rendering failed: {source}"),
            Self::Missing { name } => write!(f, "prompt `{name}`: missing"),
            Self::Empty { name } => write!(f, "prompt `{name}`: empty"),
            Self::Variables {
                name,
                expected,
                supplied,
            } => write!(
                f,
                "prompt `{name}`: variable mismatch — declared {expected:?}, supplied {supplied:?}; variables must occur exactly once"
            ),
        }
    }
}
impl Error for PromptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Render { source, .. } => Some(source),
            _ => None,
        }
    }
}
