# Murmur (rebuild)

AI meeting notes for blue-collar field work. Rust core workspace.

- `crates/harness` — reusable agent harness (no app-specific logic)
- `crates/murmur-core` — domain entities + sync-ready SQLite storage (single-writer API, tombstones, UUIDv7)

Vision spec + plan series live in the Murmur meta repo under `docs/superpowers/`.

## Testing

`cargo test` — all tests are hermetic (MockProvider or wiremock); no network, no API keys.

## Plan series

Implementation plans 01–06 live in the Murmur meta repo at `docs/superpowers/plans/2026-07-01-rust-core-*.md`.
Done: 01 foundation, 02 memory + reflection + context assembler, 03 domain + storage, 04 processing pipeline + reflection coordinator, 05 live extraction, 05b eval suite, 06a source column + swap fix.
Next: 06 STT.

Evals: `cargo test -p evals` (hermetic, no key). Real-API:
`ANTHROPIC_API_KEY=sk-... cargo run -p evals --example eval -- --out report.json`.
