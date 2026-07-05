# Roadmap

Shared priorities and sequencing. Who's doing what, what's next, what's blocked.

Updated when priorities shift. Either person can propose changes via PR.

---

## Active

| Work | Owner | Status |
|------|-------|--------|
| **BUILD PAUSED 2026-07-05** — clean point: main @ baa8848, 277 tests, first real walk DONE (EST-0047 on sim) | — | resume order below |
| Repo re-unification Phase 2 (sitewalk → damsac/Murmur; runbook on Murmur main `docs/reunify/RUNBOOK.md`) | dam | gate satisfied, awaiting dam's go |
| Plan 08 build — STT stage-2 (mic → whisper → append) + noise Part C | dam | plan READY (2 review passes), build not started |
| Issue #2 — PR #1 review follow-ups (4 state bugs + seam hygiene) | sac | open (several now also guarded core-side by 07-carry) |
| iPhone T5 spike tier (device RTF/battery, ~1hr, `spikes/stt-whisper/ios/README.md`) | dam | the one unretired STT GO condition |

## Up Next (sequenced)

1. **STT stage-2** — mic audio → `crates/stt` → the existing append path = real voice walks. (06a/06/07 all DONE 2026-07-04: source column + swap fix, stt crate, live FFI bridge.)
2. **Accuracy hardening**: word-level timestamps (whisper token_timestamps) fix the coarse-seam fallback; live-prompt pins in evals advance with the Plan 06a contract.
3. **Prompt-optimization loop** on the 05b eval suite (rank on F0.5, gate on recall).
4. **Photo attachment schema** (rides a migration after `source`).
5. **07 carry notes**: doc-number gaps on failed retry; fallible MurmurEngine/begin_walk constructors; offline copy mislabel on model-skip; narrow the artifact sweep before any non-processing artifact writer exists.

## Decisions needed (joint)

- Template keys: adopt `landscape | property | inspection` as canonical? (dam: yes — needs sac's ack)
- STT DONE semantics: flush final utterance vs speed
- Fate of the Gallery/Screens static twins after design freeze

## Completed (rebuild era)

| Work | Date | Where |
|------|------|-------|
| Vision spec (4 revs) + UI mocks + user stories | 2026-07-01 | `damsac/Murmur` `pr/dam/rebuild-vision` |
| Plan 01 — harness foundation (agent loop, tools, providers) | 2026-07-01 | this repo, 14 commits |
| Plan 02 — memory/reflection/context (provenance, snapshots) | 2026-07-02 | this repo, 15 commits |
| Plan 03 — domain + SQLite store (tombstones, sync-ready) | 2026-07-02 | this repo, 14 commits |
| Plan 04 — processing pipeline + reflection coordinator + R9 cost log | 2026-07-03 | this repo, 16 commits |
| Plan 05 — live in-session extraction | 2026-07-03/04 | this repo, 6 commits |
| Plan 05b — eval suite (corpus + deterministic grader + runners) | 2026-07-04 | this repo, 8 commits |
| Memory frontier research / STT frontier research | 2026-07-02/04 | `damsac/Murmur` `docs/research/` |
| Repo → damsac org, public | 2026-07-04 | github.com/damsac/sitewalk |
| iOS app: design system + full flow behind WalkEngine seam | 2026-07-04 | PR #1 (sac), merged |
