//! Evaluation harness and live reference clients (decision D2).
//!
//! The harness will own the eval workload, the Parallel/Exa reference clients
//! (behind the `paid` gate with a separate eval budget) and the statistical
//! comparison reports. It is never a dependency of `mcp`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
