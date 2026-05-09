# SQL backend plan

Companion to [roadmap.md](roadmap.md) Phase 8 and
[distributed-backends.md](distributed-backends.md).  Covers the staged
work to add a SQL `StorageBackend`, generic over SQLite (initial) and
CockroachDB/Postgres (future), with local filesystem pack caching.

Design decisions made up front:

- **Everything goes through SQL except packs.**  Objects, refs, and
  the commit-parent graph are always read from and written to the
  database.  No local object/ref/graph caches.
- **Pack caching uses the local filesystem.**  Built packs are cached
  on local disk, keyed by `hash(repo, wants, haves)`.  Cache misses
  build from SQL; hits stream the cached file.
- **Start with `sqlx::Sqlite`.**  Compile-time query checking during
  initial development.  Move to `sqlx::Any` or add a Postgres
  feature when CockroachDB support lands.
- **Async `StorageBackend` trait.**  The trait migrates to async
  (`-> impl Future<…> + Send`) before the SQL backend is written.
  This avoids the `Handle::block_on` hack inside sync methods and
  aligns with the existing `SshAuth` convention.

## Phase 0 — Async `StorageBackend` migration

**Standalone MR, prerequisite for all SQL work.**

Currently all 41 backend call sites in `serve.rs`, `fetch.rs`,
`receive.rs`, and `auth.rs` call `StorageBackend` methods
synchronously on the async executor — no `spawn_blocking` anywhere.
The SQL backend makes this untenable (network I/O to CockroachDB on
the executor thread).

### Trait changes

`StorageBackend` methods become
`fn foo(&self, …) -> impl Future<Output = R> + Send`, matching the
existing `SshAuth::authorize` convention.  No `async-trait` crate
needed (Rust 1.93, well past the 1.75/1.79 stabilisation).

### Filesystem backend updates

`FsGitoxide` and `FsGitCli`: wrap CPU-bound work in
`spawn_blocking`.  Cheap methods (`has_object`, `resolve_ref`) can
return immediately.

### Comparison / auth cascade

`ConcreteComparison` calls nine backend methods
(`reachable_excluding`, `read_commit_info`, `tree_diff`,
`read_object_raw`, `read_blob`).  Two options:

1. **Make `Comparison` async** — cascades into
   `RepoAccess::authorize_push` and `post_receive`, which receive
   `&dyn Comparison`.  Cleanest long-term.
2. **Pre-compute** — eagerly evaluate `new_commits`, `dropped_commits`,
   `ref_diff` before calling `authorize_push`, so `Comparison` stays
   sync and receives owned data.  Loses laziness.

Option 1 is the default plan.  If the dyn-async ergonomics are too
painful, fall back to option 2.

### Pack streaming fix

`stream_pack_sideband` (`fetch.rs`) performs a blocking
`reader.read()` directly on the async executor.  Fix by wrapping
the read loop in `spawn_blocking`.  `PackOutput.reader` stays as
sync `Read` for now.

### Scope

| Area | Call sites | Change |
|---|---|---|
| `StorageBackend` trait | 1 | methods → `-> impl Future + Send` |
| `FsGitoxide` impl | 17 methods | wrap heavy ops in `spawn_blocking` |
| `FsGitCli` impl | 17 methods | same |
| `serve.rs` | 28 | `.await` backend calls |
| `fetch.rs` | 3 + `stream_pack_sideband` | `.await` + fix blocking read |
| `receive.rs` | 1 | `.await` |
| `auth.rs` / `Comparison` | 9 | async trait or pre-compute |

### Acceptance

All existing tests pass.  `cargo test` and
`cargo test --features ssh` green.

---

## Phase 1 — SQL infrastructure

### Dependencies

`sqlx` with `runtime-tokio`, `sqlite` features.  Behind a
`sql` cargo feature flag.

### Schema

```sql
CREATE TABLE repositories (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL UNIQUE,
    head  TEXT                       -- symref target, e.g. "refs/heads/main"
);

CREATE TABLE objects (
    repo_id  INTEGER NOT NULL REFERENCES repositories(id),
    oid      BLOB NOT NULL,          -- 20 bytes (SHA-1)
    kind     INTEGER NOT NULL,       -- 0=blob, 1=tree, 2=commit, 3=tag
    data     BLOB NOT NULL,
    PRIMARY KEY (repo_id, oid)
);

CREATE TABLE refs (
    repo_id  INTEGER NOT NULL REFERENCES repositories(id),
    name     TEXT NOT NULL,
    oid      BLOB NOT NULL,
    PRIMARY KEY (repo_id, name)
);

CREATE TABLE commit_parents (
    repo_id     INTEGER NOT NULL REFERENCES repositories(id),
    commit_oid  BLOB NOT NULL,
    parent_oid  BLOB NOT NULL,
    position    INTEGER NOT NULL,
    PRIMARY KEY (repo_id, commit_oid, position)
);
```

`repositories.head` is nullable.  When null, HEAD defaults to
`refs/heads/main`.  Forges that let users configure the default
branch set it explicitly; forges with a fixed convention leave it
null.

### Types

```rust
pub struct SqlBackend {
    pool: sqlx::SqlitePool,
    pack_cache_dir: PathBuf,
}

pub struct SqlRepo {
    pool: sqlx::SqlitePool,
    repo_db_id: i64,
}

pub struct SqlIngestedPack {
    metadata: PackMetadata,
    inserted_oids: Vec<ObjectId>,
    repo_db_id: i64,
}
```

`RepoId = PathBuf` — compatible with the existing test harness.
The path has no filesystem meaning for the SQL backend; it is a
key.

### Module layout

```
mizzle/src/backend/sql/
├── mod.rs       SqlBackend, StorageBackend impl
├── schema.rs    DDL, table creation
├── objects.rs   object read/write/has
├── refs.rs      ref list/resolve/update (CAS)
└── graph.rs     commit_parents, ancestor checks, reachable
```

---

## Phase 2 — Core CRUD operations

Implement in dependency order:

1. `init_repo` — `INSERT … ON CONFLICT DO NOTHING`, optionally
   set `head`.
2. `open` — `SELECT id FROM repositories WHERE path = ?`.
3. `list_refs` / `resolve_ref` — selects on `refs`.  `list_refs`
   synthesises `HeadInfo` from `repositories.head` (or default).
4. `has_object` / `has_objects` — `SELECT EXISTS(…)` / batch
   `IN (…)`.
5. `read_blob`, `read_object_raw`, `read_commit_info` — select
   from `objects`.  `read_commit_info` reuses `parse_commit_info`
   from `inspect.rs`.
6. `update_refs` — single transaction, CAS per ref:
   `UPDATE refs SET oid = $new WHERE … AND oid = $old`, check
   affected rows.  Create/delete handled with
   `INSERT`/`DELETE`.  Rollback the entire transaction on any CAS
   failure (all-or-nothing semantics).

---

## Phase 3 — Ingest path

### `ingest_pack`

1. Read the staged pack file header; return `None` if zero objects.
2. Open with `gix_pack::Bundle::at()`.
3. Iterate all objects: extract OID, kind, raw data.
4. `INSERT INTO objects … ON CONFLICT DO NOTHING`.
5. For commits: parse parents, `INSERT INTO commit_parents`.
6. Return `SqlIngestedPack` holding the pack metadata and the
   list of newly inserted OIDs.

### `inspect_ingested`

Return the metadata already computed during `ingest_pack`.  No
additional I/O.

### `rollback_ingest`

No-op.  Orphan objects without ref pointers are harmless — same
as unreferenced packs on the filesystem.  GC is a future concern.

---

## Phase 4 — Graph traversal

### `compute_push_kind`

Handle create/delete by inspecting `old_oid` / `new_oid` for
zero-id.  For updates, ancestor check via recursive CTE:

```sql
WITH RECURSIVE ancestors(oid) AS (
    SELECT ?new_oid
    UNION ALL
    SELECT cp.parent_oid
      FROM commit_parents cp
      JOIN ancestors a ON cp.commit_oid = a.oid
     WHERE cp.repo_id = ?
)
SELECT EXISTS(SELECT 1 FROM ancestors WHERE oid = ?old_oid);
```

Ancestor → `FastForward`, not ancestor → `ForcePush`.

### `reachable_excluding`

Similar recursive CTE from `from` tips, pruning at `excluding`
OIDs, with `LIMIT cap`.  Returns commit OIDs.

### `tree_diff`

Read both tree objects from `objects`, parse with
`gix_object::TreeRef::from_bytes()`, diff entries.  Recursive for
changed subtrees.

---

## Phase 5 — `build_pack` (temp gitoxide repo)

Initial implementation — intentionally naive, correctness over
performance.  The pack cache (Phase 6) covers the cost.

1. Enumerate commit OIDs via recursive CTE (want-reachable minus
   have-reachable).
2. Walk trees of those commits: read tree objects from SQL, parse,
   recurse.  Collect all unique OIDs (commits + trees + blobs +
   tags).
3. Subtract objects reachable from have-side (same process).
4. Bulk `SELECT oid, kind, data FROM objects WHERE repo_id = ?
   AND oid IN (…)`.
5. Write all objects as loose files into a temporary gitoxide
   repo.
6. Run the standard gitoxide pack pipeline
   (`stream_pack_to_channel`) against the temp repo.
7. Return `PackOutput` as normal.

**Future optimisation:** implement `gix_object::Find` on a
SQL-backed struct (or an in-memory `HashMap` pre-fetched from SQL)
to feed the pack pipeline directly without the temp repo
round-trip.

---

## Phase 6 — Pack cache (local filesystem)

### Storage

```
{pack_cache_dir}/{repo_db_id}/{cache_key}.pack
```

### Cache key

`SHA-256(sorted_wants ‖ 0x00 ‖ sorted_haves)` — deterministic,
content-addressed.

### Flow

1. Compute cache key from want/have sets.
2. Hit → stream the cached `.pack` file (same path as
   `ship_pack_as_is`).
3. Miss → build pack per Phase 5, tee output to cache file,
   stream to client.
4. Eviction: LRU by mtime, configurable max cache size.

### Invalidation

None.  A cached pack is never wrong.  New pushes change the
want/have sets at the client, producing a different cache key.
Stale entries waste disk until evicted.

---

## Phase 7 — Test integration

1. Add third arm to `dual_backend_test!` and
   `dual_backend_access_test!` in `tests/common/mod.rs`, gated
   behind `#[cfg(feature = "sql")]`.
2. Add `#[test]` functions in `backend_parity.rs` for CAS
   correctness, multi-ref atomicity, and concurrent-push
   serialisation.
3. Add functions in `comparison_regression.rs`.
4. Add to `make_servers()` in `benches/backends.rs`.
5. `cargo test` without `--features sql` must still pass.

---

## Ordering

```
Phase 0 (async trait — standalone MR)
   │
Phase 1 (SQL infra)
   │
Phase 2 (CRUD) ← can begin adding to test harness here
   │
Phase 3 (ingest)
   │
Phase 4 (graph traversal)
   │
Phase 5 (build_pack)
   │
Phase 6 (pack cache)
   │
Phase 7 (test integration — incremental from Phase 2)
```

Phases 1–7 can ship as a single MR or be broken up at natural
boundaries (e.g. 1–3, 4–6, 7).
