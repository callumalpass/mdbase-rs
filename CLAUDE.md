# mdbase-rs is deprecated

This is the legacy mdbase engine. It is **not authoritative** for mdbase
semantics: link resolution, validation, querying, paths or anything else.

- Do not cite this repository's code, `docs/`, or `conformance/` corpora as
  how mdbase behaves, or as a specification. They describe an engine that no
  longer ships to new collections.
- The authorities are the mdbase specification and the runtime in
  mdbase-connect `runtime/` (https://github.com/mdbase-dev/mdbase-connect/tree/main/runtime): `runtime/crates/core` for
  behaviour, `runtime/docs/spec-notes.md` for spec interpretations, and
  `runtime/docs/contracts/` for the client and wire contracts.
- Change this repository only to keep the legacy Connect stack (mdbase-connect
  root `crates/`) building or secure until it is retired. Put new collection
  semantics in the runtime, never here.
