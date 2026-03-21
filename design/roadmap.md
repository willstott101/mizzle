# Roadmap

## Implementation phases

### Phase 1 — Extract `mizzle-proto` ✓

Pkt-line, capability parsing, filter/shallow utilities in a standalone
crate with no storage dependency.

### Phase 2 — SSH transport ✓

russh-based SSH server with deferred auth (`SshAuth` trait).

### Phase 3 — Runtime consolidation ✓

Commit to tokio.  Remove trillium/actix/rocket adapters, keep axum as
canonical HTTP integration.  See [runtime-consolidation.md](runtime-consolidation.md).

### Phase 4 — Define storage traits ✓

Audit all gitoxide calls in `fetch.rs`, `pack.rs`, `ls_refs.rs`,
`receive.rs`. Define the thin storage trait and the full-bypass backend
trait. Move the current gitoxide implementation behind the thin storage
trait as `FsGitoxide`. Existing tests must all pass unchanged.

The trait shape is the most consequential design decision in the project.
Worth prototyping on paper before writing code — particularly streaming
pack data (must not buffer), async graph traversal, and atomic
receive-pack (write + ref update).

### Phase 5 — `FsGitCli` backend ✓

Full-bypass backend that hands off to git CLI after auth. Use as the
correctness oracle: run the integration tests against both `FsGitoxide`
and `FsGitCli` and verify identical behaviour.

### Phase 5.1 — Optimisations

#### 5.1a — Lazy pack inspection

`inspect_pack` decodes every object in the pack (including blobs and
trees) via zlib inflate just to extract OID, kind, and size.  Auth only
needs commit/tag metadata.  Read the pack entry header to get type and
size without inflating blob/tree data.

#### 5.1b — Bitmap-accelerated have-set

`build_have_set` materialises the entire reachable object graph from
`have` tips into a `HashSet<ObjectId>`.  For large repos this is millions
of OIDs.  Git solves this with `.bitmap` files alongside pack indexes —
a single bitmap lookup replaces the full commit + tree walk.  gitoxide
supports bitmaps via `gix_pack::Bundle`.

#### 5.1c — Ship existing pack data as-is

When a single on-disk pack already covers all wanted objects, skip the
count → compress → chunk pipeline and stream the pack file directly.
`PackCopyAndBaseObjects` mode already copies individual entries, but
whole-pack bypass avoids the per-object overhead entirely.

#### 5.1d — Per-request repo handle

Each `StorageBackend` method calls `gix::open()` independently.  A
single push or fetch opens the same repo multiple times (list_refs,
build_pack/ingest, has_object, compute_push_kind, update_refs).  Cache a
repo handle for the lifetime of the request.

#### 5.1e — Reduce intermediate allocations

- `stream_pack_to_channel`: `counts.into_iter().collect()` re-collects
  unnecessarily.
- `objects_for_fetch_filtered`: final `HashSet` → `Vec` conversion could
  be avoided by returning an iterator or the set itself.
- `ChunkBuffer::new()` starts at zero capacity — pre-allocate.

### Phase 6 — Cross-backend test harness

Parameterise the integration tests over backends. Add benchmarks.
After this phase every subsequent backend gets full coverage for free.

### Phase 7 — Fuzzing

libfuzzer/AFL harness over the protocol parsing layer. Seed corpus from
traffic captures. Run against a minimal in-memory stub.

### Phase 8 — SQL backend (PoC)

SQLite first, then Postgres. Schema:
- `objects(repo, oid, type, data)`
- `refs(repo, name, oid)`
- `commit_parents(repo, commit_oid, parent_oid)` — materialised for
  graph traversal

The cross-backend harness from Phase 6 immediately validates correctness
and surfaces performance characteristics.

---

## Testing strategy

**Cross-backend integration harness** — the same test suite runs against
every backend. Tests make real `git clone`, `git fetch`, `git push` calls
against a live mizzle server backed by each backend in turn. Correctness
parity is verified; timing is recorded for performance comparison.

**Fuzzing** — the protocol parsing layer (pkt-line, fetch args, push
headers) is fuzzed against a minimal in-memory backend stub, independent
of storage.
