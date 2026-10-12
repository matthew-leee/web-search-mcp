# web-search-mcp

Final goal: a production-ready, cost-controlled web-search and evidence system for AI coding and research work, as useful as paid search services on an agreed set of test queries. A basic search wrapper or a list of links is not the final deliverable.

## Goal and requirements

The authoritative goal is the project's Vision.md in the shared notes:
[C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md](file:///C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md)

It lists nine requirements (SEARCH-01 to SEARCH-09). Its measurable targets are deliberately left open until they are agreed with the user: which kinds of queries, which reference services, quality thresholds, speed and cost budgets.

## Status

M1 in progress. The Rust workspace and the durable budget ledger (task T1 part A) exist; everything else is still a placeholder. An approved design doc sets the route and the measurable targets: [docs/designs/rust-evidence-engine.md](docs/designs/rust-evidence-engine.md).

- **Route:** a Rust evidence engine with eval first. It has pluggable retrievers (SearXNG, plus vertical sources such as GitHub, Stack Exchange, package registries, arXiv/OpenAlex and SEC EDGAR, plus capped Exa/Parallel fallback), swappable judges (Jev, a local reranker or a heuristic) and an evaluation harness from M1.
- **Targets agreed in office hours:** Parallel and Exa as references; six query categories (version-specific docs, errors, releases, technical, scientific and market research); "statistically tied" defined as demonstrated equivalence at lower total cost; quick mode about 5-10 s and deep mode up to 1-2 min; local Docker first. The monthly budget cap will be set after M1 measurements.

The engineering plan review is done (2026-10-11, decisions D2-D18). Its record and the implementation tasks T1-T11 are at the end of the design doc. Next step: harvest the eval queries (see the design doc's Assignment), then start the M1 workspace skeleton. Installing the Rust toolchain and any live or paid evaluation spend need explicit approval.

## Build and test

Rust is installed inside WSL2 Ubuntu only (Decisions S007). From Windows:

```sh
wsl.exe -e bash -lc 'cd /mnt/c/Users/mattC/playground/web-search-mcp && . ~/.cargo/env && export CARGO_TARGET_DIR=$HOME/.cache/wsm-target && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'
```

WSL needs `build-essential` and `pkg-config` (`sudo apt install -y build-essential pkg-config`). All tests are offline.

## Structure

- `crates/engine/`: library. `store::ledger` is the SQLite budget ledger: durable, fail-closed reservations with a hard monthly cap per budget. The other modules (controller, retrieve, fetch, judge, package, paid) are placeholders.
- `crates/mcp/`: `wsm-mcp` binary (placeholder; it will become the MCP transport).
- `crates/eval/`: `wsm-eval` harness (placeholder). `mcp` never depends on it.

- `docs/ROADMAP.md`: milestones M0 to M4, the M1 build steps and tasks T1 to T11, with status.
- `docs/designs/`: approved design docs (gstack office-hours output, with the eng review record appended).
- `TODOS.md`: deferred work items.

## Working rules

- Remote: https://github.com/matthew-leee/web-search-mcp (branch `main`).

- Git for every change, and this README and CHANGELOG.md kept current. Commit a meaningful README update before every push; nothing is pushed without approval.
- Automated tests must not call live search or model APIs; live checks need separate approval and a spending bound.
