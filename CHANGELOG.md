# Changelog

## Unreleased

### Added
- Project folder with Git, README and CHANGELOG. The README links to the authoritative Vision.md (SEARCH-01 to SEARCH-09). No code yet.
- `docs/designs/rust-evidence-engine.md`: approved M0 design from office hours. It records the route (Rust-native, eval-first), the agreed targets, the contracts for result semantics, cache freshness, untrusted content, source access, cost and comparison, and the M1 validation matrix. An independent review ran 2 rounds; the final fixes are not yet re-verified by the reviewer.

### Changed
- Engineering plan review of the design (2026-10-11). It confirmed the R2-1 to R2-5 office-hours fixes, and you approved D2 to D18. These add: a 3-crate workspace; one engine daemon that owns the budget, cache and ledger, reached by Pi over loopback HTTP with an Origin check, a bearer token and TLS when exposed; durable, type-enforced paid-call reservations; `degraded` and `no_match` status rules with required/optional stages; classification through the swappable Judge slot; quota tracking and origin-scoped free API keys; a byte-capped cache with separate discovery and content TTLs; a sealed acceptance set with a locked analysis plan; a paired grader-agreement gate; comparative cost per system; and a judge influence boundary. An outside reviewer model contributed 8 of these findings. The review record and implementation tasks T1 to T11 are at the end of the design doc.
- `TODOS.md` added with one P3 item: CI and multi-arch image builds once a remote is approved.
