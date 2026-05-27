# Roadmap

## Implementation phases

### Phase 1 — Extract `mizzle-proto` ✓

Pkt-line, capability parsing, filter/shallow utilities in a standalone
crate with no storage dependency.

### Phase 2 — SSH transport ✓

russh-based SSH server with deferred auth (`SshAuth` trait).

### Phase 3 — Runtime consolidation ✓

Commit to tokio.  Remove trillium/actix/rocket adapters, keep axum as
canonical HTTP integration.

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

### Phase 5.1 — Performance testing infrastructure

Instrument `build_have_set` and the pack-generation path with `tracing`
spans.  Build the deterministic reference repos (`medium`, `deep`) and
extend the benchmark suite to cover them.  See
`design/performance-testing.md` (Steps 1–3) for the full plan.

These benchmarks are a prerequisite for 5.2b: the `deep`-repo
incremental-fetch span data will show whether the bitmap optimisation
is actually needed before committing to the implementation.

### Phase 5.2 — Optimisations

#### 5.2a — Lazy pack inspection ✓

`inspect_pack` previously decoded every object in the pack (including
blobs and trees) via zlib inflate just to extract OID, kind, and size.
Auth only needs commit/tag metadata.  `mizzle/src/inspect.rs` now reads
the pack entry header for type + size on non-deltas, and `decode_header`
(partial inflate of ~32 bytes per delta hop) for deltas — full inflate
runs only for commits and tags.

#### 5.2b — Bitmap-accelerated have-set ✓

`build_have_set` materialises the entire reachable object graph from
`have` tips into a `HashSet<ObjectId>`.  For large repos this is millions
of OIDs.  Git solves this with `.bitmap` files alongside pack indexes —
a single bitmap lookup replaces the full commit + tree walk.

Gitoxide 0.67/0.68 exposes the EWAH primitive (`gix-bitmap`) but not a
reachability-bitmap reader, so `mizzle/src/bitmap.rs` parses the `.bitmap`
+ `.rev` files directly (v1 format, sha1 only).  `FsGitoxide::build_pack`
calls `try_bitmap_have_set` first; on any uncovered have (or no bitmap)
it falls back to the original walker.  See `design/performance-testing.md`
§3.1 for the comparison bench and spans.

#### 5.2c — Ship existing pack data as-is ✓

When an on-disk pack already covers exactly the request closure, skip
the count → compress → chunk pipeline and stream the pack file directly.
`PackCopyAndBaseObjects` mode already copies individual entries, but
whole-pack bypass avoids the per-object overhead entirely.

`mizzle/src/pack_reuse.rs` exposes `find_reusable_pack` and
`pack_is_exactly_reusable` — public utilities any backend can call to
test a candidate pack.  The check is conservative: a pack is reusable
only when its `.bitmap` proves it contains *exactly* the closure of
`want \ have` (no bandwidth-wasting extras, no missing objects).
`PackBitmap::covers_exactly` is the underlying primitive in
`mizzle/src/bitmap.rs`.

`FsGitoxide::build_pack` calls `find_reusable_pack` first when no
`filter` / `deepen` / `thin_pack` is requested; on a hit it returns a
`PackOutput` whose reader is the on-disk `.pack` file.  The typical
trigger is a clone against a `git repack -adb`'d repo.  Other backends
with locally-cached packs (e.g. networked backends with an SSD pack
cache) can use the same utilities directly rather than re-implementing
the bitmap coverage check.

The `clone_full/<backend>/{nobitmap,bitmap}` bench in
`benches/backends.rs` exercises both paths against the `deep` repo;
spans land in `target/criterion/pack-reuse-spans.jsonl`.

#### 5.2d — Per-request repo handle ✓

Each `StorageBackend` method calls `gix::open()` independently.  A
single push or fetch opens the same repo multiple times (list_refs,
build_pack/ingest, has_object, compute_push_kind, update_refs).  Cache a
repo handle for the lifetime of the request.

#### 5.2e — Reduce intermediate allocations

- `stream_pack_to_channel`: `counts.into_iter().collect()` re-collects
  unnecessarily.
- `objects_for_fetch_filtered`: final `HashSet` → `Vec` conversion could
  be avoided by returning an iterator or the set itself.
- `ChunkBuffer::new()` starts at zero capacity — pre-allocate.

### Phase 6 — Cross-backend test harness ✓

Parameterise the integration tests over backends. Add benchmarks.
After this phase every subsequent backend gets full coverage for free.

### Phase 7 — Fuzzing

libfuzzer/AFL harness over the protocol parsing layer. Seed corpus from
traffic captures. Run against a minimal in-memory stub.

### Phase 7.1 — Async `StorageBackend` migration

Standalone MR.  Convert all `StorageBackend` methods to
`-> impl Future<…> + Send`.  Filesystem backends wrap CPU-bound work
in `spawn_blocking`.  Fix `stream_pack_sideband` blocking read.
`Comparison` trait goes async.  See
[sql-backend-plan.md](sql-backend-plan.md) Phase 0 for full scope.

### Phase 8 — SQL backend

SQLite first (via `sqlx::Sqlite`), then CockroachDB/Postgres.
Full plan in [sql-backend-plan.md](sql-backend-plan.md).

Schema: `repositories`, `objects`, `refs`, `commit_parents`.
`build_pack` initially uses a temp gitoxide repo populated from SQL
— intentionally naive, correctness over performance.  Local
filesystem pack cache (keyed by want/have sets) covers the cost
after first build.  Future optimisation: `gix_object::Find` impl
backed by SQL or an in-memory prefetch, eliminating the temp repo
round-trip.

The cross-backend harness from Phase 6 immediately validates correctness
and surfaces performance characteristics.

### Phase 9 — Git LFS

LFS as a storage concern orthogonal to the git `StorageBackend`: a thin
`LfsStore` trait joined to the auth layer only by `RepoId`, so the LFS
object store and the git object store can be the same backend (coupled)
or different ones (e.g. S3 for LFS, SQLite for git).  The batch API
returns URLs rather than bytes, so a store chooses per object between
proxying through mizzle and redirecting to a presigned URL.  Full plan in
[lfs-backend-plan.md](lfs-backend-plan.md).

---

## Testing strategy

**Cross-backend integration harness** — the same test suite runs against
every backend. Tests make real `git clone`, `git fetch`, `git push` calls
against a live mizzle server backed by each backend in turn. Correctness
parity is verified; timing is recorded for performance comparison.

**Fuzzing** — the protocol parsing layer (pkt-line, fetch args, push
headers) is fuzzed against a minimal in-memory backend stub, independent
of storage.
