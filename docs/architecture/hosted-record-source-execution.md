# Hosted record-source execution contract

Status: accepted companion contract for the cross-repository hosted execution ADR.

Canonical decision: `mdbase-connect/docs/decisions/0010-bounded-hosted-record-source-execution.md`

Connect baseline: `6ea62cf2593e91a0e0b17e9e931ebf0ec23dc805`  
mdbase-rs baseline: `818866705dcc4b6dcfd3bbc1ba63f83fdaec406f`

## Boundary

mdbase-rs owns canonical resource compilation, exact Markdown parsing, type
matching, defaults, coercion, computed fields, CEL, projections, contracts, views,
validation, diagnostics, bounded query accumulation, portable execution outcomes,
and semantic mutation plans.

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

The filesystem provider builds the input from files. The hosted provider builds it
from one consistent encrypted PostgreSQL snapshot. Both produce the same catalog
digest and diagnostics for the same resources.

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

### Query compilation and accumulation

`compile_query` returns a private plan plus public `QueryRequirements` describing:

- body and body-derived file facts;
- link/backlink graph needs;
- computed/effective frontmatter;
- projection and contract mapping;
- ordering keys and required top-K size;
- grouping and summary state;
- full-result count; and
- diagnostics.

A `BoundedQueryExecution` consumes one canonical record at a time and accounts all
retained state against caller-supplied budgets. The initial supported algorithms are
streaming filters/projections, fixed-state counts and built-in summaries, bounded
top-K, bounded groups, bounded diagnostics, and bounded final serialization.

Exhaustion returns one stable `ExecutionFailure::BudgetExceeded { budget_kind }`.
It never truncates or requests collection materialization. Cancellation and
deadlines are checked at bounded intervals.

Custom summaries and link graphs are unsupported until they have an explicit
bounded implementation. Candidate hints may add false positives but never remove a
possible match.

### Mutation planning

A `MutationPlan` contains portable expected generation/resource revision, record
preconditions, destination uniqueness requirements, bounded reference-neighborhood
evidence, canonical exact writes/deletes, and the final portable semantic result.
It contains no host request identity, grant, ciphertext, SQL, receipt, or journal
state.

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
- no plaintext hosted indexes; and
- no general spill/sort engine in the first bounded operator set.
