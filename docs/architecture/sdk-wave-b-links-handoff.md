# B6 handoff — wb-links

Branch: `sdk-upgrade/links`, rebased onto `origin/main` at `af73732`
(authority PR #102). Rebased implementation commits: `2ac251b`, `33f7e48`,
`d60f17e`, `812d4e4`; original benchmark provenance labels `aef9783`, `2a01a29`,
`5dfd920`, `1c58cd1` describe the pre-rebase runs. The coordinator authorised
only this branch's force-with-lease push; PR creation, merge, tags and release
remain coordinator-owned.

## Delivered

- [`docs/link-resolution.md`](../link-resolution.md): native resolution contract,
  profile/key precedence, path/extension/case/folder-note rules, policy options,
  compatibility discrepancies, hosted support boundary, consumer migration,
  and architecture-budget ownership. README/CHANGELOG/evidence docs link it.
- [`conformance/link-resolution-v1.json`](../../conformance/link-resolution-v1.json):
  38 cases, executed in three modes (default, unique, source-type) both with and
  without cache. Covers exact/root/relative/`../`, Markdown/wiki/bare/embeds,
  duplicates and same-directory/depth/lexical ranking, aliases versus title/ID,
  `.md`/`.mdx`/arbitrary extensions, folder notes, Unicode/case, anchors and URLs.
- `tests/link_resolution_v1.rs`: eight real-engine integration tests, including
  the exact Connect scalar `linksTo()` predicate and its `.exists(link, ...)`
  list form. Relative/bare/aliased inputs and missing/null/unresolved fields are
  exercised with and without cache. No mock authority supplies query semantics.
- Writer's installed **JavaScript** capability probe is recorded with producer
  labels (unsupported CEL `asFile`, additive projection, raw test-authority
  snapshots, five resolver targets, and source-inventory PathIndex ambiguity).
  Rust independently executes the five resolver target examples.
- Native CEL overloads:
  `value.asFile({"ambiguity":"native"|"unique","types":["source"]})` and
  `value.asFile(sourcePath, options)`. Nonempty registered type lists intersect
  stored-target constraints. Unknown/invalid options are diagnostics. Explicit
  source overrides retain originating-record constraints; untyped copies cannot
  erase them. Unique policy cannot accept a ranked ambiguous winner or suppress
  the shared candidate budget.
- Static CEL facts and canonical plans prepare a complete policy index only when
  an explicit/dynamic option argument can require it. Cached no-argument and
  literal source-path defaults retain their stored-winner/lazy-index fast paths.
- Hosted option calls fail explicitly rather than infer uniqueness/types from
  winner-only neighbors. Conflicting typed fields sharing a target also fail
  explicitly instead of applying the wrong declaration's scope.

Primary engine files: `src/cel/{host,provenance,program}.rs`,
`src/links/{linked_files,resolver,policy_tests,mod}.rs`,
`src/query/canonical/{preflight,execute}.rs`, `src/runtime/hosted_links.rs`.
`src/expressions/evaluator.rs` only exposes existing shared parser helpers within
this crate. No wire definitions or client API files changed. Canonical query
preflight/execution merged with wb-authority's B2/B3 changes during rebase;
changelog and budget conflicts were resolved by preserving both features.

## Post-rebase verification (CARGO_BUILD_JOBS=4)

On `af73732`, format, architecture, workspace/all-target/all-feature Clippy and
all four CI strict Clippy variants passed with `-D warnings`. The architecture
check measures **201 files / 109,603 lines**, combining authority's +1/+601 and
B6's +1/+516 deltas and retaining both per-file justifications in
[`docs/link-resolution.md`](../link-resolution.md).

`cargo test --locked -p mdbase --all-features --lib` plus the nine selected
integration suites passed **549 tests, 0 failed, 3 ignored**: 492 engine unit
tests and 57 integration tests. This includes all link-policy/traversal/security/
provenance/person-link suites, and authority's `query_document_revisions` and
`query_metadata` producer tests. The eight B6 integration tests pass unchanged.
Log: `/tmp/wb-links-rebase-verification.log`. The full workspace/testbed evidence
below is pre-rebase; it was not relabeled as a post-rebase full CI run.

## Pre-rebase verification (CARGO_BUILD_JOBS=4)

All requested final checks passed:

- `cargo fmt --check`.
- `cargo run --locked -p mdbase-architecture-check -- .`: 200 Rust source files,
  109,002 lines; exact reviewed budgets, no new ambient-I/O/debt allowances.
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`.
- CI's strict lint variants: mdbase, mdbase-command and mdbase-runtime with
  `--no-default-features --all-targets`; mdbase-testbed-adapter with
  `--all-targets`; all with `--locked` and `-D warnings`.
- `cargo test --locked --workspace --all-features`: **774 passed, 0 failed,
  3 ignored**, including all eight new integration tests, existing traversal/
  security/person-link suites, hosted rejection tests, and candidate-budget test.
- Pinned spec `b3883f94c43c6e37904f793d45747ba0c874c871`, portable testbed:
  **3/3 passed** (`core.shared-contract-consumers`, `runtime.competing-workers`,
  `runtime.crash-recovery`).

Final command log: `/tmp/wb-links-final-verification.log`. The final narrow
integration run separately passed **52 tests** (`/tmp/wb-links-final-narrow.log`).

The earlier single `scripts/ci-local` run was **not green**: it recorded the
initial architecture-budget mismatch, an in-flight field-conflict fixture
failure before the current guard was rebuilt, and a Cargo-contention testbed
adapter timeout. Each failed step was subsequently rerun successfully above;
the full script was not looped. Its other quality steps, package verification
(mdbase/mdbase-command), runtime package listing and cargo-deny policy passed.
Live PostgreSQL contracts and Windows-only compilation were not run. The spec
runner's `npm ci` reported one high-severity dependency vulnerability; no npm
package or dependency was changed by this task. Original log:
`/tmp/wb-links-ci-local.log`.

## Indexed benchmark

[`benchmarks/link-resolution-v1.json`](../../benchmarks/link-resolution-v1.json)
retains three samples per mode. Debug, synthetic, 10,000 indexed lookups;
baseline `f60adfe`, final implementation `1c58cd1`. Median milliseconds:

| Records | Default before | Default after | Explicit native | Unique + source |
| --- | ---: | ---: | ---: | ---: |
| 500 | 18.281 | 17.778 | 187.395 | 194.188 |
| 50,000 | 17.167 | 16.797 | 184.359 | 190.596 |

No speedup claim: shared-machine noise and different feature builds are noted.
Policy re-resolution pays bounded selector/evidence costs instead of using a
stored winner. Cost is nearly flat across collection sizes for this distinct-key
fixture; matching-bucket cardinality still matters. Capture, real classification,
CEL, cache-query setup and network costs are excluded. The SDK bench explicitly
uses synthetic query transport and cannot measure these engine semantics, so
this scenario lives in the assigned Rust worktree. Re-run with
`cargo test --locked -p mdbase --all-features --lib indexed_policy_lookup_50k -- --ignored --nocapture`.

## Decisions / deferred work — do not advertise B6 yet

1. **Defaults remain unchanged.** The agreed design overstates current default
   normalization/strict-error behavior. Root wiki dot segments/backslashes and
   malformed/root-crossing inputs are characterized, not silently fixed. Default
   stored-target association also lacks field identity. Coordinator/spec approval
   and a consumer migration are required before changing default semantics.
2. **Hosted policy universe is incomplete.** Neighbors omit losing candidates and
   their type facts. Complete same-snapshot candidate evidence/targets and hosted
   corpus execution are required. Current rejection is
   `link_resolution_options_context_required`, not a capability probe.
3. **Field-specific declaration provenance remains required** for conflicting
   typed fields sharing a value. Current policy rejects this case with
   `link_resolution_field_context_required`; it cannot claim full B6 declaration
   parity. Ordinary single-declaration source/list cases are tested.
4. **Withhold `link-resolution-options-v1`.** Existing SDK query wire carries CEL,
   so no wire operation/grant change was implemented. Reader/TaskNotes keep their
   default recipes. Writer keeps the explicitly source-scoped remote fallback
   and its genuine unsaved-draft PathIndex boundary. Never send/drop options and
   infer equivalent uniqueness on an old or unsupported authority. UI/offline
   title/alias suggestions remain separate product policies. Scope is not auth.

This is ready for coordinator review as native B6 groundwork and consumer
conformance evidence, **not** a claim that hosted/full B6 capability is releasable.
