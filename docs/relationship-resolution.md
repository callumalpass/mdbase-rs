# Relationship resolution evidence

Local filesystem resolution and hosted authority resolution retain separate candidate enumeration. Both pass eligible candidates to the same bounded selector.

A resolved occurrence carries a typed `reason`:

- `configured_id` for a unique configured-ID match;
- `only_candidate` when no ranking was needed;
- `exact_path` for a unique path lookup;
- `same_directory`, `shallowest_path`, or `lexical_tie_break` for basename ranking.

It also carries the selected lookup key, complete `candidate_count`, a canonical digest over the lookup kind and ordered `(record_id, path)` identities, and every lexically sorted losing `alternative` with its complete candidate identity. Candidate enumeration is already bounded by `MAX_RESOLUTION_CANDIDATES`, so evidence is complete rather than sampled (`MAX_RESOLUTION_ALTERNATIVES` is exactly one less than that bound). Projection validation recomputes count, digest, and ranking from the winner plus all losers. Duplicate eligible configured IDs remain ambiguous and never fall through to filename or title lookup.

Zero valid eligible candidates is the only missing outcome. Unsafe paths, malformed syntax, conflicting identities, and selector failures are diagnostics. Resolution, backlinks, validation, cache, and rename-planning paths propagate them as typed failures; graph data never carries diagnostic sentinels.

`resolved_path` retains its v0.3 wire shape. Selector evidence is additive only on structural projection occurrences. Because that changes persisted projection content, semantic projection format v6/schema v5 accepts only v6 evidence-bearing storage bindings. Older projections remain deserializable so they can be identified and rebuilt, but fail currentness checks and cannot be mixed with v6 digest acceptance.

## Connect compatibility migration

This phase intentionally breaks the Rust API until the paired Connect follow-up lands. That follow-up must make these exact changes:

1. Handle `Collection::build_backlinks_index` as `Result<HashMap<_, _>, CatalogError>` and map `CatalogError.code`/`message` to the operation diagnostic; never inspect a reserved graph key.
2. Decode semantic projection format `6` / schema `mdbase-semantic-projection-v5`, including `ResolvedStructuralOccurrence.reason`, `selected_lookup`, `candidate_count`, `candidate_digest`, the complete `alternatives` set, and aligned `alternative_candidates` identities.
3. Treat missing, unsorted, oversized, or semantically impossible v6 evidence as a stale/invalid projection requiring authoritative rebuild. Do not accept it using a v5 digest or silently downgrade it to missing.
4. Preserve the v0.3 `resolved_path` response exactly; expose evidence only through structural projection APIs that accept additive fields.
5. Regenerate Connect Rust/TypeScript bindings and fixtures for `ResolutionReason`, then run cross-version projection, backlinks, validation, and rename-planning tests before release.
