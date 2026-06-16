# ADR-0010: Lattice extraction via tree-sitter tags queries (multi-language)

**Date:** 2026-06-16
**Status:** Accepted

## Context

Lattice PR1 extracted symbols with a hand-rolled, Rust-specific tree-sitter walker
(`walk`/`classify` over `function_item`, `struct_item`, … node kinds). That does not scale: every
new language would need its own bespoke node-kind classifier, and it captured **definitions only**
— no references, so no `impact` / `path` / call-graph.

PR2 must (a) support most languages a user is likely to have, not just Rust, and (b) produce
reference/call edges so the index can answer "what breaks if I change X" and feed auto-retrieval.

## Decision

Extract via **tree-sitter *tags queries*** (`tags.scm`) — the same mechanism GitHub code-nav uses.
Each grammar ships a `TAGS_QUERY` that captures `@definition.*` and `@reference.*` with a category
(`function`, `class`, `method`, `call`, `module`, …). `tree-sitter-tags` runs the query and yields
`Tag { is_definition, syntax_type_id, name_range, range, span }`. One uniform code path handles
every language; **adding a language is one row** in the registry (grammar `Language` + its bundled
`TAGS_QUERY` + extensions) — no per-language Rust.

- **Definitions** → graph nodes. **References** → a name-keyed `lattice_ref` row (see ADR note
  below). `contains` edges are derived from **span nesting** (a def whose range sits inside
  another's), which is language-agnostic and needs nothing from the grammar.
- The category string is taken **verbatim from the grammar** (`syntax_type_name`) rather than
  re-classified, so we never fight a grammar's taxonomy (e.g. Rust's `tags.scm` labels a struct
  `class`; methods are `function`; `impl` blocks aren't captured, so Rust methods are siblings of
  their type, while Python methods nest under their class — both are correct, just different).

### Version pin: tree-sitter core **0.24.x**

The tree-sitter grammar ecosystem pins each grammar to a `^0.x` core, and these are not all in
lockstep. Verified (mid-2026): `typescript`, `java`, `cpp`, and `ruby` have **no** release on core
0.25+, while `tree-sitter-tags` needs the core to match. The only version set with a complete tags
query across all ten target languages is the **0.24.x line** (+ `tree-sitter-tags 0.24.7`). PR1's
`tree-sitter 0.26` / `tree-sitter-rust 0.24` is therefore **downgraded** in `forge-index` (the only
crate using tree-sitter — change is localized). Initial languages: Rust, Python, JavaScript,
TypeScript (+TSX), Go, Java, C, C++, Ruby. C# is deferred (its 0.24-compatible release ships no
`TAGS_QUERY`; adding it later means vendoring a `tags.scm`).

### References stored by name, not resolved dst id

`lattice_ref(src_id, name, kind, line)` keeps the *callee name*, not a resolved node id, and
resolution is a name-join at query time. Rationale: a resolved cross-file `dst_id` is fragile under
incremental reindexing — re-indexing a file `DELETE`s its nodes and (via `ON DELETE CASCADE`) any
edges pointing **into** it, so stored cross-file edges would silently rot. A name-keyed ref is tied
to its *own* file's `src` node, cascades cleanly with it, and always resolves against the current
node set. `lattice_edge` is kept for intra-file `contains` (cascade-safe). The table is additive —
no constraint migration on the PR1 schema.

## Alternatives considered

- **Keep the hand-rolled walker, add per-language classifiers.** Rejected: O(languages) bespoke
  code, and it still wouldn't capture references without per-language reference rules.
- **Stay on tree-sitter 0.26, drop the four laggard languages.** Rejected: TS/Java/C++/Ruby are
  among the most-requested languages; dropping them defeats "most languages."
- **Resolve reference edges to `dst_id` at index time + a resolution pass.** Rejected: the
  CASCADE-on-reindex rot above; a query-time name-join is simpler and always consistent.
- **LSP-based extraction.** Rejected: needs a language server installed per language — violates
  Lattice's zero-setup, in-process, pure-Rust ethos.

## Consequences

**Positive:** one extraction path for 10 languages; references + `impact`/`path` fall out of the
same query; adding a language is trivial; no startup cost (configs are built lazily per thread).
**Negative:** `forge-index` is pinned to tree-sitter 0.24 until the laggard grammars move forward;
`TagsConfiguration` is not `Sync`, so the compiled-query registry is `thread_local!` (rebuilt once
per thread) rather than a global. **Neutral:** category strings are grammar-defined, so the same
concept can read differently across languages (documented; queries/tools treat `kind` as opaque).
