# Benchmark-only hosted storage semantic seam

Status: frozen prototype contract for proposed Connect ADR 0011. It is not an
accepted public API and must not be merged as a permanent storage decision before
user review.

## Inputs and ownership

mdbase-rs receives a `CompiledCatalog` and one `CanonicalRecordInput` at a time. It
owns exact Markdown parsing, persisted/effective frontmatter, type classification,
relationships, diagnostics, candidate compilation, and canonical residual
evaluation. It has no SQL, encryption, PostgreSQL, grant, journal, KMS, or physical
index types.

Connect owns storage, snapshots, batch IO, encryption, key caching, projection
generation leases/checkpoints, authorization, deadlines, cancellation, and
persistence. The disposable benchmark adapter serializes only mdbase-rs values.

## Projection value

The benchmark projection format is
`mdbase-connect/docs/benchmarks/hosted-storage-model/projection.schema.json` and is
versioned as `hosted-benchmark-projection-v1`. It contains canonical path/types/file
facts, persisted and effective frontmatter, outgoing relationships, bounded
diagnostics, and no body-derived search value. It never contains body text,
document revision, a separate body byte count, tokens, n-grams, or a full-text
vector. Its file-size fact necessarily leaks total exact-document length.

`project_record` returns the projection and a deterministic canonical JSON digest.
Malformed/non-object frontmatter follows exact snapshot behavior: exact Markdown is
preserved, persisted/effective maps are empty, and a bounded diagnostic is emitted.
An invalid record path is rejected.

The authority decomposes the envelope into readable path/type/file columns plus the
semantic JSON payload, and stores record revision, catalog revision,
projection-format version, generation, and the canonical projection digest beside
it. Those currentness bindings are not fields owned by mdbase-rs.

## Closed candidate IR

The benchmark IR is
`mdbase-connect/docs/benchmarks/hosted-storage-model/candidate-ir.schema.json`.
Supported operations are only:

- conjunction, disjunction, and negation;
- type membership;
- equality and membership against a fixed allow-list of projected field paths;
- case-insensitive text containment against allowed projected strings;
- scalar less-than for the frozen date/range workload;
- relationship target equality; and
- body substring requirements.

Compilation returns:

- `requirements`: whether exact Markdown/body, effective/persisted frontmatter,
  relationships, diagnostics, ordering, grouping, count, or response serialization
  is required;
- `candidate`: the greatest safe provider-translatable subset, or `All`;
- `residual`: the complete canonical expression; and
- limits for retained results, ordering, grouping, diagnostics, bytes, and checks.

Candidate evaluation has three values: `possible`, `impossible`, and `unknown`.
Only `impossible` may exclude a current projection. Missing fields, unsupported
operators, malformed records, stale/absent projections, and body predicates on a
body-free projection are `unknown`, never false.

The SQL adapter translates the versioned candidate AST through fixed parameterized
templates. It does not parse CEL or invent field semantics. Property tests require
that every canonical match is either a candidate match or comes from the mandatory
stale/missing union.

## Residual and bounded result evaluation

The residual evaluator consumes projected facts and, when requirements demand it,
the canonical exact record. It produces the frozen response shape and accounts:

- candidate rows, canonical rows, exact documents and bytes;
- retained result items/bytes;
- top-K entries and offset;
- group count and aggregation bytes;
- diagnostics count/bytes; and
- cancellation/deadline checks.

Budget exhaustion returns the existing stable budget kind and discards the
operation result. It never truncates a successful result. The benchmark records
typed budget outcomes separately from canonical expected results.

## Authorization classification

`classify_for_authorization` accepts one exact record or a projection explicitly
asserted current by the authority adapter. mdbase-rs does not trust that assertion
implicitly: the adapter supplies the pinned record/catalog/format/generation
bindings and tests prove the predicate before calling the projection path.

An absent, stale, corrupt, or semantically ambiguous projection requires exact
classification. Failure returns an error suitable for fail-closed authorization.
An unversioned persisted type hint can accelerate neither allow nor deny.

## Conformance evidence

The prototype must compare filesystem/catalog canonical execution with all five
storage candidates for every frozen fixture/workload and generated property case.
Required properties are:

- exact projection equality and digest stability;
- zero candidate false negatives;
- identical final membership, order, groups, summaries, diagnostics, response
  fields, and typed budgets;
- stale/missing/corrupt projection fallback;
- malformed exact Markdown preservation;
- authorization narrowing/widening fail-closed behavior; and
- cancellation without retained plaintext or partial success.
