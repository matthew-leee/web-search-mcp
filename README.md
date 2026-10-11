# web-search-mcp

Final goal: a production-ready, cost-controlled web-search and evidence system for AI coding and research work, as useful as paid search services on an agreed set of test queries. A basic search wrapper or a list of links is not the final deliverable.

## Goal and requirements

The authoritative goal is the project's Vision.md in the shared notes:
[C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md](file:///C:/Users/mattC/Documents/Knowledge/Projects/web-search-mcp/Vision.md)

It lists nine requirements (SEARCH-01 to SEARCH-09). Its measurable targets are deliberately left open until they are agreed with the user: which kinds of queries, which reference services, quality thresholds, speed and cost budgets.

## Status

Not started. There is no code yet, and no language or architecture has been chosen. Rust is the default unless another language has a significant, documented advantage.

Next step: agree the open targets in Vision.md (gstack office-hours, then plan reviews), then build in tested milestones.

## Working rules

- Git for every change, and this README and CHANGELOG.md kept current. Commit a meaningful README update before every push; nothing is pushed without approval.
- Automated tests must not call live search or model APIs; live checks need separate approval and a spending bound.
