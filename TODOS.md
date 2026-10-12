# TODOS

## Infrastructure

### CI and multi-arch image builds

**What:** Add CI (fmt, clippy, `cargo test`, offline validation matrix) and arm64/amd64 container builds for the engine daemon.

**Why:** The design ships as Docker Compose on ARM64 now and as a VPS deploy later (SEARCH-07/08). Without CI, regressions in the offline contract tests only show up when someone runs them locally.

**Pros:** Every change is checked against the mocked validation matrix. Images for both architectures are reproducible, so the VPS move stays config-only.

**Cons:** Needs a hosted remote and a CI provider. Both require the user's approval and may become metered services.

**Context:** Accepted in /plan-eng-review D18 (2026-10-11), item 5. The repo has no remote yet (README, design doc "Distribution Plan"). Start once the user approves a remote: one workflow running the offline tests, then a buildx multi-arch image job. Publishing images is a separate approval.

**Effort:** S (human: ~1 day / CC: ~30 min)
**Priority:** P3
**Depends on:** user approval of a hosted remote and CI provider; M1 workspace skeleton (Next Steps 3).

## Cost control

### Bound-violation decision request (D19)

**What:** When a paid call reports `over_bound_micros: Some(_)`, the daemon records the violation durably and raises a decision request to the user (provider, reserved bound, actual charge, reservation id). It never disables the provider automatically.

**Why:** The user decided (D19, 2026-10-12) that a provider exceeding its code-set bound is disclosed and decided by the user. The gate already discloses it; nothing yet delivers the request.

**Context:** Gate side done in T1 part B (`engine::paid`). The request surface belongs with the daemon/controller (T2, M3). Open detail: how the request reaches the user (MCP response metadata, a status tool, or a log).

**Priority:** P2
**Depends on:** T2 daemon; the first real paid client.

## Completed
