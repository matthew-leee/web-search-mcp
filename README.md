# web-search-mcp

Final goal: a production-ready, cost-controlled web-search and evidence system for AI coding and research work, as useful as paid search services on an agreed set of test queries. A basic search wrapper or a list of links is not the final deliverable.

## Goal and requirements

The authoritative goal is the project's Vision.md in the shared notes:
[C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md](file:///C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md)

It lists nine requirements (SEARCH-01 to SEARCH-09). Its measurable targets are deliberately left open until they are agreed with the user: which kinds of queries, which reference services, quality thresholds, speed and cost budgets.

## Status

M1 in progress. The Rust workspace, the durable budget ledger (task T1 part A) and the type-enforced paid-call gate (task T1 part B) exist; everything else is still a placeholder. An approved design doc sets the route and the measurable targets: [docs/designs/rust-evidence-engine.md](docs/designs/rust-evidence-engine.md).

- **Route:** a Rust evidence engine with eval first. It has pluggable retrievers (SearXNG, plus vertical sources such as GitHub, Stack Exchange, package registries, arXiv/OpenAlex and SEC EDGAR, plus capped Exa/Parallel fallback), swappable judges (Jev, a local reranker or a heuristic) and an evaluation harness from M1.
- **Targets agreed in office hours:** Parallel and Exa as references; six query categories (version-specific docs, errors, releases, technical, scientific and market research); "statistically tied" defined as demonstrated equivalence at lower total cost; quick mode about 5-10 s and deep mode up to 1-2 min; local Docker first. The monthly budget cap will be set after M1 measurements.

The engineering plan review is done (2026-10-11, decisions D2-D18). Its record and the implementation tasks T1-T11 are at the end of the design doc. Next: harvest the eval queries (the design doc's Assignment) and continue M1 with T9 connectors and T2/T3/T4 (see [docs/ROADMAP.md](docs/ROADMAP.md)). Live or paid evaluation spend needs explicit approval.

## Build and test

Rust is installed inside WSL2 Ubuntu only (Decisions S007). From Windows:

```sh
wsl.exe -e bash -lc 'cd /mnt/c/Users/mattC/playground/web-search-mcp && . ~/.cargo/env && export CARGO_TARGET_DIR=$HOME/.cache/wsm-target && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo test -p engine paid::'
```

WSL needs `build-essential` and `pkg-config` (`sudo apt install -y build-essential pkg-config`). All tests are offline.

## Structure

- `crates/engine/`: library. `store::ledger` is the SQLite budget ledger: durable, fail-closed reservations with a hard monthly cap per budget. `paid` is the paid-call gate (see below). The other modules (controller, retrieve, fetch, judge, package) are placeholders.
- `crates/mcp/`: `wsm-mcp` binary (placeholder; it will become the MCP transport).
- `crates/eval/`: `wsm-eval` harness (placeholder). `mcp` never depends on it.

### Paid-call gate

Paid API calls go through `engine::paid::PaidGate`. It reserves the attempt's worst-case cost against a named budget (`operating`, or `eval` for eval reference clients) in the ledger *before* dispatching, and every dispatched outcome carries a `PaidCallRecord` (provider, reservation id, actual or estimated cost), the disclosure metadata the evidence package will attach. Billing clients implement the sealed `PaidProvider` trait inside `engine::paid` only; the gate hands them an unforgeable `Attempt` token carrying the ledger reservation id (the provider-side idempotency key). Over-cap calls return `SkippedBudget` and ledger failures `LedgerUnavailable`, both with zero provider requests. Unknown charges, timeouts and cancellation keep the liability open at its estimate until it is settled (`Ledger::settle_by_id`) or expired (`Ledger::expire_open_older_than`); each retry is a new call and a new reservation. Settlement reports the authoritative recorded charge via `SettleReceipt` (a racing or replayed settle is disclosed as `AlreadySettled`), and `PaidCallRecord::over_bound_micros` discloses a provider charge above the reserved bound (recorded as-is, never clamped). Such a bound violation is raised to the user for a decision; the provider is never disabled automatically (decision D19). No real provider clients exist yet.

- `docs/ROADMAP.md`: milestones M0 to M4, the M1 build steps and tasks T1 to T11, with status.
- `docs/designs/`: approved design docs (gstack office-hours output, with the eng review record appended).
- `TODOS.md`: deferred work items.

## Working rules

- Remote: https://github.com/matthew-leee/web-search-mcp (branch `main`).

- Git for every change, and this README and CHANGELOG.md kept current. Commit a meaningful README update before every push; nothing is pushed without approval.
- Automated tests must not call live search or model APIs; live checks need separate approval and a spending bound.
