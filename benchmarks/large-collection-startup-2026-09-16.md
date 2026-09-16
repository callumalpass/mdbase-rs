# TaskNotes large-collection baseline — 2026-09-16

Non-gating, single-sample local engine/runtime observations. Harness and reproduction commands: [large collection benchmark](../docs/large-collection-benchmark.md).

## Provenance and limits

- Engine: `23895275942458072d4858e52523c8239c84088e`, plus the benchmark binary source (no engine behavior changes).
- Rust 1.94.0, optimized release build, Linux x86_64.
- TaskNotes manifest SHA-256: `5b5de7d37a23ec33fc0b2ca4fca0ae46e060e5e74ffb41b3586cc8ad93e99840`.
- Benchmark Rust source SHA-256: `32cc8bdb7419be2e9be21e4976a8808f2662de7d1690556319813e008c1b51c8`.
- Binary SHA-256: `4a8951b1306fb95e02b845c958ad63ddbc4d2238791b1e1af8be7cff512d993d`.
- Matrix started `2026-09-15T23:55:35Z`; default 300-second per-runtime-operation measurement budget.
- 1 KiB note bodies; 10,000 non-Markdown files of approximately 4 KiB in each noise scenario, independent of note count.
- All notes are tasks except `mixed`, which has 20% tasks and binary noise.
- Fresh fixtures/processes, no OS-cache eviction; filesystem data was recently generated. Machine was not idle (initial load averages approximately 4.9/4.9/4.4 and swap almost full). Do not treat differences of a few seconds as established regressions.
- This pinned engine revision was behind remote main. Neither current deployment behavior nor full Connect/TaskNotes browser latency is established here.

## Results

All ten cases completed, with correct task counts, body inclusion/exclusion, no duplicate rows, no introduced setup diagnostics, and every original note/noise file unchanged.

Times below are seconds. Apply columns measure the coordinated runtime path, including its transaction work. Peak RSS is process-lifetime, not attributable solely to one phase.

| Notes | Noise / task mix | Runtime open | Assess install | Apply install | Assess current | Apply upgrade | Peak RSS MiB |
|---:|---|---:|---:|---:|---:|---:|---:|
| 1,000 | Markdown only | 0.90 | 0.23 | 1.56 | 0.14 | 1.65 | 50 |
| 1,000 | Binary | 0.87 | 0.31 | 2.54 | 0.24 | 2.62 | 58 |
| 1,000 | JSON | 0.87 | 0.92 | 3.00 | 0.30 | 4.96 | 214 |
| 1,000 | Excluded JSON | 0.97 | 0.22 | 3.37 | 0.13 | 2.84 | 52 |
| 1,000 | 20% tasks + binary | 1.00 | 0.22 | 3.61 | 0.14 | 3.09 | 57 |
| 10,000 | Markdown only | 7.16 | 1.96 | 19.43 | 1.31 | 19.54 | 303 |
| 10,000 | Binary | 6.36 | 1.99 | 18.40 | 1.38 | 18.96 | 305 |
| 10,000 | JSON | 6.45 | 2.61 | 19.41 | 1.46 | 20.52 | 422 |
| 10,000 | Excluded JSON | 6.43 | 1.90 | 17.93 | 1.30 | 18.93 | 305 |
| 10,000 | 20% tasks + binary | 7.26 | 1.30 | 18.63 | 0.62 | 18.43 | 274 |

At 10,000 tasks, local query traversal with bodies took 0.32–0.34 seconds and produced approximately 16.1 MB of serialized engine result JSON. Without bodies it produced approximately 5.8 MB. Requests asked for 1,000 rows, but the engine capped pages at 256: **40 pages** were observed. The mixed fixture returned exactly 2,000 tasks in eight pages.

This is engine traversal, not a measurement of 40 HTTP/encrypted round trips or browser decoding/rendering.

## Explicit 20-second runtime-budget probe

Same binary, 10,000 task notes + 10,000 JSON files, `--deadline-ms 20000 --phase-timeout 90`:

- Installation completed in **19.42 seconds**.
- Upgrade returned at **20.00 seconds** with `outcome_unknown`: **mutation settlement was pending**.
- The external watchdog did not fire; this was an engine outcome, not an orchestrator kill.
- Do not interpret this as a clean rollback or proof that no mutation occurred. The disposable fixture was cleaned up; settlement recovery was not examined in this probe.
- This does not emulate TaskNotes' 20-second network deadline exactly, or establish failure under Connect's different internal operation budget.

## Interpretation

The measured setup/runtime work alone reaches the scale of interactive request budgets. The already-current assessment also remains collection-sized. Reducing the task fraction from 100% to 20% dramatically reduces query output but does not similarly reduce runtime setup cost.

JSON noise raised process memory substantially in this sample. Binary enumeration was not consistently slower at 10k than the Markdown-only case on this busy host; repeat measurements before claiming a precise attachment-count penalty.

The leading next steps are repeated quiet-host measurements, comparison with the deployed engine revision, phase-level profiling of setup staging/settlement, and an isolated LAB browser/Connect layer. No performance fix is implied by this benchmark-only change.

## Evidence locations

The initiative workspace retains complete raw artifacts under:

- `results/required-contract-smoke/`: all five 20-note smoke cases.
- `results/required-contract-baseline/`: the ten-case matrix summarized here.
- `results/deadline-20s-json/`: the explicit budget failure.

Each directory contains metadata, exact manifest, JSONL progress, stderr, configuration, and machine-readable/Markdown reports. Baseline and deadline directories also retain the exact executable and benchmark source. Earlier `results/baseline/` observations installed all bundled packs and are exploratory; do not substitute them for the required-contract matrix above.

All synthetic collection/staging directories were removed; logs and reproducibility inputs remain. Harness verification: three Rust tests and nine Python safety tests passed.
