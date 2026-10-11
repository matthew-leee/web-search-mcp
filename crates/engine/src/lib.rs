//! Web-search evidence engine core (decision D2).
//!
//! This library is the heart of the long-lived engine daemon (decision D3): it
//! owns retrieval, extraction, judging, evidence packaging and the durable
//! budget ledger that gates every paid call. The `mcp` crate is only the
//! transport shell and the `eval` crate is the evaluation harness; neither owns
//! state. The `paid` module is the one place billing clients may be built.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod controller;
pub mod fetch;
pub mod judge;
pub mod package;
pub mod paid;
pub mod retrieve;
pub mod store;
