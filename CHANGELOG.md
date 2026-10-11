# Changelog

## Unreleased

### Added
- Cargo workspace (D2): `engine` library, `wsm-mcp` and `wsm-eval` binaries (placeholders), and `rust-toolchain.toml`. Builds and tests in WSL2.
- `engine::store::ledger` (task T1 part A, D6/D13): a SQLite budget ledger in integer micro-USD.
  - A reservation is committed in one `BEGIN IMMEDIATE` transaction before its ticket is returned, so concurrent reservations cannot exceed the cap.
  - Liabilities survive restart; settlement is idempotent; expiry charges the estimate.
  - In-memory and non-WAL storage is refused. Concurrent first opens migrate safely.
  - The `Reservation` ticket cannot be built or cloned outside the ledger (compile-fail doc-tests).
  - 14 offline integration tests. Known gap: a storage failure on `reserve` after a successful `open` cannot be provoked through file permissions, so that path is verified by code inspection only.
- Project folder with Git, README and CHANGELOG. The README links to the authoritative Vision.md (SEARCH-01 to SEARCH-09). No code yet.
- `docs/designs/rust-evidence-engine.md`: approved M0 design from office hours. It records the route (Rust-native, eval-first), the agreed targets, the contracts for result semantics, cache freshness, untrusted content, source access, cost and comparison, and the M1 validation matrix. An independent review ran 2 rounds; the final fixes are not yet re-verified by the reviewer.

### Changed
- Engineering plan review of the design (2026-10-11). It confirmed the R2-1 to R2-5 office-hours fixes, and you approved D2 to D18. These add: a 3-crate workspace; one engine daemon that owns the budget, cache and ledger, reached by Pi over loopback HTTP with an Origin check, a bearer token and TLS when exposed; durable, type-enforced paid-call reservations; `degraded` and `no_match` status rules with required/optional stages; classification through the swappable Judge slot; quota tracking and origin-scoped free API keys; a byte-capped cache with separate discovery and content TTLs; a sealed acceptance set with a locked analysis plan; a paired grader-agreement gate; comparative cost per system; and a judge influence boundary. An outside reviewer model contributed 8 of these findings. The review record and implementation tasks T1 to T11 are at the end of the design doc.
- `TODOS.md` added with one P3 item: CI and multi-arch image builds once a remote is approved.
