use serde::Serialize;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Root {
    /// The downloaded frona-ontologies release. Replaced wholesale on upgrade.
    Release,
    /// Ontologies the user added. Trusted fully - it is their knowledge base - but
    /// checked for the one thing that would silently break the walk.
    User,
}
