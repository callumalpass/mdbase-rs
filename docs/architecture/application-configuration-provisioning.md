# Application-declared collection setup

Status: proposed for the next pre-release contract

## Decision

mdbase owns one canonical collection-setup planner and transaction. A setup may
contain managed type packs and narrowly scoped configuration contributions.
Callers assess the complete setup, show that exact assessment for review, and
apply it with the assessment digest and collection revision returned by the
planner.

Connect transports this contract and binds it to a registered application
declaration. A connector, relay, hosted provider, or application must not merge
`mdbase.yaml`, calculate setup conflicts, or apply packs independently.

The first configuration algebra deliberately has one predicate and one
mutation:

- requirement predicate: `contains`
- provision operation: `set_add`

Both operate on a JSON-pointer-selected sequence under a top-level `x-*`
extension namespace. This is sufficient for declarations such as adding
`views/tasknotes/**/*.base` to `/x-obsidian/bases/include` without granting
an application arbitrary configuration writes.

## Public engine model

The canonical input is equivalent to:

```json
{
  "application_id": "dev.mdbase.tasknotes",
  "declaration_digest": "sha256:...",
  "requirements": {
    "configuration": [
      {
        "id": "tasknotes-base-sources",
        "path": "/x-obsidian/bases/include",
        "predicate": "contains",
        "value": "views/tasknotes/**/*.base"
      }
    ]
  },
  "provisions": {
    "configuration": [
      {
        "requirement": "tasknotes-base-sources",
        "operation": "set_add",
        "path": "/x-obsidian/bases/include",
        "value": "views/tasknotes/**/*.base"
      }
    ],
    "type_packs": []
  }
}
```

The Rust API uses typed structures rather than an unvalidated JSON object:

- `CollectionSetup`
- `ConfigurationRequirement`
- `ConfigurationProvision`
- `ConfigurationPredicate::Contains`
- `ConfigurationOperation::SetAdd`
- `CollectionSetupAssessment`
- `CollectionSetupApplyOptions`
- `CollectionSetupReceipt`

The application identifier must be a stable namespaced identifier. The
declaration digest, requirement/provision values, and type-pack manifests use
canonical JCS hashing. Every configuration provision must reference exactly one
requirement and repeat its path and value exactly. Duplicate requirement IDs,
orphan provisions, duplicate provisions, and requirements without provisions
are invalid declarations.

The initial value domain is JSON scalar values: string, finite number, boolean,
or null. Object and array membership are deferred until their equality and
upgrade semantics have a demonstrated application need.

## Path policy

A configuration path:

1. is an RFC 6901 JSON pointer;
2. begins with one non-empty top-level segment matching `x-[a-z0-9][a-z0-9-]*`;
3. contains only non-empty object-key segments after decoding `~0` and `~1`;
4. contains no array indexes, `-` append token, NUL, or control characters;
5. is bounded in segment count and encoded length; and
6. never addresses `spec_version`, `settings`, `runtime`, service limits,
   validation policy, type storage, security, or another core namespace.

Missing intermediate extension mappings are created. An existing intermediate
scalar or sequence is a structured `configuration_path_conflict`. The target
must be missing or a sequence; any other target produces
`configuration_type_conflict`. The planner never replaces a conflicting
value.

## Merge algebra

For a target sequence `S` and declared scalar `v`:

```text
set_add(S, v) =
  S                     when v is already a member of S
  S followed by v       otherwise
```

Membership uses canonical JSON scalar equality. Existing order and every
unrelated YAML value are preserved. New values append in declaration order.
Applying the same setup twice is idempotent.

Multiple applications may contribute the same `(path, value)`. The YAML
contains one value while the durable provision lock records every contributing
application. Disconnecting or revoking an application does not edit
`mdbase.yaml` and does not remove its contribution. Cleanup is a future,
explicit collection-owner operation with its own assessment.

The first contract has no subtract, replace, merge-object, or whole-document
operation.

## Assessment

Assessment runs against a crash-safe shadow of the complete collection. It:

1. captures a deterministic revision over the exact preflight file baseline;
2. validates the application identity, declaration digest, paths, requirement
   links, operations, and values;
3. plans each configuration contribution;
4. plans and stages every type pack in declaration order using the existing
   canonical type-pack planner;
5. validates the resulting configuration and all collection records;
6. produces one assessment containing configuration actions, type-pack
   assessments, conflicts, the starting collection revision, the provision
   digest, and the final resource revisions; and
7. hashes that complete identity into `assessment_digest`.

Assessment statuses are:

- `current`: every requirement is already satisfied and every receipt is
  current;
- `provision`: at least one safe addition or pack change is required;
- `conflict`: at least one path, type, ownership, pack, or validation conflict
  prevents the complete setup.

Conflicts include an exact JSON pointer, stable code, expected shape, observed
shape, and human-readable message. They do not include collection record
contents.

## Apply and transaction boundary

Apply requires:

- the exact application ID;
- exact declaration digest;
- exact provision digest;
- exact reviewed assessment digest;
- exact reviewed collection revision; and
- explicit downgrade approval for every assessed type-pack downgrade.

The engine replans from the live collection. Any mismatch returns
`concurrent_modification` before changing files. A conflict returns its typed
assessment and changes nothing.

The already planned shadow contains the final `mdbase.yaml`, type/contract
resources, `mdbase.lock.yaml`, and `mdbase.provisions.yaml`. One existing
crash-recoverable collection transaction compares the complete live baseline
again and commits all changed files together. Process termination is recovered
by the normal transaction journal. There is no state in which configuration is
committed without its packs or receipts.

After commit, the engine reopens and validates the collection. The returned
receipt is derived from the durable lock state and includes the application ID,
declaration/provision digests, assessment digest, committed collection revision,
configuration contributions, type-pack receipts, and whether transaction
cleanup was deferred.

An ambiguous transport outcome is recovered by the authority's durable mutation
journal using the same request ID. Replaying the setup is safe because both the
collection transaction and contribution merge are idempotent.

## Durable contribution lock

`mdbase.provisions.yaml` is a canonical collection resource. Version 1 records
sorted contribution groups keyed by canonical path and scalar value. Each group
contains sorted contributors with:

- application ID;
- declaration digest;
- provision digest; and
- requirement ID.

The file contains no credential, token, user identity, collection path, or
record content. Type packs continue to use `mdbase.lock.yaml`. Both locks are
included in provider snapshots, resource revisions, setup transactions, and
backup/restore behavior.

Receipts are retained when an application disconnects. A later declaration from
the same application may add a new contributor receipt but cannot silently
rewrite or remove an older contribution.

## Authority and authorization contract

Connect exposes two provider-neutral operations:

- `collection.setup.assess` (read/preflight)
- `collection.setup.apply` (durable mutation)

The signed application declaration contains the exact configuration
requirements and provisions. Connect registration computes the declaration and
provision digests. Authorization grants a setup capability only when the
registered declaration contains provisions, and apply verifies the request
against that registered declaration before dispatch.

The local connector remains the final authorization boundary for relay-backed
filesystem collections. Hosted and local authorities call the same mdbase-rs
API and serialize the same assessment, receipt, and conflict types.

## Failure and privacy contract

Every failure is typed and bounded. The initial stable codes are:

- `invalid_collection_setup`
- `configuration_path_conflict`
- `configuration_type_conflict`
- `collection_setup_conflict`
- `collection_setup_stale`
- `collection_setup_apply_failed`
- existing type-pack, validation, transaction, and recovery codes where those
  remain more precise

Diagnostics may contain application IDs, declaration IDs, requirement IDs,
extension pointers, actions, revisions, and digests. They never contain
configuration values unless those values came from the public application
declaration, and never contain record payloads, credentials, collection
filesystem paths, or keys.

## Rejected alternatives

- Raw JSON Patch or whole-`mdbase.yaml` replacement gives applications more
  authority than their declared feature requires.
- A Connect-owned YAML merge would diverge between hosted and local authorities.
- One endpoint per application namespace would make TaskNotes policy part of
  generic collection infrastructure.
- Applying configuration and packs in separate requests creates a durable
  half-configured state and ambiguous recovery.
- Removing contributions automatically on disconnect can break another
  application or user workflow and is not reversible without a new review.

## Initial conformance matrix

The shared fixture set covers:

- missing namespace, intermediate mapping, and target sequence creation;
- already-present values and repeat apply;
- two applications contributing the same value;
- unrelated key and sequence-order preservation;
- wrong intermediate and terminal types;
- forbidden core namespaces and malformed pointers;
- declaration/provision digest mismatch;
- collection change between assess and apply;
- type-pack conflict plus safe configuration addition producing no partial
  change;
- termination after each transaction step and replay after response loss; and
- identical filesystem, hosted-provider, and relay-backed results.
