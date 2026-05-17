# KV backend plan

Companion to [roadmap.md](roadmap.md),
[distributed-backends.md](distributed-backends.md), and the
now-merged [sql-backend-plan.md](sql-backend-plan.md).  Covers a
transactional KV `StorageBackend` targeting **TiKV** and
**FoundationDB**.  Positioned as a path to HA / geo-replicated
deployments without operating Postgres or CockroachDB clusters.

## Why a KV backend at all

The SQL backend (now merged on `main`) already runs against SQLite and
will run against Postgres / CockroachDB with modest sqlx work.  A KV
backend earns its place if it is meaningfully simpler to operate or
faster on the hot paths.  The case:

- **Objects** are immutable, content-addressed — any KV value works.
- **Refs** need multi-key all-or-nothing CAS — exactly what
  TiKV / FoundationDB transactions provide natively.
- **Graph traversal** (`reachable_excluding`, `compute_push_kind`) is
  an in-process walk over `par/<oid>` keys — no recursive CTEs, no
  query planner, batch-fetch per BFS layer.

No schema, no DDL migrations, no `sqlx::Any` shim between SQLite and a
distributed engine — one impl, HA from day one.

## TiKV vs FoundationDB for the blob-storage question

**TiKV stores blobs, trees, commits, and refs in one keyspace with no
application-level chunking.**  Per-value limit defaults to
~8 MiB (`raftstore.raft-entry-max-size`); enabling **Titan**, TiKV's
integrated blob-file store, pushes the practical limit far higher and
keeps large values out of the Raft log without changing the key API.
For typical git repos every object fits in one key/value pair.

**FoundationDB has a hard 100 KiB value limit and a 10 MiB txn limit.**
Anything larger must be chunked at the application layer:
`("obj", repo, oid, "chunk", i)` with a header key listing length.
Manageable, but it's real code (chunked writer, chunked reader, partial
read with `cap`, orphan-chunk handling on rollback) that TiKV simply
does not need.

**Implication for MVP:** TiKV is the simpler target.  The plan now
treats TiKV as primary and FDB as a "same key layout, add chunking"
secondary target.  The chunking module is feature-gated and only
compiled for `fdb`.

Design decisions made up front:

- **TiKV is the primary target.**  Transactional API
  (Percolator-based), single keyspace for refs / objects / graph, no
  chunking for any object up to the per-value limit.  Enable Titan
  for repos with very large blobs.
- **FoundationDB is a secondary target with the same key layout plus
  chunking.**  Strictly serializable, smaller operational footprint
  than TiKV, but the 100 KiB cap forces chunking for many normal
  blobs.
- **No external blob store in MVP.**  Keep everything in the KV.  A
  later optimisation can offload large blobs to S3 / R2 / Tigris
  keyed by `oid` — content-addressed, no consistency concerns.
- **Pack cache:** the SQL backend's pack-cache code (currently inline
  in `backend/sql/objects.rs`) is extracted to a shared module before
  the KV backend lands.
- **No DDL, no migrations.**  Keys are byte strings; values are bytes.
  Forward compatibility via a single-byte key-prefix scheme and a
  `("meta", "schema_version")` cell read on `open()`.

## Key layout (tuple-encoded, shown human-readable)

Keys are length-prefixed tuples (TiKV: raw bytes; FDB: `tuple` layer).
All numeric components are big-endian.

```
("meta", "schema_version")                  → u32
("repo", <repo_id>, "head")                 → "refs/heads/main"  (optional symref)
("repo", <repo_id>, "exists")               → ""                 (marker)

("ref",  <repo_id>, <name>)                 → <oid:20>
("obj",  <repo_id>, <oid:20>)               → (kind:u8, len:u32, bytes)
("par",  <repo_id>, <commit_oid:20>, <pos:u16>) → <parent_oid:20>
```

**TiKV:** the `obj` value holds the object inline regardless of size,
up to the cluster's per-value limit.  No additional keys.

**FoundationDB:** if `len > 90 KiB`, the inline bytes are replaced
with an empty tail and the body is split into

```
("obj",  <repo_id>, <oid:20>, "chunk", <i:u16>) → bytes  (≤ 80 KiB each)
```

The chunked-blob code lives in `chunked.rs` and is only compiled
under `--features fdb`.

`<repo_id>` is an 8-byte numeric id assigned at `init_repo` time and
stored under `("repo_index", <path>) → <repo_id>`.  Matches the SQL plan's
`repositories.id` indirection — keeps refs/objects keys short and
forge path strings out of hot keys.

Range scans:

- All refs of a repo: prefix `("ref", <repo_id>, …)`.
- All parents of a commit: prefix `("par", <repo_id>, <commit_oid>, …)`.
- All chunks of a large blob: prefix `("obj", <repo_id>, <oid>, "chunk", …)`.

## Types

```rust
pub struct KvBackend {
    db: tikv_client::TransactionClient,  // or foundationdb::Database under --features fdb
    pack_cache_dir: PathBuf,
}

pub struct KvRepo {
    db: tikv_client::TransactionClient,
    repo_id: u64,                 // resolved at open()
}

pub struct KvIngestedPack {
    metadata: PackMetadata,
    inserted_oids: Vec<ObjectId>, // for optional rollback
    repo_id: u64,
}
```

Concrete `db` type is selected by feature flag; the rest of the
backend speaks to a thin `kv::Txn` wrapper so refs/objects/graph code
is engine-agnostic.

`RepoId = PathBuf` — same indirection as the SQL backend.

## Module layout

```
mizzle/src/backend/kv/
├── mod.rs       KvBackend, StorageBackend impl
├── txn.rs       engine-agnostic Txn trait, TiKV + FDB adapters
├── keys.rs      tuple encode/decode, key prefix constants
├── objects.rs   read / write / has
├── refs.rs      list / resolve / update (CAS in single txn)
├── graph.rs     in-process walks over par/ subspace
└── chunked.rs   large-blob chunking helpers (fdb only)
```

---

## Phase 0 — Async `StorageBackend` (already done)

Already merged on `main` ahead of the SQL backend: trait methods
return `impl Future<Output = …> + Send`.  No further work needed.

## Phase 1 — KV infrastructure

### Dependencies

`tikv-client` behind a `tikv` cargo feature flag (primary).
`foundationdb` behind an `fdb` cargo feature flag (secondary).  The
two flags are mutually exclusive; `compile_error!` if both are
enabled.  An `--all-features` test job picks one.

Local dev:

- **TiKV:** `tiup playground` brings up PD + TiKV in one command,
  no schema step.
- **FDB:** single-node Docker image, `fdbcli`, also no schema.

### `init_repo` / `open`

- `init_repo` runs one txn:
  read-modify-write `("next_repo_id")` counter, write
  `("repo_index", <path>) → <id>` and `("repo", <id>, "exists") → ""`
  conditional on absence.  Idempotent.
- `open` reads `("repo_index", <path>)`; returns `KvRepo { repo_id }`.

---

## Phase 2 — Core CRUD

1. **`list_refs`** — range scan over `("ref", <repo_id>, …)`.  Read HEAD
   symref from `("repo", <repo_id>, "head")`, default to
   `refs/heads/main` if absent.  Snapshot is not linearisable
   (matches trait docs).
2. **`resolve_ref`** — point read.
3. **`has_object` / `has_objects`** — point reads; `has_objects` issues
   the lookups concurrently within one snapshot read txn.
4. **`read_blob` / `read_object_raw`** — point read on
   `("obj", repo, oid)`.  Under TiKV the value is the whole object.
   Under FDB, if the inline tail is empty and `len > 90 KiB`,
   range-scan `…, "chunk", *` and concatenate.  Honour `cap` by
   short-circuiting once exceeded.
5. **`read_commit_info`** — `read_object_raw` then
   `inspect::parse_commit_info` (shared with the SQL backend).
6. **`update_refs`** — single transaction:
    - For each `RefUpdate`: read current key, validate CAS against
      `old_oid` (null = create-only / delete CAS rules).
    - If any check fails: abort the txn, return error → protocol
      surfaces `ng <ref> stale info`.
    - Otherwise write all new values / clear all deleted keys, commit.

   FDB serialises the entire batch atomically.  Concurrent pushes to
   the same ref produce a `not_committed` retryable error on one side;
   we surface it as a stale-info rejection without retry (git
   semantics).

---

## Phase 3 — Ingest path

### `ingest_pack`

1. Header check on staged pack → return `None` if empty.
2. Open with `gix_pack::Bundle::at()`.
3. Iterate objects, extracting OID, kind, raw data
   (reuse `inspect::parse_commit_info` exactly like the SQL backend).
4. Write each object as a single `("obj", repo, oid)` key under
   TiKV.  Under FDB, chunk if > 90 KiB.  Batch N objects per txn
   under FDB to stay under the 10 MiB / 5 s txn limits; TiKV's
   txn limits are generous enough that a per-pack batch is fine.
5. For commits: write `("par", repo, commit_oid, i) → parent_oid`
   for each parent in the same per-object batch.
6. Return `KvIngestedPack { metadata, inserted_oids, repo_id }`.

### `inspect_ingested`

Return the pre-computed `PackMetadata` from step 4.  No I/O.

### `rollback_ingest`

Best-effort delete of `inserted_oids` and their parent keys in
batches.  Optional: orphan objects without referencing refs are
harmless, matching the SQL plan's no-op approach.  Keep the
deletion code so tests can verify isolation.

---

## Phase 4 — Graph traversal

No CTEs.  All walks are in-process BFS over `par/` keys.

### `compute_push_kind`

- Either OID null → create / delete classification, no I/O.
- Both non-null → ancestor BFS from `new_oid`, reading
  `par/<repo>/<oid>/*` per step, stop when `old_oid` found
  (FastForward) or when frontier empties (ForcePush).  Cap walk depth
  at a configurable ceiling to bound work for divergent histories.

### `reachable_excluding`

Standard frontier BFS from `from` tips.  Maintain a `visited` set
seeded with the reachable set of `excluding` (computed first, with
the same cap budget split across the two walks).  Honour the `cap`
hard ceiling per the trait contract.

Batch parent lookups: per BFS layer, issue all `par/<oid>/*` range
reads in one snapshot read txn.  Both TiKV and FDB fan these out
concurrently.

This replaces the SQL backend's recursive CTE
([`sql/graph.rs::reachable_excluding`](../mizzle/src/backend/sql/graph.rs))
with a plain in-process BFS.  Worth comparing in benchmarks once
both backends are live — on deep histories the BFS may be cheaper
than a CTE round-trip per layer.

### `tree_diff`

Identical to the SQL plan: `read_object_raw` for both trees,
`gix_object::TreeRef::from_bytes()`, recursive diff.  Reuses the
existing helper.

---

## Phase 5 — `build_pack` (temp gitoxide repo)

Intentionally naive, same shape as the SQL plan.  Correctness over
performance; pack cache (Phase 6) covers the steady-state cost.

1. Enumerate commit OIDs reachable from wants excluding haves (Phase 4
   walk).
2. Walk trees of those commits: read each tree, parse, recurse.
   Accumulate the full object set.
3. Bulk-read all objects.  For chunked blobs, read all chunks in one
   range scan per blob.
4. Write all objects as loose files into a temp gitoxide repo.
5. Run `stream_pack_to_channel` against the temp repo.
6. Return `PackOutput`.

**Shared with SQL backend:** the "temp gitoxide repo populated from
external store" trick.  Worth extracting into
`mizzle/src/backend/temp_pack_repo.rs` once both backends exist.

**Future optimisation (also shared):** a `gix_object::Find` impl
backed by the KV that streams objects into the pack pipeline without
the temp repo round-trip.

---

## Phase 6 — Pack cache (extract from SQL backend)

The merged SQL backend has the pack-cache code inline in
`mizzle/src/backend/sql/objects.rs`
(`pack_cache_key`, `try_cache_hit`, `write_to_cache`, plus the
tee-to-cache flow in `build_pack`).  Layout and key derivation are
already correct and not SQL-specific.

**Pre-work MR (before Phase 6 of this plan):** extract those helpers
into `mizzle/src/backend/pack_cache.rs` with a thin API:

```rust
pub fn cache_path(dir: &Path, repo_id: u64, key: &CacheKey) -> PathBuf;
pub fn try_hit(dir: &Path, repo_id: u64, key: &CacheKey) -> Option<PackOutput>;
pub fn tee_writer(dir: &Path, repo_id: u64, key: &CacheKey) -> impl Write;
pub struct CacheKey(/* SHA-256 of sorted wants ‖ 0x00 ‖ sorted haves ‖ opts */);
```

The SQL backend gets refactored to consume the shared module in the
same MR.  KV backend then uses it unchanged.  Repo-id type changes
from `i64` to `u64` in the shared API — the SQL backend casts.

---

## Phase 7 — Test integration

Mirror the pattern the merged SQL backend established:

1. Add a `kv_backend_access_test!` macro in `tests/common/mod.rs`
   gated by `#[cfg(feature = "tikv")]` (and an `fdb` twin), modelled
   on the existing `sql_backend_access_test!` macro.
2. Add a `kv_backend_from_fs` helper (parallel to
   `sql_backend_from_fs`) that spins up a TiKV / FDB connection
   (cluster-file path from env var), `init_repo`s the backend, and
   ingests a starter pack so parity tests have known refs and
   objects.
3. Add KV arms to `backend_parity.rs` and
   `comparison_regression.rs` for CAS correctness, multi-ref
   atomicity, and concurrent-push serialisation — same shape as the
   SQL tests added in commit `74b4a96`.
4. Add KV variant to `make_servers()` in `benches/backends.rs`.
5. `cargo test` without `--features tikv,fdb` must still pass.
   Integration tests gated on `MIZZLE_TIKV_PD_ADDR` /
   `MIZZLE_FDB_CLUSTER_FILE` env vars.

---

## Ordering

```
Phase 0 (async trait)                     — merged
SQL backend                               — merged (reference impl)
Pre-work: extract pack-cache module       — standalone MR
   │
Phase 1 (KV infra, key layout)
   │
Phase 2 (CRUD: refs + objects)            ← begin test harness wiring here
   │
Phase 3 (ingest)
   │
Phase 4 (graph traversal)
   │
Phase 5 (build_pack)
   │
Phase 6 (consume shared pack_cache)
   │
Phase 7 (test integration — incremental from Phase 2)
```

---

## Harvest from the merged SQL backend

The SQL backend (PR #12, merged) is the reference impl for nearly
everything in this plan.  Re-use, don't redesign:

| Concept | SQL location | KV reuse |
|---|---|---|
| Pack-cache key, layout, tee-to-cache | `backend/sql/objects.rs::pack_cache_key` etc. | Extract to `backend/pack_cache.rs` (pre-work MR), KV consumes verbatim |
| Temp-gitoxide-repo pack builder | `backend/sql/objects.rs::build_pack` body | Copy first, extract to `backend/temp_pack_repo.rs` once both exist |
| `parse_commit_info` for ingest | `inspect.rs` | Reused as-is |
| Per-backend test macro pattern | `sql_backend_access_test!` in `tests/common/mod.rs` | Add sibling `kv_backend_access_test!` macro |
| In-memory test helper | `sql_backend_from_fs` | Add sibling `kv_backend_from_fs` |
| Backend-parity test bodies | `tests/backend_parity.rs::pack_cache_miss_then_hit_sql` and CAS / atomicity / concurrent-push cases | Add KV-gated twins |
| `commit_parents` ingest semantics | `backend/sql/objects.rs::ingest_pack` (parent rows) | Same logic, write to `par/<oid>/<i>` keys instead |

### Suggested ordering

1. **Pre-work MR (small, lands first):**
   - Extract pack-cache helper from `backend/sql/objects.rs` into
     `backend/pack_cache.rs`; refactor SQL backend to consume it.
     Repo-id type widened to `u64` in the shared API.
   - Update `design/roadmap.md` Phase 8 to mark SQL as ✓ and add a
     Phase 8.1 / 9 reference to this plan.
2. **KV backend MR(s):** Phases 1–7 of this plan against TiKV first.
   FDB chunking module added behind `--features fdb` either in the
   same series or follow-up.
3. **Convergence MR (after KV lands):**
   - Extract `temp_pack_repo.rs` from the two `build_pack` impls,
     refactor both to use it.
   - Consider a shared `gix_object::Find` impl factory for the
     "skip the temp repo" optimisation path.

### Trait-surface risk

The trait already moved to async on `main`.  Anything new either
backend wants on `StorageBackend` should land in its own MR, not
inside a backend MR — same rule the SQL backend followed.
