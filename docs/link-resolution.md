# Native link resolution V1

This is the mdbase-rs engine contract for CEL record traversal. The executable
corpus is [`conformance/link-resolution-v1.json`](../conformance/link-resolution-v1.json),
run by [`tests/link_resolution_v1.rs`](../tests/link_resolution_v1.rs). Resolution
is collection semantics, not a client-side filename heuristic. A type/scope
filter is query policy, **not authorization**.

## Snapshot and provenance

Resolve eligible collection records from one captured snapshot. `asFile()` does
not open arbitrary filesystem files or produce fake records for binary assets.
`settings.record_extensions` controls which files participate as records.
Binary file references have a separate file-descriptor boundary.

The source directory is that of the record the value came from: the candidate,
`this`, a traversed record, or an explicit source-path argument. List elements
retain that provenance through CEL comprehensions. Query `types` selects source
rows; it does not limit traversal targets. Each evaluation permits at most
10,000 traversal calls; eligible resolution candidates retain the shared
16,384-candidate budget. Budget failures are diagnostics, not missing links.

## Existing forms (compatibility baseline)

```text
source.asFile()
source.asFile("annotations/a.md")
record["sources"].exists(link, link != null && link.asFile() != null)
```

These forms preserve pre-B6 behavior, including deterministic ranking and the
existing extractor's edge cases. They never implicitly traverse the first
member of a list. Missing/ambiguous targets return null. CEL evaluation failures
are emitted as `expression_evaluation_error` diagnostics; query rows do not
silently count a failed predicate as a match. The v0.3 operation envelope can
remain `valid: true` with a diagnostic and null value—inspect diagnostics, not
just the envelope's validity bit.

Stored frontmatter/body links reuse their snapshot graph winners. Frontmatter
occurrences precede body occurrences with the same extracted target, retaining
that field's declared target types. Other expression links use the snapshot key
index collection-wide. The existing shortcut is keyed by source and extracted
target, not field identity; a constructed value equal to a stored link can reuse
that winner. B6 does not change default outgoing/backlink graph semantics.

## Target and key rules

1. Wikilink `|label` and `#anchor` describe display/subtarget syntax, not additional
   target aliases. Markdown labels and destination titles do not name targets.
   Valid embeds use their link target. Missing/external/local-anchor targets do
   not produce a record.
2. A leading `/` is collection-root-relative. Markdown/bare paths are extracted
   with a `./` source-relative marker. `./` and `../` wikilinks are source-relative;
   other wikilinks containing `/` are root-relative. A simple wikilink, or a bare
   scalar name without path/extension syntax, is a key lookup.
3. Explicit paths use exact, case-sensitive canonical spelling. First try the
   exact path, then append `.md` unless it ends in `.md` or `.mdx`. Never strip
   arbitrary extensions: a path to `plot.png` can select eligible `plot.png`,
   otherwise `plot.png.md`. A simple wiki `[[book.md]]` strips `.md` for key
   lookup; `[[plot.png]]` retains the key `plot.png`. `[[chapter]]` can name
   `chapter.mdx` by basename; `[[chapter.mdx]]` does not generically strip `.mdx`.
4. Simple keys use Unicode lowercase. Lookup order is configured ID, basename,
   then v0.2 legacy title. v0.3 IDs participate only with explicit
   `settings.id_field`; v0.3 titles/editor alias suggestions never add keys.
   The first nonempty eligible class wins. Duplicate IDs/titles remain ambiguous,
   with no fallthrough to basenames.
5. One candidate resolves. Multiple basenames rank by same source directory,
   then fewest path segments, then UTF-8 lexical canonical path. This is not
   shortest character count or inventory order. Folder notes have no special
   implicit `Folder/Folder.md` convention: use an explicit path or ordinary key.
6. Declared link target types constrain stored links. An exact existing path
   outside the eligible types is unresolved, not retargeted to its `.md`
   alternative. Type names use mdbase canonical case-insensitive matching.

### Baseline/design discrepancies (do not silently change defaults)

The Wave B agreement requests portable separator/dot normalization and strict
invalid-input diagnostics for all traversal. The current no-option engine does
not fully implement those rules: root-relative wiki `[[sources/./book]]` and
`[[sources\\book]]` do not normalize to `sources/book.md`; malformed/root-crossing
scalar values can become null without diagnostics. Unresolved declared links
can also fall back to an untyped expression lookup when no stored winner exists.
The corpus explicitly records the first two differences rather than pretending
that default behavior was already conformant. **Changing the defaults requires
coordinator/spec approval.** Explicit option forms below enforce the strict
rules without changing the compatibility baseline.

## Explicit options (filesystem-native implementation)

```text
source.asFile({"ambiguity":"unique","types":["source"]})
source.asFile("annotations/a.md", {"ambiguity":"unique","types":["source"]})
```

The map accepts only:

- `ambiguity`: `"native"` (default) or `"unique"`;
- `types`: an optional **nonempty** list of registered, valid type names.

Unknown keys, invalid values/names, and unknown types produce CEL diagnostics.
There is no folder scope, target alias, extension override, or fuzzy matching
option in V1. Scope is expressed through eligible record types.

Requested types intersect declared target types; they cannot widen the snapshot's
stored-target constraints. An explicit lexical source-path override retains the
originating record's constraints, and untyped copies cannot erase them. Explicit policies always re-resolve against the typed snapshot
index, including stored links with an existing graph winner and unresolved
stored links. Constraints use the first declared frontmatter target-key association described
above, independently of the lexical source override. There is no per-link scan of the full collection.
Compiled CEL facts identify explicit/dynamic option calls; cached canonical queries
prepare the typed policy index once only when needed. No-option cached queries
retain their existing stored-winner and lazy untyped-index paths.

Different typed fields sharing one extracted target expose a further baseline
limitation: the native graph associates values by source/target, not field
identity. Explicit policies reject conflicting declarations with
`link_resolution_field_context_required` rather than apply one field's scope to
another. Field-specific declaration provenance is required to support those
collections; no-option graph selection remains unchanged.

`unique` rejects more than one eligible candidate in the winning class **before
ranking**, including a same-directory winner. Filtering happens before counting.
ID/title ambiguity still does not fall through. Exact path precedence is unchanged.
`native` retains ranking, but an explicit map (including `{}`) opts into strict
parsing/validation; it is not a byte-for-byte synonym for the no-option overload.
Strict parsing rejects multiple-target/malformed scalar link intent, invalid
portable paths, drive/UNC paths, NUL, and root-crossing traversal. Separators and
dot segments normalize lexically inside the collection. Empty/local-anchor-only
and external URLs are unresolved. Lists require explicit CEL iteration.

## Hosted support and capability gate

Current hosted relationship neighborhoods contain graph winners and incoming/
outgoing neighbors, not a complete policy candidate universe. In particular,
losing basename candidates and their type metadata may be absent. Options fail
closed with `link_resolution_options_context_required` (wrapped by the ordinary
CEL diagnostic), even if an earlier default lookup populated a neighborhood
index. Default hosted traversal remains supported and unchanged.

**Do not advertise `link-resolution-options-v1` yet.** Hosted provider/plan work
must supply complete eligible candidate evidence and target records from the
same snapshot, and declaration provenance must handle conflicting typed fields,
before this capability can be advertised. The native corpus must
then run against hosted policy evaluation too. No endpoint or wire change is
implemented here. Existing resolution evidence is described in
[`relationship-resolution.md`](relationship-resolution.md).

## Architecture budget review

The checker measures 200 Rust source files / 109,002 lines (previous ceilings
199 / 108,486). The one new source file is the private indexed-policy budget/
benchmark test module; integration fixtures live under `tests`/`conformance`.
The 516-line ceiling increase pays for explicit overload/provenance handling,
strict policy parsing, eligible candidate constraints, and hosted fail-closed
tests—not a second client resolver or additional filesystem discovery path.
The existing resolver ceiling increases from 1,175 to its measured 1,265 lines
for the policy/index metadata it owns. All other concentration, ambient-I/O,
legacy-call, and transitional-reference budgets remain unchanged. These exact
ceilings are review signals, not unused headroom.

## Consumers and producer evidence

The corpus records Writer's installed **JavaScript** producer probe separately:
projections are additive; that producer lacks CEL `asFile`; its resolver chooses
`notes/book.md`/`other/duplicate.md` from the probe inventory. Writer's PathIndex
uses a source-only inventory and rejects duplicate basenames. Raw snapshots in
the record test authority are not evidence of query semantics. Rust executes
those five target examples independently; JS unsupported is not reported as
native Rust failure or success.

Connect's `linksTo()` emits presence/null guards plus authority traversal, with
`.exists(link, ...)` for lists. Integration tests execute that exact shape on
Rust, including relative/bare/aliased links and missing/null/unresolved fields,
with and without cache. The helper is not a remote resolver and does not yet
emit these options.

Consumer migration: retain existing Reader/TaskNotes default recipes. Writer
must retain its explicitly source-scoped remote fallback and unsaved-draft
PathIndex until negotiated hosted/native policy parity exists. Never send options
to an authority without explicit support, or silently remove options and claim
equivalent uniqueness. Editor/Obsidian offline aliases and suggestions remain
product UI policies, not additional authority keys. No mandatory default-query
migration is required.
