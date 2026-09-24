# Indexed link traversal — 2026-09-24

## Scope and revisions

Baseline is **main `4227a60`**; the change is on `perf/indexed-link-traversal`. Both sides were measured with the same benchmark source, [`src/bin/link-traversal-benchmark.rs`](../src/bin/link-traversal-benchmark.rs), built with `--release` (no debug assertions), on one otherwise idle 4-core Linux machine, before and after runs back to back.

The fixture is synthetic and disposable: 1,000 `source` records and N `annotation` records whose declared `source` link field holds an ID wikilink (`'[[src_00042]]'`, `target_type: source`), roughly the shape of a reading library. Every case runs through `FilesystemRuntime::execute`, as a Connect daemon runs it, after one warm-up run; times are the median of three. No daemon, real collection or hosted environment was involved.

```bash
cargo run --release --bin link-traversal-benchmark -- \
  --annotations 2000 6000 12000 24000 --sources 1000 --output results.json
```

## What changed

`asFile()` had its own resolver: for every call it scanned every record for an exact path, then every record for a basename, then every record for a configured ID. A query that follows one link per candidate was therefore quadratic in collection size. It also disagreed with the rest of the engine: it tried filenames before IDs and ignored a link field's declared target types, while backlinks, validation and the cache index resolve through `LinkResolutionIndex` (ID first, target types honoured, duplicate IDs ambiguous).

1. **Stored links resolve once.** The query snapshot's link graph now also records the target each stored link resolved to, keyed by source path and stored link text. It comes from the same pass that builds backlinks, or, on the cached path, from the cache's `links` table, which the indexer already maintains incrementally with resolved targets. `asFile()` on a record's own link is a hash lookup and returns exactly the target backlinks use.
2. **Other link values share the resolver.** A link built in an expression, or read from `this`, resolves through `LinkResolutionIndex` without target types, built on first use.
3. **Targets by path.** Traversal finds the target record through a path map instead of a scan.

`EvalContext::all_files` is now `Option<Arc<LinkedFiles>>`; `Collection::build_link_graph` builds it (with backlinks) for callers constructing an `EvalContext` themselves. `LinkResolutionIndex::resolve` holds the resolution previously in `Collection::resolve_link_target`, which delegates to it.

**Behaviour change:** where a filename match and an ID match disagree, or a link field's target type rules out a filename match, `asFile()` now returns the record link resolution, backlinks and validation already chose. `tests/link_traversal.rs` covers both cases on built and cached link graphs; two of its three tests fail on the baseline.

## Measurements

Median milliseconds; 1,000 sources in every row.

| Annotations | Case | Baseline | Indexed | Speed-up |
|---:|---|---:|---:|---:|
| 2,000 | `where` through `source.asFile()` | 837.5 | 30.4 | 27× |
| 2,000 | sort by `source.asFile().title` | 480.5 | 39.2 | 12× |
| 6,000 | `where` through `source.asFile()` | 5,869.3 | 80.5 | 73× |
| 6,000 | sort by `source.asFile().title` | 2,976.4 | 107.3 | 28× |
| 12,000 | `where` through `source.asFile()` | 21,440.6 | 153.7 | 139× |
| 12,000 | sort by `source.asFile().title` | 17,416.2 | 219.5 | 79× |
| 24,000 | `where` through `source.asFile()` | 120,215.8 | 317.6 | 379× |
| 24,000 | sort by `source.asFile().title` | 49,351.9 | 474.4 | 104× |

Unchanged cases, for context:

| Annotations | Plain page (baseline → indexed) | `file.backlinks` filter | Delete dry run with `check_backlinks` |
|---:|---:|---:|---:|
| 2,000 | 25.5 → 22.4 | 21.1 → 19.6 | 119.9 → 111.4 |
| 6,000 | 75.8 → 69.9 | 44.7 → 43.3 | 282.3 → 271.3 |
| 12,000 | 147.9 → 146.1 | 74.7 → 73.8 | 575.7 → 516.6 |
| 24,000 | 339.5 → 295.7 | 132.0 → 151.5 | 990.0 → 1,014.3 |

Every case returned the same `total_count` (or broken-link count) on both sides. Link-following queries now cost about as much as a plain page plus link-graph loading, and grow linearly. The `file.backlinks` filter at 24,000 reads the extra `links` columns for stored targets (about 20 ms); smaller sizes are within noise.

## Not changed

A delete with `check_backlinks` captures the collection from the filesystem and rebuilds backlinks, deliberately: mutations treat Markdown, not the cache, as authoritative. It grows linearly (about 1 s at 25,000 records here) and is left as is.
