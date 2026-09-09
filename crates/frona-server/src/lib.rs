extern crate self as frona;

pub mod agent;
pub mod api;
pub mod app;
pub mod auth;
pub mod call;
pub mod chat;
pub mod contact;
pub mod core;
pub mod credential;
pub mod db;
pub mod inference;
pub mod memory;
pub mod notification;
pub mod policy;
pub mod scheduler;
pub mod space;
pub mod storage;
pub mod tool;

pub use frona_derive::{ChannelFactory, Entity, migration};

/// Initialize the process TLS provider before constructing HTTP clients.
pub fn initialize_tls() {
    // Both aws-lc-rs and ring are enabled by dependencies, so select explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("Failed to build shared HTTP client")
}
