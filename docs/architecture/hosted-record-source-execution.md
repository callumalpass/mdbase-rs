# Hosted record-source execution contract

Status: accepted production semantic contract. Candidate B query, projection,
structural relationship, residual, and mutation seams are under implementation.

Canonical decision: `mdbase-connect/docs/decisions/0010-bounded-hosted-record-source-execution.md`

Governing storage-model decision:
`mdbase-connect/docs/decisions/0011-server-trusted-queryable-hosted-execution.md`

Historical benchmark prototype seam:
`docs/architecture/hosted-storage-benchmark-seam.md`

Connect baseline: `6ea62cf2593e91a0e0b17e9e931ebf0ec23dc805`  
mdbase-rs baseline: `818866705dcc4b6dcfd3bbc1ba63f83fdaec406f`

## Boundary

mdbase-rs owns canonical resource compilation, exact Markdown parsing, type
matching, defaults, coercion, computed fields, CEL, projections, contracts, views,
validation, diagnostics, bounded query accumulation, portable execution outcomes,
and semantic mutation plans.

The selected hosted authority stores encrypted exact Markdown as the sole canonical
record and a provider-readable, rebuildable semantic projection. Body prose and
exact Markdown are absent from that projection.

The integrating authority owns record storage, asynchronous IO, snapshots,
encryption, authorization, admission, durable cursor storage, quotas, retries, and
atomic persistence. mdbase-rs must not import Connect, SQL, Tokio, account, grant,
OAuth, KMS, or provider-journal concepts.

The API is incremental. It does not require a complete `CollectionSnapshot`,
filesystem root, or `Collection` containing every record.

## Required semantic seams

### Catalog

A `CatalogInput` consists of exact structural resource documents, their canonical
kinds, spec version, and an authority-supplied opaque resource revision. Compiling
it produces an immutable `CompiledCatalog` that owns type, contract, schema, saved
view, and configuration semantics. Resource paths remain canonical collection
paths; no authority credential or database identity enters the value.

The filesystem provider builds the input from files. A hosted provider builds it
from one consistent authority snapshot, regardless of whether the benchmarked
physical representation is encrypted, hybrid, or provider-readable. Both produce
the same catalog digest and diagnostics for the same resources.

### Canonical record input

One input contains:

- optional authority-stable record ID;
- canonical relative path;
- opaque exact-document revision;
- exact Markdown document;
- optional record facts needed by the requested operation; and
- an optional type hint bound to a type-catalog revision.

The catalog parses and classifies the record. An absent or stale hint cannot exclude
it. Exact Markdown rendering and revisions remain round-trippable.

### Point execution

Point read, projection, contract, validation, and authorization-relevant type
classification accept one record plus a compiled catalog. They do not enumerate or
load unrelated records unless the requested semantic feature explicitly requires a
bounded link/reference neighborhood.

### Structural projection

Parsing one canonical record produces a versioned semantic projection and a
deterministic structural/link digest. The projection contains canonical path and
file facts, matched types, persisted/effective frontmatter, diagnostics, and
structurally significant body facts without body prose or exact Markdown.

One shared structural parser supplies projection, validation, backlink, rename,
delete, and reference semantics. It preserves wikilink, Markdown-link and embed
kind, normalized target, raw target form where required, alias, anchor, relative
form, source, body tags, and resolution outcome. Resolution distinguishes resolved,
missing, ambiguous, and unsafe traversal outcomes; it never collapses multiple
basename, ID, or title matches into one arbitrary target.

The projection exposes canonical outgoing occurrences. The authority persists
those rows and derives backlinks from their inverse. Authority record/catalog/
generation currentness remains outside mdbase-rs, but the semantic engine and
projection format versions and content digest are explicit outputs.

### Query compilation and accumulation

`compile_query` returns a versioned closed plan plus public `QueryRequirements`
describing:

- body and body-derived file facts;
- link/backlink graph needs;
- computed/effective frontmatter;
- projection and contract mapping;
- ordering keys and required top-K size;
- grouping and summary state;
- full-result count; and
- diagnostics.

A `BoundedQueryExecution` consumes one canonical record at a time and accounts all
retained state against caller-supplied budgets. The supported bounded algorithms are
streaming filters/projections, fixed-state counts and built-in summaries, bounded
top-K, bounded groups, bounded diagnostics, and bounded final serialization.

Exhaustion returns one stable `ExecutionFailure::BudgetExceeded { budget_kind }`.
It never truncates or requests collection materialization. Cancellation and
deadlines are checked at bounded intervals.

Custom summaries and recursive graph traversal are unsupported until they have an
explicit bounded implementation. Candidate plans may add false positives but never
remove a possible match. Missing, stale, malformed, unavailable, or unsupported
projection facts evaluate as unknown and remain candidates. Canonical residual
evaluation reuses filesystem semantics.

Configured Obsidian Base resources compile through a separate versioned hosted Base
plan. It reuses the canonical Bases parser/evaluator for shared and named-view
filters, formulas, TaskNotes renderer fields, ordering, grouping, timezones,
`this.file`, links, embeds, and backlinks. The authority supplies one projection
plus a declared-complete bounded incoming/outgoing neighborhood; absence of the
completion proof fails closed. mdbase-rs reconstructs resolved links and inverse
backlinks from those projections and never accepts exact record Markdown for this
path. Candidate and edge enumeration, snapshots, SQL, and cursors remain provider
responsibilities.

Base expression evaluation charges every AST node, including formula and list
callback recursion, to a caller-supplied work ceiling. Exhaustion is
`hosted_base_operator_budget_exceeded`, not a false filter result. Recursive parser
and evaluator entry points use a bounded growable stack so valid TaskNotes method
chains cannot exhaust a small async worker stack. Hosted evaluators also accept a
provider-neutral cooperative cancellation/deadline token checked at every AST node.

For stale or absent provider projections, `finalize_projection_batch` reconstructs
a caller-bounded exact snapshot without consulting stale provider indexes. It
builds canonical identity keys, applies the same closed relationship-resolution
plans, enforces the global relationship-candidate ceiling, and returns complete
semantic projections suitable for the ordinary Base evaluator. The host remains
responsible for exact-document, plaintext-byte, time, and memory limits.

### Mutation planning

A `MutationPlan` contains portable expected generation/resource revision, record
preconditions, destination uniqueness requirements, bounded reference-neighborhood
evidence, canonical exact writes/deletes, and the final portable semantic result.
It contains no host request identity, grant, ciphertext, SQL, receipt, or journal
state. Rename/delete/reference plans use the same structural occurrences and
resolution semantics as projection generation. They preserve aliases and anchors,
report ambiguity, and include expected neighbor revisions rather than discovering
or mutating authority storage themselves.

Authorities prepare against a read snapshot and atomically revalidate the plan at
their own commit boundary. A changed precondition causes bounded reprepare or a
typed conflict; mdbase-rs does not own provider transaction retries.

## Conformance

Every seam is delivered with a filesystem consumer and a provider-neutral fixture
before Connect uses it. Fixtures compare:

- catalog digest and diagnostics;
- exact record parsing and type classification;
- read/projection/contract envelopes;
- query results, ordering, grouping, summaries, counts, and diagnostics;
- every budget failure kind and atomicity;
- mutation plans and no-op/invalid outcomes; and
- stale type-hint behavior.

SQLite may safely narrow filesystem candidates only when authoritative fallback
preserves completeness. Hosted PostgreSQL hints follow the same rule.

## Deliberate exclusions

- no hosted filesystem emulation;
- no PostgreSQL or CEL-to-SQL adapter in mdbase-rs;
- no Connect protocol or durable cursor rows;
- no application-level authorization types;
- no authority-specific SQL, physical-index, encryption, currentness, generation,
  lease, checkpoint, or persistence policy in mdbase-rs; and
- no general spill/sort engine in the first bounded operator set.
