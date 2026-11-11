//! Reusable prompt loading and rendering. Explicit overrides take precedence over
//! directory files, which take precedence over embedded defaults. Only an absent
//! file falls through; malformed or unreadable overrides remain errors.
mod error;
mod loader;
mod render;
mod template;

pub use error::PromptError;
pub use loader::PromptLoader;
pub use render::render_template;
pub use template::PromptTemplate;

#[cfg(test)]
mod tests;
