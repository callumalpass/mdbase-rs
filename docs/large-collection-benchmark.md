# Large collection setup/startup benchmark

This is a local, synthetic **engine/runtime** benchmark for the TaskNotes large-collection investigation. It does not connect to a daemon, LAB, staging, production, or a user's collection. It complements `phase0-baseline` (general queries/mutations/concurrency) by exercising application contract setup and its nested runtime transaction path.

Initial measurements, including a settlement-pending failure under a 20-second runtime budget: [2026-09-16 baseline](../benchmarks/large-collection-startup-2026-09-16.md).

## Build and smoke test

Requires Rust from `rust-toolchain.toml`, Python 3, and Linux (child address-space limits and `/proc` memory observations).

```bash
cargo build --locked --release --bin large-collection-benchmark -j 2
python scripts/test-benchmark-large-collections.py

python scripts/benchmark-large-collections.py \
  --manifest /path/to/tasknotes-app/src/generated/mdbase-app.json \
  --output /path/to/new-results/smoke \
  --notes 20 --noise-files 20 --page-size 7
```

Always use the release binary. The binary emits whether debug assertions are enabled. The smoke matrix tests all five fixture variants, first/continuation pages, zero new schema diagnostics, managed upgrades, and byte-for-byte preservation of every original note/non-Markdown file.

The manifest is supplied rather than duplicated into this repository. It is the public bundled application declaration, **not** a credential/configuration file. Results retain its exact bytes and SHA-256. Only packs providing a required contract are selected, matching Connect's initial required-contract setup selection; optional scratchpad packs are not installed in this workload.

## Baseline matrix

```bash
python scripts/benchmark-large-collections.py \
  --manifest /path/to/tasknotes-app/src/generated/mdbase-app.json \
  --output /path/to/new-results/baseline \
  --notes 1000 10000 \
  --noise-files 10000 \
  --repeats 3
```

Default note bodies are 1 KiB, noise payloads 4 KiB, in subdirectories of 100 files. Sizes are configurable. The runner executes cases sequentially; every case/repetition gets a fresh fixture and process.

| Case | Task fraction | Non-Markdown files |
|---|---:|---|
| `markdown` | 100% | None |
| `binary` | 100% | Non-UTF-8 `.bin` files |
| `json` | 100% | Valid ordinary JSON, not schema files |
| `excluded-json` | 100% | Same JSON under an excluded directory |
| `mixed` | 20% | Same binary noise as `binary`; remaining notes are not tasks |

This separates note count, matching task count, directory traversal, JSON staging, and exclusions. Binary noise models file enumeration, **not** PDF parsing/thumbnailing/streaming. Notes have valid TaskNotes metadata, no recurrence, and deterministic unique IDs. This deliberately avoids conflating setup costs with recurrence materialization or a dense link graph.

## Measured phases

1. Fixture generation (reported separately; not product startup time).
2. Initialize an existing folder of notes using `init_collection`.
3. Resource-only `Collection::open` on the unprovisioned collection.
4. Fresh `FilesystemRuntime::open`, including watcher startup.
5. Open/establish a change-feed baseline.
6. Assess TaskNotes contract installation via the runtime provider's collection boundary.
7. Apply installation through `FilesystemRuntime::execute_with_context`, **including** runtime prepare/staging/commit/reconciliation, not just the inner direct collection method.
8. Drop/reopen the provisioned runtime; on-disk cache from setup remains.
9. Query all `task` records with effective frontmatter and bodies, requesting 1,000 items; time first and each continuation page, total traversal, and serialized engine-result bytes. Verify exact count and no duplicates.
10. Repeat without bodies as an explicitly **warm** comparison, not a controlled cold A/B trial.
11. Assess already-current setup.
12. Assess/apply a synthetic managed-contract upgrade; reassess as current.
13. Verify SHA-256 of every original note/noise file.

The upgrade changes the managed contract's Markdown body, recalculates its resource digest, and sets the task pack version to `99.0.0`. It preserves contract semantics/bindings. **It is not a historical TaskNotes version migration**, and does not benchmark changing record schemas or existing-type adoption conflicts. Use an additional explicit fixture for those workflows rather than interpreting this timing as coverage of every migration.

Actual page sizes may be lower than requested: the baseline engine clamps retained read pages to 256 items. At 10,000 tasks that yields **40 pages**, not 10. The report records observed counts. Payload bytes are serialized engine results, not HTTP/encrypted wire bytes.

## Deadlines and watchdogs

The measurement default is a generous 300,000 ms runtime operation budget, to observe cost rather than censor measurements at the product timeout. The independent default watchdog kills a case after 360 seconds in one phase, including non-cooperative setup code. Try product-sized budgets separately:

```bash
python scripts/benchmark-large-collections.py \
  --manifest /path/to/tasknotes-app/src/generated/mdbase-app.json \
  --output /path/to/new-results/deadline-30s \
  --notes 10000 --cases markdown binary json \
  --deadline-ms 30000 --phase-timeout 90
```

`--deadline-ms` applies to runtime calls (each query page gets a fresh budget); it does **not** pretend that direct assessment or runtime construction supports deadlines. Assessment is protected by the external watchdog. A failed install stops that case; missing subsequent phases are not zero-time successes. Timeout/abort stderr and partial events remain available.

Applying a 20-second engine budget is only a diagnostic experiment, **not** proof of a TaskNotes browser timeout: the real request also includes connection/authorization, serialization, encryption, transport, and SDK cancellation behavior.

## Safety and artifacts

- Output must be a **new** directory. Existing output paths are refused.
- The Rust worker creates a **new** collection directory; existing collection paths are refused.
- Each fixture and all engine temporary staging live inside a runner-owned temporary directory. They are removed on success, error, watchdog timeout, or normal Python exception cleanup.
- Default child address-space cap is 4 GiB; core dumps are disabled. This is not an RSS quota. A memory-limit abort is recorded as a failed run, not an application correctness defect.
- Disk headroom is checked before the matrix and polled during each case. The default floor is 1 GiB; an additional conservative fixture/staging allowance is required at startup. A triggered disk guard stops the matrix.
- Children run with reduced CPU priority. No daemon/browser is opened or stopped. No OS caches are dropped.
- The runner snapshots the binary and benchmark source at startup so rebuilding cannot change later cases mid-matrix. Build immediately before running to ensure those sources match the binary.
- `metadata.json`: engine revision, dirty status, source/binary/manifest hashes, toolchain, host/load/memory conditions, arguments, and measurement caveats.
- `*.jsonl`, `*.stderr.log`, `*.config.json`: per-case progress/failures and exact fixture dimensions. Configs reference disposable directories that no longer exist after cleanup.
- `results.json`, `summary.md`: completed case results, updated after each case.
- `manifest.json`, `benchmark-binary`, `benchmark-source.rs`, `runner-source.py`: reproducibility inputs. Keep bulky run artifacts outside Git.

`rss_kib` is Linux resident memory at an event boundary. `process_hwm_kib` is the process-lifetime high-water mark; it is **not** a peak attributable to a single phase. If the worker is killed mid-phase, the last emitted memory observation may understate its final peak. Fixture hashes, schemas, runtime caches, and allocation retention are included in RSS.

## Interpretation and remaining coverage

These are observations, not CI timing gates. Repeat at least three times on a quiet host before judging a regression. Fresh fixture/runtime does **not** mean cold OS cache: files were just generated. The initial pinned baseline revision was `2389527`; it was intentionally not advanced to the newer remote branch. Record/compare revisions explicitly before applying findings to a deployed release.

This harness does not measure Connect's registry admission/authorization/grant setup, relay limits, hosted storage, browser memory/rendering, JavaScript task decoding, SDK retries, auto-archive, or rolling recurrence. The next layer is the same fixture matrix through an isolated LAB connector and the actual TaskNotes repository/browser, with first-visible-task and fully-ready markers. That layer must follow the LAB safety workflow and remain separate from this deterministic local benchmark.
