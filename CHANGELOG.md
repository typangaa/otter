# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [1.0.0] - 2026-10-02

weir is now a task runner and GPU slot scheduler for jailed CLI agent workers.
The v0 orchestration surface is removed; see `docs/design-v1.md` for the
decision record.

### Removed (BREAKING)

- Engines: fan-out, pipeline, eval-loop, fusion and router (`src/engine/`).
- Resilience layer: retry, rate limit, circuit breaker and `ResilientBackend`.
- Metrics and their persistence (`observability/{metrics,persist}`).
- The `chat`, `backend` and `workflow` commands, including the TOML write-back
  subcommands, and the stdio-cli backend layer.
- The legacy v0.5 config schema. A config without `version = 2` is now rejected
  with a migration hint (see `examples/weir.v2.example.toml`).
- The `legacy/` Python prototype and the v0 example configs.
- Dependencies `toml_edit` and `async-trait`.

### Added

- Config v2 with strict resolution (`--config` > `$WEIR_CONFIG` > XDG > HOME,
  never `./weir.toml`), `deny_unknown_fields` and a worker allow-list.
- `weir validate --deep`: wrappers on PATH, working `bwrap`, agent-jail
  `WT_ROOTS` match, creatable `state_dir`.
- Typed exit codes classifying wrapper results (ok, check failed, timeout,
  empty, quota, denied, jail refused, usage, error).
- Spawn core: workers run in their own process group with capped output, hard
  deadlines and SIGTERM/SIGKILL escalation; `weir worker run`.
- `flock`-based GPU slot leases with affinity: `weir lease run` and
  `weir lease status`.
- `weir task run|show|list|clean`: fresh git worktree per task, jailed checks
  via `agent-jail`, patch capture and an append-only JSONL ledger
  (`weir.task/1`).
- Escalation ladder (`[ladder.NAME]`) with a persisted agy quota cooldown
  (`cooldown.json`).
- `weir status` shows the v2 config summary and active cooldowns; `weir schema`
  describes the v2 schema.
- GitHub release workflow publishing a Linux tarball on `vX.Y.Z` tags.
- Example Claude Code skill in `examples/skill/SKILL.md`.

### Fixed

- `validate --deep` retries a spawn that fails with `ETXTBSY`, which made the
  `cli::deep` unit tests flaky under parallel test threads.

### Changed

- Binary shrinks to about 1.8 MB with no HTTP, MCP or server code.
- Superseded v0 plan documents moved to `docs/archive/`.

## [0.5.1]

- Fixed a panic when debug-logging a prompt whose byte 200 fell inside a
  multi-byte character.

## [0.5.0] and earlier

- 0.5.0: call-time backend overrides for `workflow run`.
- 0.4.x: removed the MCP server and rebranded as a single-binary CLI agent
  orchestrator.
- 0.3.x and earlier: resilience layer, persisted metrics, API-key removal, HTTP
  removal. All superseded by 1.0.0.
