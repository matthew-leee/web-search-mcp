# Roadmap

Final goal: a production-ready, cost-controlled web-search and evidence system for AI coding and research. On an agreed workload it must be statistically tied with Parallel and Exa (demonstrated equivalence) at lower total cost. The authoritative source is Vision.md (SEARCH-01 to SEARCH-09) in the Knowledge vault. Every milestone delivers real, tested components; none of them replaces the final goal.

This page links three layers together:
- **milestones** M0 to M4, from Vision.md;
- the **M1 build steps**, from the design doc's "Next Steps";
- **tasks** T1 to T11, from the engineering review ("Implementation Tasks" in [designs/rust-evidence-engine.md](designs/rust-evidence-engine.md)).

Status values: done, in progress, not started. Last updated: 2026-10-11.

## Overview

| Milestone | Delivers (Vision) | Requirement IDs | Review tasks | Status |
|---|---|---|---|---|
| M0 | Requirements, targets, definition of done | all | none | done |
| M1 | Discovery/retrieval vertical slice plus eval harness | 01, 02, 07, 08, 09 | T1, T2, T3, T4, T9; eval parts of T5 and T10 | in progress |
| M2 | Query-aware extraction, ranking, evidence packaging, version/freshness handling | 02, 03, 05 | T7, freshness part of T8 | not started |
| M3 | Bounded automatic controller, swappable decisions, caching/accounting, failure isolation | 04, 05, 06, 07 | T6, cache-bound part of T8 | not started |
| M4 | Full quality/robustness evaluation, close gaps, deployment and operations | all | T11; acceptance-run parts of T5 and T10 | not started |

A task can span two milestones when part of it is needed early. In that case it is listed under both, with the part named.

## M0: Alignment (done)
- Office hours: route, references, workload, done bar, latency and runtime agreed (repo commit 2736b7f).
- Engineering review: decisions D2 to D18 and tasks T1 to T11 (commit 78c080a).

## M1: Retrieval slice and eval harness (in progress)

Build steps (design doc "Next Steps") and the tasks attached to each:

| # | Step | Tasks | Status |
|---|---|---|---|
| 1 | Approvals: Rust toolchain (done: WSL2), M1 eval spend cap, comparison and cost protocols | none | partly done |
| 2 | Eval set: harvest the 42 development queries with their must-have facts (user) | none | not started |
| 3 | Workspace skeleton: crates `engine`, `mcp`, `eval` | none | done (b6cdd83) |
| 3a | Budget ledger and paid-call gate | **T1**: part A ledger done (b6cdd83); part B paid gate not started | in progress |
| 4 | SearXNG compose (arm64), retriever plus fixtures, then GitHub and Stack Exchange connectors | **T9** quotas and optional keys | not started |
| 5 | Fetch/extract with safe URL handling, evidence packager v1, MCP quick tool | **T3** URL policy and key scoping; **T4** status rules; **T2** daemon, HTTP access control, TLS check | not started |
| 6 | Source access policy written in Sources.md and enforced; M1 validation matrix green (offline) | proofs from T2, T3, T4, T9 | not started |
| 7 | Exploratory capped baseline against Parallel/Exa (no parity claim); grade; record in Experiments.md | eval parts of **T5** (dev set, stats, blinding, grader gate) and **T10** (per-system cost views) | not started |

Before step 7 you must approve: the eval spend cap, the comparison and cost protocols, and the grader agreement metric and threshold.

## M2: Extraction, ranking, evidence quality
- **T7** Judge influence boundary: judges return only the IDs they were given; paid triggers stay inside code-set bounds; schema-valid adversarial fixtures.
- **T8** (freshness part) Separate discovery and content TTLs, with `searched_at`, `fetched_at` and `validated_at`.
- Extractor choice (Rust crate or a Trafilatura sidecar) and local reranker, chosen by measurement and recorded in Decisions.md.

## M3: Automatic controller and accounting
- **T6** Classification through the Judge slot (heuristic default, Jev selectable, flagged fallback). A heuristic-only router may land earlier if M1 routing needs it.
- **T8** (bound part) Byte-capped cache with LRU eviction; the ledger is never evicted.
- Paid fallback triggers wired to the T1 gate; cancellation with delayed-charge reconciliation.

## M4: Acceptance, deployment, operations
- **T5** (acceptance part) Sealed acceptance set, locked analysis plan, paired grader gate. This is the only run that can claim parity.
- **T10** (claim part) Lower-total-cost comparison under the approved cost protocol.
- **T11** CI and multi-arch images (also in TODOS.md).
- Load/soak tests, VPS deployment checks (TLS rule from D15), observability.

## Decisions waiting on the user
These are listed under Open Questions in the design doc. None blocks the code until M1 step 7.
- M1 eval spend cap and grading judge model.
- Equivalence margin, statistical test, sample size; sealed acceptance set construction.
- Grader agreement metric and threshold.
- Approval of the comparison and cost protocols.
- Market-research sources; robots/ToS policy.
