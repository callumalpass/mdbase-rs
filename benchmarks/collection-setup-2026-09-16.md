# Collection setup simplification — 2026-09-16

## Scope and revisions

This follows the [initial large-collection investigation](large-collection-startup-2026-09-16.md), but compares against **current main `fc1dc07`**, not the older `2389527` engine. The benchmark adaptation is committed as `03308dd`; engine changes are `ca283b7` plus the reference-path correction `cc09a92` on `perf/collection-setup`. Final measurements are built at `4919ca4`, which additionally isolates test-only crash injection by commit identity.

The current-main measurements were made at `75fe81f` with only the benchmark adaptation dirty (subsequently committed as `03308dd`). No engine changes were present in that baseline. Both versions use the same required-contract TaskNotes declaration, SHA-256 `5b5de7d37a23ec33fc0b2ca4fca0ae46e060e5e74ffb41b3586cc8ad93e99840`.

All collections were disposable synthetic fixtures. No Connect daemon, real collection, browser, hosted environment, staging, or production was modified. This is not a deployed-release or end-to-end TaskNotes qualification.

## What changed

1. **One full setup workspace, one transaction owner.** The setup planner keeps its staged collection and exact desired bytes together. Runtime preparation consumes those directly instead of creating an outer full shadow and executing an inner migration transaction. The existing durable runtime journal still owns publication, concurrency checks, crash recovery, reconciliation, and settlement. Direct collection apply retains its existing migration commit owner. Both paths share review/conflict/downgrade checks. Changed setups validate the captured baseline and staged candidate; current setups still receive full assessment.
2. **Only stage actual definition dependencies.** Held-root type and contract loaders no longer walk the entire collection and copy every JSON file. They stage definition documents and their explicit local schema references, preserving the canonical parsers and diagnostics. Shared mutation staging likewise excludes unrelated JSON while retaining declared dependencies and recognized schema resources. Legal referenced schemas outside conventional schema directories, including within definition directories, remain available. Intervening directories needed by paths such as `detour/../schema.json` are preserved without copying their unrelated contents; missing directories and symlinks are not invented or followed. No schema reference is fetched remotely or read outside held-root authority.
3. **Do not validate ordinary notes as views.** Snapshot discovery previously compiled and ran the full canonical view schema against every Markdown record. The schema requires the exact `type: view` discriminator, so other records can be rejected immediately. Candidate view records still receive full validation; invalid views remain records as before. This removed much of the cost of runtime open and post-commit settlement as well as preparation.
4. **Preserve typed failures.** Shadow helpers inherit the caller's existing operation context rather than taking an unbounded-read branch. File/depth capture failures now remain recorded like the other capture limits. Regression tests also exposed a pre-existing panic when definition/resource mutations encountered a commit conflict; the canonical invalid-outcome constructor now handles those mutation families.

No new persisted format, public configuration, operation protocol, background job system, task replica, or timeout increase was introduced. Definition-only parser workspaces remain; this is one **full-collection** setup workspace, not a claim that the engine creates only one temporary directory.

## Measurements

The comparison below uses the original current-main sample and **medians of three fresh optimized 10k-note runs** under a 20-second per-operation budget. Times are seconds; apply includes both preparation and commit/settlement, but assessment is listed separately.

| Phase | Markdown baseline | Markdown optimized | +10k JSON baseline | +10k JSON optimized |
|---|---:|---:|---:|---:|
| Runtime open | 6.91 | 1.12 | 11.56 | 1.20 |
| Assess installation | 2.84 | 2.48 | 7.92 | 2.55 |
| Install preparation | 11.66 | 3.02 | 19.28 | 2.99 |
| Install commit/settlement | 10.15 | 1.17 | 13.34 | 1.17 |
| **Install total** | **21.81** | **4.17** | **32.62** | **4.15** |
| Provisioned runtime reopen | 8.28 | 1.25 | 11.80 | 1.34 |
| Assess already current | 1.68 | 1.59 | 2.11 | 1.85 |
| Assess upgrade | 3.62 | 3.52 | 7.64 | 3.52 |
| Upgrade preparation | 12.34 | 4.00 | 19.26 | 4.15 |
| Upgrade commit/settlement | 10.15 | 1.14 | 13.18 | 1.16 |
| **Upgrade total** | **22.49** | **5.12** | **32.44** | **5.31** |

Medians of subphases need not sum exactly to the median total. The repeated Markdown installation range was 4.11–5.33s; JSON 4.14–4.60s. Upgrade ranges were 5.07–5.49s and 5.08–5.43s respectively. All six budget-probe cases passed, including both installation and upgrade. No timeout was increased.

The separate five-variant matrix also passed all **10 cases**:

| Notes | Variant | Runtime open | Install | Upgrade | Process peak MiB |
|---:|---|---:|---:|---:|---:|
| 1,000 | Markdown | 0.29 | 1.25 | 1.06 | 54.3 |
| 1,000 | Binary noise | 0.42 | 1.44 | 1.28 | 61.6 |
| 1,000 | JSON noise | 0.56 | 1.44 | 1.25 | 61.6 |
| 1,000 | Excluded JSON | 0.39 | 1.32 | 1.36 | 60.0 |
| 1,000 | Mixed / 20% tasks | 0.42 | 1.28 | 0.61 | 60.1 |
| 10,000 | Markdown | 1.19 | 5.77 | 6.70 | 306.5 |
| 10,000 | Binary noise | 1.76 | 6.06 | 6.57 | 320.5 |
| 10,000 | JSON noise | 2.04 | 5.71 | 6.89 | 320.2 |
| 10,000 | Excluded JSON | 2.49 | 5.78 | 6.34 | 316.2 |
| 10,000 | Mixed / 20% tasks | 2.20 | 4.67 | 4.59 | 307.7 |

The matrix was slower than the later repetitions: retain both rather than treating the best observation as a guarantee. Original current-main process peaks were 336.0 MiB for Markdown and 447.1 MiB with JSON; repeated optimized runs were 313.9–319.7 MiB. These are whole-process high-water marks, not peaks attributable to setup alone.

The optimized matrix and repeated probes used binary SHA-256 `a02b67a437625acf2d029911767050dee39d4fb8e78c33453e548738f137a14c` at `4919ca4`; the original baseline binary is `f292320e3929b1e35b5327dac6eb0a55a2edcc2022ce87216cfff8c3c9beb1c3`. Subsequent `fc3bd0b` routes directly to the same closed definition constructor and records the reviewed architecture inventory; it does not change the measured setup algorithm.

A fresh 10k-note +10k-JSON confirmation on the delivered `fc3bd0b` code also passed the 20-second operation budget: runtime open **1.26s**, installation **4.58s**, upgrade **5.28s**. Its retained binary SHA-256 is `c7de0e4012c4a60631d6072d76714576f987d41a0a62241043a007de67091c34` (`delivered-smoke/`).

### Attribution from intermediate experiments

The single-stage/dependency-only checkpoint reduced installation from 21.8s to 17.9s for Markdown and from 32.6s to 17.8s with unrelated JSON. Settlement still took about 9.4s. Adding the view discriminator reduced settlement to about 1.1s and runtime open to about 1.1–1.2s. This is why removing copies alone was insufficient.

These intermediate observations are retained under `single-stage` and `discriminator`; they are experiments, not additional release candidates. The final committed engine is the source of the final matrix and budget-probe results above.

## Correctness and verification

- `cargo test --locked --release --workspace --all-features -j 2`: **699 passed, 1 ignored** across 43 test groups, including the pinned historical and v0.3 conformance suites. Fixtures came from `f4202a5d76f189684bfb2dc34334b8e3a8017ccc`, selected explicitly with `MDBASE_SPEC_REPO_DIR` and `MDBASE_SPEC_TESTS_DIR`.
- `cargo test --locked --release -p mdbase --no-default-features --lib -j 2`: **415 passed, 1 ignored**.
- Workspace/all-target/all-feature Clippy and canonical no-default-feature/all-target Clippy: **passed with `-D warnings`**.
- Formatting, diff whitespace checks, and the architecture/debt-budget checker: **passed**.
- Python benchmark harness safety tests: **9 passed**.
- Final performance matrix: **10/10 passed**; repeated 20-second probes: **6/6 passed**; delivered-code 10k JSON confirmation: **passed**.

Live PostgreSQL was **not** qualified locally: no test database was configured, so its opt-in cases returned without exercising a server. Cross-platform, portable black-box, packaging, and live PostgreSQL qualification remain CI/release gates, not claims made by these local checks.

New/extended regression coverage includes:

- exactly one full shadow for a changed runtime setup; no authority writes or inner migration journal during preparation;
- durable prepared setup reattachment after runtime restart, unchanged assessment/receipt shape, and preserved original note bytes;
- current setup without a full shadow;
- stale review rejection and commit-time conflict rejection without a panic or partial publication;
- inner schema-file capture limits and cancellation remaining typed failures;
- only required schema dependencies staged, every canonical contract schema wrapper covered, fragment refs handled without recursive external resolution, missing refs left to canonical diagnostics, and symlink/escape rejection;
- required schemas both outside and inside definition directories, unrelated JSON omitted;
- valid view resources, malformed views, ordinary notes, malformed frontmatter, and array-valued view lookalikes retaining their prior classification.

The broader existing transaction, root-replacement, crash/recovery, watcher, schema validation, and conformance suites also remain in the verification gate. A no-default-features run exposed interference between parallel crash tests: their single process-global fault slot could overwrite another transaction's injection. The test-only hook is now keyed by commit, with a deterministic two-commit regression test. This does not change shipping recovery behavior.

## Architecture budget review

The reviewed Rust inventory grows from 173 to 175 files: the disposable benchmark worker and one shared definition-workspace owner. The measured line ceiling is 101,501. The worker accounts for 512 lines; much of the remaining growth is explicit transaction, dependency, cancellation, and crash-injection regression coverage. Oversized-file ceilings track the measured changes rather than granting spare headroom, and the contract-loader ceiling decreases.

Ambient-I/O ownership is moved out of the two loaders into the shared helper, whose ambient writes are limited to its private workspace. Collection authority reads remain on `CollectionRoot`. The benchmark's fixture-only I/O is separately inventoried. No new ambient authority acquisition or compatibility owner is allowed: runtime setup constructs the existing closed definition outcome directly, and the wire-only constructor/variant inventories remain unchanged.

## Reproduction and retained evidence

Use [the benchmark guide](../docs/large-collection-benchmark.md). Build with the pinned toolchain and `--locked --release`; use a new output directory. The apply phase now records preparation and commit/settlement separately under **one shared operation deadline**, rather than giving each phase a fresh budget.

```bash
cargo build --locked --release --bin large-collection-benchmark -j 2
python scripts/benchmark-large-collections.py \
  --manifest /path/to/tasknotes-app/src/generated/mdbase-app.json \
  --output /path/to/new-results/verified-matrix --notes 1000 10000
python scripts/benchmark-large-collections.py \
  --manifest /path/to/tasknotes-app/src/generated/mdbase-app.json \
  --output /path/to/new-results/deadline-20s \
  --notes 10000 --cases markdown json --repeats 3 \
  --deadline-ms 20000 --phase-timeout 90
```

Local artifacts are under `/home/calluma/worktrees/collection-setup/results/`:

- `current-main-baseline/`: original current-main binary, source, manifest, metadata, and raw observations;
- `single-stage/`, `discriminator/`, `discriminator-source/engine.patch`: intermediate attribution evidence;
- `verified-matrix/`: final committed-engine 1k/10k five-variant matrix;
- `verified-deadline-20s/`: three fresh 10k Markdown/JSON repetitions under the 20-second budget;
- `delivered-smoke/`: 10k JSON confirmation after the closed-outcome boundary/inventory cleanup.

The earlier `final-matrix/` and `final-deadline-20s/` also passed but precede the final schema-reference directory-traversal correction; use the `verified-*` runs for the delivered engine.

Each successful case verifies every original note/noise file byte-for-byte, exact task counts, pagination without duplicates, zero new schema diagnostics, and current setup after installation/upgrade. Fixtures and their temporary staging are cleaned; reports, exact executables, manifests, and sources remain.

## Limits and follow-up

- Observational timings on a shared host, not CI thresholds. Baseline has one sample per case; the repeated final budget probes show candidate variation but do not turn that baseline into a statistical regression gate. Host load and disk headroom changed between runs; swap was nearly full. No OS caches were dropped, and freshly generated fixtures are not cold-cache tests.
- The upgrade changes a managed contract's Markdown body/digest and pack version while preserving semantics. It is **not** a historical schema migration or existing-type adoption/conflict benchmark.
- Setup still performs collection-wide validation and keeps before/after snapshots and baseline/desired bytes. Ordinary readiness assessment, large task result bodies, and TaskNotes' full-cache/recurrence gating remain optimization opportunities. Do not remove those checks or introduce a persistent task replica merely to improve a timing.
- Next integration gate: qualify the candidate through isolated LAB/Connect/browser tests for first-visible-task, full readiness, interruption/reconnection, and both local/hosted providers before release adoption. Connect transport/authorization and TaskNotes UI behavior were not measured here.
