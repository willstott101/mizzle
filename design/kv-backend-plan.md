# KV backend plan

Companion to [roadmap.md](roadmap.md) and
[distributed-backends.md](distributed-backends.md).  Covers a transactional
KV `StorageBackend` targeting **FoundationDB** first, with **TiKV** as a
near-drop-in alternative.  Positioned as a simpler MVP path to HA /
geo-replicated deployments than the SQL backend.

## Why a KV backend at all

The thin storage trait decomposes naturally:

- **Objects** are immutable, content-addressed — any KV value works.
- **Refs** need multi-key all-or-nothing CAS — exactly what
  FoundationDB / TiKV transactions provide natively.
- **Graph traversal** (`reachable_excluding`, `compute_push_kind`) is
  an in-process walk over `parents/<oid>` keys — no recursive CTEs.

The SQL plan ships SQLite for dev, then climbs a ladder to
Postgres/CockroachDB for HA.  A KV backend skips the ladder: one impl,
HA from day one, no schema migrations.

Design decisions made up front:

- **FoundationDB is the primary target.**  Strictly serializable, multi-key
  txns, well-documented multi-region story, small operational footprint.
  TiKV remains a viable second target via the same key layout.
- **Objects ≤ 90 KiB live inline; larger objects chunked.**  FDB's
  100 KiB value limit / 10 MiB txn limit forces chunking for big blobs.
  Threshold tunable; chunk size 80 KiB.  TiKV has no per-value limit
  but the same chunking code is harmless.
- **No external blob store in MVP.**  Keep everything in the KV.  A
  later optimisation can offload large blobs to S3 / R2 / Tigris keyed
  by `oid` — content-addressed, no consistency concerns.
- **Pack cache reuses the SQL plan's filesystem cache** — same on-disk
  layout, same eviction policy, extracted as a shared helper.
- **No DDL, no migrations.**  Keys are byte strings; values are bytes.
  Forward compatibility via a single-byte key-prefix scheme.

## Key layout (FoundationDB tuple-style, shown human-readable)

Keys are length-prefixed tuples.  `\x` denotes a literal type-tag byte;
all numeric components are big-endian.

```
("meta", "schema_version")                  → u32
("repo", <repo_id>, "head")                 → "refs/heads/main"  (optional symref)
("repo", <repo_id>, "exists")               → ""                 (marker)

("ref",  <repo_id>, <name>)                 → <oid:20>
("obj",  <repo_id>, <oid:20>)               → (kind:u8, len:u32, inline:bytes)
                                              -- inline if len ≤ 90 KiB
("obj",  <repo_id>, <oid:20>, "chunk", <i>) → bytes              (for large objects)
("par",  <repo_id>, <commit_oid:20>, <pos:u16>) → <parent_oid:20>
```

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
    db: foundationdb::Database,   // or tikv_client::TransactionClient
    pack_cache_dir: PathBuf,
}

pub struct KvRepo {
    db: foundationdb::Database,
    repo_id: u64,                 // resolved at open()
}

pub struct KvIngestedPack {
    metadata: PackMetadata,
    inserted_oids: Vec<ObjectId>, // for optional rollback
    repo_id: u64,
}
```

`RepoId = PathBuf` — same indirection as the SQL backend.

## Module layout

```
mizzle/src/backend/kv/
├── mod.rs       KvBackend, StorageBackend impl
├── keys.rs      tuple encode/decode, key prefix constants
├── objects.rs   read / write / has, inline + chunked
├── refs.rs      list / resolve / update (CAS in single txn)
├── graph.rs     in-process walks over par/ subspace
└── chunked.rs   large-blob chunking helpers
```

---

## Phase 0 — Async `StorageBackend` (already done)

Shared prerequisite with the SQL backend.  Already merged on `main`:
trait methods return `impl Future<Output = …> + Send`.  No further
work needed.

## Phase 1 — KV infrastructure

### Dependencies

`foundationdb = "0.9"` (or current).  Behind a `fdb` cargo feature
flag.  Optional second feature `tikv` adds `tikv-client`; the two
flags are mutually exclusive via a small `cfg`-gated dispatch.

Local dev: FDB ships a single-node Docker image and a `fdbcli` for
ops.  Bring-up is two commands; no schema step.

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
4. **`read_blob` / `read_object_raw`** — read header key.  If inline,
   return the bytes.  If chunked, range-scan `…, "chunk", *` and
   concatenate.  Honour `cap` by short-circuiting once `cap` exceeded.
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
3. Iterate objects, extracting OID, kind, raw data.
4. Write each object: inline if size ≤ 90 KiB, else chunk.  Use
   batches of N objects per txn to stay under FDB's 10 MiB / 5 s txn
   limits.  Chunk uploads for one large blob may span multiple txns
   safely — orphan chunks without a header are harmless (and GC'd
   on a future pass).
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
reads in one snapshot read txn.  FDB happily fans these out
concurrently.

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

## Phase 6 — Pack cache (shared with SQL backend)

Verbatim from the SQL plan, Phase 6.  Filesystem layout, cache key
(`SHA-256(sorted_wants ‖ 0x00 ‖ sorted_haves)`), eviction, and
no-invalidation rationale all apply unchanged.

**Recommendation:** extract the pack cache as
`mizzle/src/backend/pack_cache.rs` and share it between the SQL and
KV backends.  Whichever backend lands first writes it; the second
just consumes it.

---

## Phase 7 — Test integration

1. Generalise `dual_backend_test!` and `dual_backend_access_test!` in
   `tests/common/mod.rs` into an N-arm macro.  Each impl gated behind
   its cargo feature.  SQL plan already wants this change for `sql`;
   coordinate so the macro is generalised once.
2. Add `fdb`-gated arm to `backend_parity.rs`,
   `comparison_regression.rs`, and `benches/backends.rs`.
3. `cargo test` without `--features fdb` must pass.  FDB tests require
   a running cluster; gate behind `MIZZLE_FDB_CLUSTER_FILE` env var
   like other integration suites.

---

## Ordering

```
Phase 0 (async trait)          — already merged
   │
Phase 1 (KV infra, key layout)
   │
Phase 2 (CRUD: refs + objects) ← can begin adding to test harness here
   │
Phase 3 (ingest)
   │
Phase 4 (graph traversal)
   │
Phase 5 (build_pack)
   │
Phase 6 (pack cache — shared module)
   │
Phase 7 (test integration — incremental from Phase 2)
```

---

## Parallel work with the SQL backend

Both backends can be developed in parallel.  The two branches touch
the following shared areas — coordinate or extract first to avoid
merge pain.

### Shared, low-risk (separate files, no contention)

- `mizzle/src/backend/sql/` vs `mizzle/src/backend/kv/` — different
  submodules.
- Cargo features (`sql` vs `fdb` / `tikv`) — independent.
- New deps (`sqlx` vs `foundationdb`) — independent.

### Shared, needs coordination

- **`StorageBackend` trait** (`mizzle/src/backend/mod.rs`).  Already
  async, so neither branch needs to migrate it.  Any *new* method
  either branch adds is a hard conflict — propose trait additions
  via a small precursor MR.
- **Shared types** (`PackMetadata`, `RefsSnapshot`, `HeadInfo`, etc.
  in `backend/mod.rs`).  Read-only consumers; safe unless a backend
  needs a new field.
- **Test harness macros** (`dual_backend_test!`,
  `dual_backend_access_test!` in `tests/common/mod.rs`).  Both
  backends need to add a third (and fourth) arm.  **Extract once:**
  convert to a list-driven macro before either backend lands, in a
  standalone MR.  Avoids two backends each rewriting the same macro.
- **`benches/backends.rs` `make_servers()`**.  Same — list-driven
  factory once, both backends append.

### Should be extracted up front

- **Pack cache** (`mizzle/src/backend/pack_cache.rs`).  Both Phase 6's
  are identical.  Land as a standalone MR first; both backends
  consume it.  Roughly: cache key derivation, on-disk layout,
  eviction, "stream cached file or build-and-tee" helper.
- **Temp-gitoxide-repo pack builder**
  (`mizzle/src/backend/temp_pack_repo.rs`).  Both Phase 5's
  materialise objects into a temp repo and run gitoxide's pack
  pipeline.  Extract on the second implementation, or up front if
  both branches start at the same time.

### Risk areas (real merge pain if not coordinated)

- **`inspect.rs` `parse_commit_info`** changes — neither backend
  should touch this without a precursor MR; both depend on it.
- **`Comparison` trait** behaviour — already stable, but if either
  backend needs richer access (e.g. a new
  `StorageBackend::read_tree_metadata`), both will want it.
- **Roadmap text** — both branches will edit `design/roadmap.md`
  Phase 8.  Land a "split Phase 8 into 8a SQL / 8b KV" edit first.

### Suggested coordination sequence

1. **Pre-work MR (standalone, small):**
   - Extract pack-cache helper.
   - Generalise the dual-backend test macros to list-driven.
   - Split roadmap Phase 8 into 8a (SQL) / 8b (KV).
2. **Parallel development:**
   - SQL branch implements `mizzle/src/backend/sql/`.
   - KV branch implements `mizzle/src/backend/kv/`.
   - Neither branch modifies trait surface.
3. **Convergence MR (after second backend lands):**
   - Extract `temp_pack_repo.rs` and refactor both to use it.
   - Consider a shared `gix_object::Find` impl factory for the
     future optimisation path.

### Verdict

The two branches are about 90 % independent.  The 10 % overlap is
real but localised to test harness wiring, a couple of shared
helpers, and the roadmap doc.  A 30-minute pre-work MR eliminates
most of it.  Recommend running them in parallel rather than
sequencing.
