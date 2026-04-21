//! Library crate so integration tests (and downstream consumers) can reach
//! the tool implementations and helpers directly. The binary entry point
//! (`main.rs`) is a thin shell that orchestrates these modules.
//!
//! This lib is intentionally minimal-doc: it exists to enable integration
//! tests against `GreenMail`, not as a stable public API. The pedantic
//! documentation lints (`missing_errors_doc`, `missing_panics_doc`,
//! `must_use_candidate`, `too_long_first_doc_paragraph`) are allowed at
//! crate level — the binary's own module docs already cover the intent
//! and the tool layer is the stable surface, not these raw helpers.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::too_long_first_doc_paragraph
)]

pub mod config;
pub mod email;
pub mod imap_client;
pub mod oauth2;
pub mod tools;
