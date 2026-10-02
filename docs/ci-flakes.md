# CI flakes

## Bounded, visible CI retries

Rust CI uses `node scripts/ci/cargo-test.mjs` with its original Cargo selection
and feature options. The script builds once and uses Cargo's stable JSON
protocol to discover each harness and its package working directory. Local runs
fail on the first failure. In CI, a complete libtest assertion failure may retry
**only that exact named test once**, in the same binary. A passing retry must
report exactly one passing test.

Recovered failures keep the job green but emit a warning annotation, a step
summary, a JSONL record and original/retry logs in `flake-evidence-*` artifacts
(14 days). Artifact names include the GitHub run attempt, so manual reruns retain
prior evidence without name collisions. A second failure keeps the job red. Compilation failures, crashes,
unknown harness output and doctests do not retry. Doctests retain their original
Cargo gate. No `continue-on-error`, blanket job retries, sleeps or widened test
timeouts are permitted as flake fixes. CI currently uses Cargo, not nextest; if
nextest replaces it, use its per-test retry/report mechanism rather than stacking
this wrapper on top of it.

## Nightly stress

`.github/workflows/flake-stress.yml` runs nightly and via workflow dispatch;
`iterations` defaults to 20, validated as 1–1000. It repeats the core/runtime
library tests whose names contain type_file, concurrent, durability, lease,
crash, recovery, watch or restart. Full exact names are discovered, and selected
tests run together in each harness, preserving internal parallelism. Empty
selection fails explicitly. Ignored tests are excluded, not counted as coverage.
Stress never retries; any failure makes the lane red. Each lane has a 90-minute
wall-clock cap; choose large dispatch counts accordingly.

Linux/Windows inherit affinity to two CPUs. Standard public GitHub macOS runners
have no affinity API/two-core configuration, so Homebrew `cpulimit` gives the
process tree a 200% aggregate CPU-time budget, not physical-core pinning.
Rust build/test worker budgets are two. Exact two-vCPU macOS coverage would need
a separately provisioned runner and replacement of the `macos-15` label.

A separate trusted reporting job consumes artifacts as data, and creates,
reopens or updates one bot-owned, marker-identified **CI flake stress tracking**
issue with exact names, platform, iteration and run/artifact link. Only that job
has `issues: write`; PR CI has no write token. Prior reports remain in issue edit
history. Setup failures remain red with their workflow logs. No new secrets or
repository/merge-queue settings changes are required. Testbed adapters are
prebuilt before the unchanged protocol-response deadline, so cold compilation
cannot consume a semantic operation's time budget.

```sh
node scripts/ci/two-cpus.mjs scripts/ci/cargo-test.mjs \
  --stress 100 --match 'operations::type_file::tests' --locked -p mdbase --lib
node scripts/ci/track-flakes.mjs .ci-flakes --dry-run
```

## Type-file race

Candidate validation previously copied every collection file, including
unrelated `.mdbase-publish-*` names. Atomic create publishes with a hard link,
then removes its temporary name: other validators could observe disappearing
names or files with two links, which authority reads correctly reject. These
I/O failures violated the concurrency test's expected `path_conflict` outcomes.

Validation now stages only the candidate's explicit local schema dependencies,
using the existing definition-staging dependency discovery. It does not weaken
hard-link/symlink checks or serialize concurrent creates. A barrier-controlled
regression holds an unrelated two-link publication window open; it fails with
the old whole-collection snapshot and passes with the dependency-only snapshot.
A second regression verifies that explicit relative schema references still work.
