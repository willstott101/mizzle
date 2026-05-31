# Git LFS plan

Companion to [architecture.md](architecture.md),
[auth.md](auth.md), and [distributed-backends.md](distributed-backends.md).
Covers Git LFS support as a storage concern that sits *beside* the git
`StorageBackend`, not inside it — so the LFS object store and the git
object store can be the **same** backend (coupled) or **different**
backends (e.g. S3 for LFS, SQLite for git) with no change to the
protocol or auth layers.

## Why LFS is a separate trait, not new `StorageBackend` methods

A Git LFS object is an opaque, immutable blob addressed by its SHA-256
content hash.  It never enters the packfile protocol: the git repo
stores only a tiny **pointer blob**

```
version https://git-lfs.github.com/spec/v1
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393
size 12345
```

which mizzle's git backend already handles as an ordinary blob.  The
real bytes move over a completely separate HTTP API — the **batch API**
plus a **transfer adapter** — keyed by sha256, with its own auth and its
own lifecycle.

This is exactly the "Objects" access pattern
[distributed-backends.md](distributed-backends.md) already isolates:

> **Objects** (`read_object`, `has_object`, `write_objects`): immutable,
> content-addressed — any CAS or blob store works.

LFS is that pattern with none of the ref/graph/pack machinery.  Folding
it into `StorageBackend` would force every git backend to grow a blob
store it may not want, and would couple the two storage choices that we
specifically want to keep independent.  So LFS gets its own thin trait,
`LfsStore`, constrained to the same `RepoId` as the auth layer.  Whether
it resolves to the same physical backend as git is a **wiring decision**,
identical in shape to the auth↔storage coupling we already have.

Design decisions made up front:

- **`LfsStore` is orthogonal to `StorageBackend`.**  They share only the
  `RepoId` associated type (`L: LfsStore<RepoId = A::RepoId>`), enforced
  at the `serve_*` entry point exactly like
  `B: StorageBackend<RepoId = A::RepoId>` is today.  Coupled = one type
  implements both; separated = two types sharing the id type.
- **The batch API returns URLs, not bytes.**  This indirection is what
  makes "S3 for LFS" invisible to the client and to mizzle.  Each object
  resolves to a `TransferAction`: either `Proxy` (mizzle streams the
  bytes through its own transfer endpoint) or
  `Redirect { href, … }` (the client transfers directly against a
  presigned URL).  A store picks per object; the batch handler and the
  git client never know the difference.
- **Protocol types live in `mizzle-proto`.**  Batch request/response
  JSON, the transfer-adapter enums, `LfsOid`, and pointer-blob parsing
  have no storage dependency and belong in the proto crate (hard rule:
  `mizzle-proto` has no storage dependency).
- **Auth gates at the batch boundary.**  The batch call is the single
  authorisation point; a new `RepoAccess::authorize_lfs(op)` hook
  (default `Ok`) classifies download vs upload.  Bytes only flow after
  it passes — the LFS analogue of "pack data is staged, not stored,
  until auth passes".
- **No git CLI, ever.**  LFS is pure HTTP + blob I/O; nothing here shells
  out to `git`.  Even `FsLfs` is a plain directory of files.
- **Read auth follows the v1 stance.**  [auth.md](auth.md) declares
  read-side authorisation out of scope for v1 ("if you can reach the
  repo, you can fetch everything").  LFS download inherits that: the
  default `authorize_lfs(Download)` is `Ok`.  Upload is gated.

## Storage topologies (the headline)

Because `LfsStore` and `StorageBackend` are independent traits joined
only by `RepoId`, every combination is a one-line wiring change.

| Topology | git store | LFS store | How |
|---|---|---|---|
| **No LFS** | `SqlBackend` | — | `serve_with_backend` (today's path) |
| **Coupled** | `SqlBackend` | `SqlBackend` | one type implements both traits; pass it twice |
| **Coupled (KV)** | `KvBackend` | `KvBackend` | LFS bytes under `("lfs", repo, oid)` keys |
| **Separated** | `SqlBackend` | `S3LfsStore` | two types, same `RepoId = PathBuf` |
| **Separated (FS git, S3 LFS)** | `FsGitoxide` | `S3LfsStore` | classic small-repo + offloaded big files |
| **Hybrid** | `KvBackend` | `KvBackend` + S3 spill | store decides `Proxy` vs `Redirect` per blob size |

### Coupled

A single backend type implements both traits.  The SQL backend gains an
`lfs_objects(repo_id, oid BLOB[32], size INTEGER, data BLOB)` table; the
KV backend writes `("lfs", <repo_id>, <oid:32>) → bytes` (the
[kv-backend-plan.md](kv-backend-plan.md) already foreshadows
"offload large blobs to S3 / R2 / Tigris keyed by `oid`").  Both return
`TransferAction::Proxy` and serve bytes through mizzle.

```rust
let backend = SqlBackend::new(pool, cache_dir);
// same instance is both git store and LFS store
serve_with_backends(access, backend.clone(), backend, path, limits, req).await
```

### Separated

```rust
let git = SqlBackend::new(pool, cache_dir);
let lfs = S3LfsStore::new(bucket, region, creds);   // RepoId = PathBuf, like git
serve_with_backends(access, git, lfs, path, limits, req).await
```

`S3LfsStore` returns `TransferAction::Redirect` with presigned URLs, so
the multi-gigabyte transfer never touches the mizzle process — the
client `PUT`s/`GET`s straight to S3.  Mismatched id types
(`S3LfsStore<RepoId = Uuid>` against `RepoAccess<RepoId = PathBuf>`) are
a compile error, the same guarantee the git side already gives.

### Verdict on the user's question

Both are easy, and for the same structural reason the project already
relies on: storage identity is an associated type carried on
`RepoAccess`, and backends are plain values handed to `serve_*`.  Adding
LFS adds one more value of one more trait to that call.  Coupling is
"pass the same value twice"; separation is "pass two values".  No trait
in the protocol or auth layer changes shape.

## The `LfsStore` trait

Lives in `mizzle/src/lfs/mod.rs`.  Mirrors `StorageBackend`'s async
(RPITIT) convention and its `RepoId` / `Repo` / `open` shape so a coupled
backend can reuse its existing repo handle.

```rust
pub trait LfsStore: Send + Sync + 'static {
    type RepoId: Send + Sync + Clone + 'static;
    /// Expected to be a cheap handle (e.g. a cloned pool reference), not
    /// an expensive resource — constructed once per request.
    type Repo: Send + Sync;

    fn open(&self, id: &Self::RepoId)
        -> impl Future<Output = Result<Self::Repo>> + Send;

    /// Existence + size.  `None` = object absent.
    fn stat(&self, repo: &Self::Repo, oid: &LfsOid)
        -> impl Future<Output = Result<Option<u64>>> + Send;

    /// How the client should download a present object.
    fn download_action(&self, repo: &Self::Repo, oid: &LfsOid, size: u64)
        -> impl Future<Output = Result<TransferAction>> + Send;

    /// How the client should upload a missing object.
    fn upload_action(&self, repo: &Self::Repo, oid: &LfsOid, size: u64)
        -> impl Future<Output = Result<TransferAction>> + Send;

    /// Stream a stored object to the client (proxy transfer only).
    ///
    /// Only called when `download_action` returned `TransferAction::Proxy`.
    /// Stores that always return `Redirect` never have this called; they
    /// should return `Err(LfsError::ProxyNotSupported)`.
    fn read(&self, repo: &Self::Repo, oid: &LfsOid)
        -> impl Future<Output = Result<impl AsyncRead + Send>> + Send;

    /// Receive and store an object from the client (proxy transfer only).
    ///
    /// Only called when `upload_action` returned `TransferAction::Proxy`.
    /// Stores that always return `Redirect` never have this called; they
    /// should return `Err(LfsError::ProxyNotSupported)`.
    /// Implementations must verify the sha256 of the received bytes
    /// matches `oid` and reject on mismatch.
    fn write(&self, repo: &Self::Repo, oid: &LfsOid, size: u64,
             src: impl AsyncRead + Send + Unpin)
        -> impl Future<Output = Result<()>> + Send;
}

pub enum TransferAction {
    /// mizzle streams the bytes via its own transfer endpoint; mizzle
    /// fills in the href (`<lfs-base>/objects/<oid>`).
    Proxy,
    /// Client transfers directly against this URL (e.g. presigned S3).
    Redirect {
        href: String,
        header: Vec<(String, String)>,
        expires_at: Option<std::time::SystemTime>,
    },
}
```

`LfsOid` is a `[u8; 32]` newtype (sha256), distinct from gix `ObjectId`
(a git object name).  It lives in `mizzle-proto` with the pointer parser.

For redirect stores (S3), presigning is a local HMAC operation; only
`stat` (HeadObject) is a network round-trip per batch object.  If
profiling shows the separate `stat` + `*_action` calls are a bottleneck,
a `batch_object(op, oid, size)` default method can fuse them — deferred
until there is evidence it matters.

Reference stores at a glance:

| Store | `download/upload_action` | `read`/`write` |
|---|---|---|
| `FsLfs` | `Proxy` | read/write `objects/<oid[0:2]>/<oid[2:4]>/<oid>` |
| `SqlLfs` (coupled) | `Proxy` | `SELECT`/`INSERT` on `lfs_objects` |
| `KvLfs` (coupled) | `Proxy` | get/put `("lfs", repo, oid)` |
| `S3LfsStore` | `Redirect { presigned }` | `Err(LfsError::ProxyNotSupported)` |

## Wiring into `serve`

Two entry points, mirroring the existing `serve` / `serve_with_backend`
pair:

```rust
/// LFS-only handler.  Mount for `*/info/lfs/...` paths.
pub async fn serve_lfs<A, L>(
    access: A, lfs: L, path: &str, req: Request,
) -> Response
where A: RepoAccess + Send + 'static,
      L: LfsStore<RepoId = A::RepoId> + Clone + Send + 'static;

/// Git + LFS in one dispatcher for the catch-all `/{*key}` route:
/// routes `info/lfs/...` to `serve_lfs`, everything else to the git path.
pub async fn serve_with_backends<A, B, L>(
    access: A, git: B, lfs: L, path: &str,
    limits: &ProtocolLimits, req: Request,
) -> Response
where A: RepoAccess + Send + 'static,
      B: StorageBackend<RepoId = A::RepoId> + Clone + Send + 'static,
      L: LfsStore<RepoId = A::RepoId> + Clone + Send + 'static;
```

Path resolution reuses the existing
`path.rsplit_once(".git/")` split in `servers/axum.rs`: an LFS request
has `service_path` beginning `info/lfs/`.  Forges that don't want LFS
keep calling `serve_with_backend` unchanged.

LFS service paths to dispatch:

| Method + path | Handler |
|---|---|
| `POST info/lfs/objects/batch` | batch API |
| `GET  info/lfs/objects/<oid>` | proxy download (Proxy stores only) |
| `PUT  info/lfs/objects/<oid>` | proxy upload (Proxy stores only) |
| `POST info/lfs/objects/verify` | post-upload size/existence check |
| `*    info/lfs/locks…` | file-locking API — out of scope (see below) |

## Auth

One hook on `RepoAccess`, defaulted so it is non-breaking:

```rust
/// Authorise an LFS transfer batch.  Called once per batch request,
/// before any object bytes move.  Default: allow (matches the v1
/// "reachable ⇒ readable" stance; forges gate uploads here).
///
/// `git_ref` is the optional git ref from the BatchRequest (e.g.
/// `refs/heads/main`) — forges that scope LFS access by branch can use
/// it.  It is advisory: git-lfs clients may omit it.
fn authorize_lfs(&self, _op: LfsOperation, _git_ref: Option<&str>) -> Result<(), String> {
    Ok(())
}
```

`LfsOperation` is `Download | Upload`.  The `Box<T>` forwarding impl in
`traits.rs` gains a matching delegation; `Infallible` inherits the
default.

Flow:

1. **Batch** — `authorize_lfs(op, git_ref)` runs first.  On `Err`, return
   HTTP 403 with the reason; no actions are issued, so no transfer URL is
   ever minted.  This is the gate.
2. **Proxy transfer endpoints** (`GET`/`PUT objects/<oid>`) receive their
   own HTTP request.  The forge constructs a `RepoAccess` from that
   request's credentials (bearer token, session cookie — whatever it uses
   for all other endpoints) and `authorize_lfs` is called again.  There
   is no mizzle-issued token: the forge's existing per-request auth
   covers proxy transfer identically to the way it covers
   `/git-upload-pack`.
3. **Redirect transfer** needs no second check: the presigned URL carries
   its own time-limited credential, issued only after step 1 passed.  Auth
   lives entirely at the batch boundary.

### Upload integrity

- **Proxy mode** hashes the stream on `write` and rejects a mismatch
  against the claimed `oid` (and a size mismatch), so a client cannot
  store garbage under a valid-looking oid.  The `verify` endpoint then
  confirms `stat` returns the expected size.
- **Redirect/presigned mode** — mizzle never sees the bytes, so it
  cannot hash them; it relies on `verify` (existence + size) and S3's
  own integrity headers.  Document this trust boundary: a presigned
  upload trusts the client to write correct bytes for the oid.  Forges
  wanting strong verification either use Proxy mode or run an
  async read-back-and-hash job (out of scope here).

### SSH-delegated auth (`git-lfs-authenticate`)

Over SSH, git-lfs runs `ssh host git-lfs-authenticate <path> <op>` and
expects JSON `{ href, header, expires_in }` pointing at the HTTP LFS
endpoint.  `servers/ssh.rs` already dispatches exec commands
(`git-upload-pack` / `git-receive-pack`); add a `git-lfs-authenticate`
arm that asks the forge (a new `SshAuth` hook) for the HTTP base URL and
a minted token, and writes the JSON to stdout.  This bridges an
SSH-cloned repo to the HTTP transfer API.  Later phase.

## Module layout

```
mizzle-proto/src/lfs.rs          batch JSON, transfer enums, LfsOid, pointer parse
mizzle/src/lfs/
├── mod.rs        LfsStore trait, TransferAction, serve_lfs, serve_with_backends
├── batch.rs      batch API handler (request → actions JSON)
├── transfer.rs   proxy GET/PUT/verify endpoints, hash-on-write
└── fs.rs         FsLfs reference store (standard on-disk layout)
mizzle/src/backend/sql/lfs.rs    SqlLfs (coupled) — lfs_objects table
mizzle/src/backend/kv/lfs.rs     KvLfs (coupled) — ("lfs", repo, oid) keys
mizzle/src/backend/s3_lfs.rs     S3LfsStore (separated) — presigned redirect
```

---

## Phase 0 — `mizzle-proto` LFS types

**Standalone MR, no storage dependency.**

`mizzle-proto/src/lfs.rs`:

- `LfsOid([u8; 32])` with hex parse/display (`sha256:` prefix handling).
- `BatchRequest { operation, transfers: Vec<String>, objects: Vec<{oid, size}>, ref: Option<…> }`.
- `BatchResponse { transfer, objects: Vec<BatchObject> }` where each
  `BatchObject` carries `actions` (`download` / `upload` / `verify`) or an
  `error { code, message }`.
- `Operation { Download, Upload }`.
- `parse_pointer(blob: &[u8]) -> Option<LfsPointer { oid, size }>` and a
  pointer serialiser.

serde structs + a couple of unit tests against the canonical fixtures
from the git-lfs spec.  Nothing else depends on this yet.

## Phase 1 — `LfsStore` trait + auth hook + entry points

- Define `LfsStore` and `TransferAction` in `mizzle/src/lfs/mod.rs`.
- Add `authorize_lfs` (default `Ok`) to `RepoAccess`; update the
  `Box<T>` forwarding impl.
- Add `serve_lfs` and `serve_with_backends` skeletons that route paths
  but return 501 until Phase 2.

No concrete store yet.  `cargo build` green; existing tests unaffected.

## Phase 2 — Batch API + proxy transfer

`batch.rs`:

1. Parse `BatchRequest`; call `authorize_lfs(op, git_ref)`.
2. For each object: `stat`.
   - **download**: present → `download_action`; absent → per-object
     `error { code: 404 }`.
   - **upload**: absent → `upload_action`; present → no action (already
     have it), optionally a `verify` action.
3. For `TransferAction::Proxy`, synthesise the href
   `<lfs-base>/objects/<oid>` (no separate token — the forge's normal
   auth credentials cover the transfer endpoint); for `Redirect`, pass
   `href`/`header`/`expires_at` straight through.
4. Emit `BatchResponse` as `application/vnd.git-lfs+json`.

`transfer.rs` (proxy stores only):

- `GET objects/<oid>` → `stat` then stream `read` into the response.
- `PUT objects/<oid>` → `authorize_lfs(Upload)`, stream body into `write`
  with hash-on-write; reject oid/size mismatch.
- `POST objects/verify` → `stat`, compare size.

## Phase 3 — `FsLfs` reference store (the oracle)

Standard git-lfs on-disk layout
`<root>/<oid[0:2]>/<oid[2:4]>/<oid>`, `Proxy` for both actions.  Pairs
with `FsGitoxide` / `FsGitCli`.  This is the correctness oracle: a real
`git lfs push` / `git lfs pull` round-trips against it (Phase 7).

`write` streams to a temp file in the same directory and renames it into
place on success, so a failed upload never leaves a partial file at the
canonical OID path.  `read` is a plain file open.

## Phase 4 — Coupled stores (`SqlLfs`, `KvLfs`)

Implement `LfsStore` *on the existing backend types*, proving coupling:

- **SQL**: `CREATE TABLE lfs_objects (repo_id, oid BLOB, size INTEGER,
  data BLOB, PRIMARY KEY (repo_id, oid))`.  `open` reuses `SqlRepo`.
  `Proxy`; `read`/`write` are single-row select/insert.
- **KV**: `("lfs", <repo_id>, <oid:32>) → bytes` (under TiKV's per-value
  limit; reuse the FDB chunking module for large blobs).  `Proxy`.

Wiring passes the same backend value for both git and LFS.

## Phase 5 — `S3LfsStore` (separated, redirect mode)

The "S3 for LFS, SQLite for git" deliverable.  `RepoId = PathBuf`,
mapping repo → key prefix `<bucket>/<repo>/lfs/<oid>` (or pure `<oid>` for
cross-repo dedup — see below).

- `stat` → `HeadObject`.
- `download_action` → presigned `GetObject` URL (`Redirect`).
- `upload_action` → presigned `PutObject` URL with conditions:
  `Content-Length` set to the declared size, and a content-checksum
  condition (`x-amz-checksum-sha256` if the bucket has checksum
  enforcement) so S3 rejects undersized or corrupt uploads before they
  are committed.  Document the trust boundary: mizzle never reads the
  uploaded bytes, so `verify` (existence + size via `stat`) is the only
  post-upload integrity check mizzle can perform; strong sha256 verification
  requires either Proxy mode or a separate async read-back.
- `read`/`write` → `Err(LfsError::ProxyNotSupported)` (never invoked in
  redirect mode).

Behind an `s3` cargo feature (`aws-sdk-s3` or `rusty-s3` for
presign-only).  Multi-gigabyte transfers bypass the mizzle process
entirely.

## Phase 6 — SSH `git-lfs-authenticate`

Add the exec arm in `servers/ssh.rs` and an `SshAuth` hook returning the
HTTP LFS base URL + a minted token.  Lets SSH-cloned repos use the HTTP
transfer API.

## Phase 7 — Test integration

Real-client parity, matching the cross-backend harness pattern the SQL
and KV backends established:

1. A `lfs_test!` macro running `git lfs track`, `git add`, `git push`,
   then a fresh `git lfs pull` in a second clone, asserting byte-identical
   round-trip.  Run against each `(git backend, LFS store)` combination.
2. **Coupled** arms: `(SqlBackend, SqlBackend)`, `(KvBackend, KvBackend)`.
3. **Separated** arms: `(SqlBackend, FsLfs)`, `(FsGitoxide, S3LfsStore)`
   (S3 against MinIO in CI, gated on an env var like the TiKV tests).
4. Auth tests: `authorize_lfs(Upload)` rejection → batch 403, no bytes
   stored; proxy `PUT` oid-mismatch rejection.
5. `cargo test` with no LFS features still passes.

---

## Ordering

```
Phase 0 (proto LFS types)              — standalone MR
   │
Phase 1 (LfsStore trait, auth hook, entry points)
   │
Phase 2 (batch API + proxy transfer)
   │
Phase 3 (FsLfs — oracle)               ← begin parity harness here
   │
Phase 4 (coupled SqlLfs / KvLfs)       — demonstrates coupling
   │
Phase 5 (S3LfsStore)                   — demonstrates separation
   │
Phase 6 (SSH git-lfs-authenticate)
   │
Phase 7 (test integration — incremental from Phase 3)
```

Phases 0–3 deliver a working, fully tested HTTP LFS server on the
filesystem.  Phases 4–5 are the coupled/separated demonstrations and can
ship independently once the trait is stable.

## Harvest from existing backends

| Concept | Existing location | LFS reuse |
|---|---|---|
| Async RPITIT trait shape | `backend/mod.rs::StorageBackend` | `LfsStore` mirrors it |
| `RepoId`-coupled `serve` constraint | `serve.rs` `B: StorageBackend<RepoId = A::RepoId>` | add `L: LfsStore<RepoId = A::RepoId>` |
| Path split / dispatch | `servers/axum.rs` `rsplit_once(".git/")` | route `info/lfs/` prefix |
| Defaulted `RepoAccess` hooks | `traits.rs` (`authorize_push` etc.) | `authorize_lfs` follows the same default pattern |
| FDB large-value chunking | `backend/kv/chunked.rs` (planned) | reuse for `KvLfs` big blobs |
| Per-backend test macro | `sql_backend_access_test!` in `tests/common/mod.rs` | sibling `lfs_test!` |
| Streaming response body | `servers/axum.rs` `GitResponse` reader | proxy download reuses it |

## Out of scope (candidates for later)

- **File-locking API** (`info/lfs/locks…`) — mutable, needs CAS like
  refs; a separate small surface, deferrable until a forge needs it.
- **Pure-SSH transfer** (`git-lfs-transfer`, the SSH object protocol) —
  newer, optional; Phase 6 covers only the SSH→HTTP auth bridge.
- **Cross-repo dedup tuning** — content addressing makes global dedup
  free and safe (same oid ⇒ same bytes), but auth stays per-repo.
  Reference stores namespace per repo for simplicity; `S3LfsStore` can
  key purely by `oid` to dedup.  Not a v1 concern.
- **Quarantine-then-promote uploads** — promote on `verify` instead of
  on `PUT`.  The batch-gates-auth model already prevents unauthorised
  writes; promotion semantics are an enhancement.

## Cross-references

- [architecture.md](architecture.md) — layers, `RepoId` coupling
- [auth.md](auth.md) — read-side authz stance, staging-before-commit
- [distributed-backends.md](distributed-backends.md) — the objects-vs-refs
  split LFS slots into
- [kv-backend-plan.md](kv-backend-plan.md) — large-blob / S3-offload note
