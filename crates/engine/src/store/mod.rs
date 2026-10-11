//! Durable state owned by the engine daemon.
//!
//! The [`ledger`] module is the single source of truth for what paid spend is
//! committed, so the monthly cap stays a hard limit across crashes, restarts
//! and concurrent clients.

pub mod ledger;
