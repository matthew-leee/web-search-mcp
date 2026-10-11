# web-search-mcp

Final goal: a production-ready, cost-controlled web-search and evidence system for AI coding and research work, as useful as paid search services on an agreed set of test queries. A basic search wrapper or a list of links is not the final deliverable.

## Goal and requirements

The authoritative goal is the project's Vision.md in the shared notes:
[C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md](file:///C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md)

It lists nine requirements (SEARCH-01 to SEARCH-09). Its measurable targets are deliberately left open until they are agreed with the user: which kinds of queries, which reference services, quality thresholds, speed and cost budgets.

## Status

M0 (alignment). There is no code yet. An approved design doc sets the route and the measurable targets: [docs/designs/rust-evidence-engine.md](docs/designs/rust-evidence-engine.md).

- **Route:** a Rust evidence engine with eval first. It has pluggable retrievers (SearXNG, plus vertical sources such as GitHub, Stack Exchange, package registries, arXiv/OpenAlex and SEC EDGAR, plus capped Exa/Parallel fallback), swappable judges (Jev, a local reranker or a heuristic) and an evaluation harness from M1.
- **Targets agreed in office hours:** Parallel and Exa as references; six query categories (version-specific docs, errors, releases, technical, scientific and market research); "statistically tied" defined as demonstrated equivalence at lower total cost; quick mode about 5-10 s and deep mode up to 1-2 min; local Docker first. The monthly budget cap will be set after M1 measurements.

Next step: harvest the eval queries (see the design doc's Assignment), then run engineering plan review before implementation. Installing the Rust toolchain and any live or paid evaluation spend need explicit approval.

## Structure

- `docs/designs/`: approved design docs (gstack office-hours output).

## Working rules

- Git for every change, and this README and CHANGELOG.md kept current. Commit a meaningful README update before every push; nothing is pushed without approval.
- Automated tests must not call live search or model APIs; live checks need separate approval and a spending bound.
