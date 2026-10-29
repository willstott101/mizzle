# Architecture

For the high-level pitch see [../README.md](../README.md).

## Layers

```
┌──────────────────────────────────────────────────────────────┐
│  Transport                                                   │
│  HTTP smart protocol v1/v2 · SSH · Git LFS (HTTP)            │
│  Axum integration + generic protocol functions               │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Auth  —  RepoAccess trait (HTTP) · SshAuth trait (SSH)      │
│  Constructed by caller with all permissions pre-resolved.    │
│  Pushes inspected through a lazy, bounded Comparison handle. │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Protocol layer  (mizzle-proto + mizzle serve/fetch/receive) │
│  pkt-line I/O · fetch/push argument parsing · negotiation    │
│  Temporary FS staging of received packs                      │
└──────────┬───────────────────────────────┬───────────────────┘
           │                               │
┌──────────▼──────────────────────┐ ┌──────▼───────────────────┐
│  StorageBackend (git objects,   │ │  LfsStore (LFS blobs)    │
│  refs, graph, packs)            │ │  FsLfs · NoLfs           │
│  FsGitoxide · FsGitCli          │ │                          │
│  SqlBackend (feature "sql")     │ │                          │
│  KvBackend  (feature "tikv")    │ │                          │
└─────────────────────────────────┘ └──────────────────────────┘
```

### Transport

Mizzle is pinned to tokio.  The SSH server (russh, behind the `ssh`
feature) requires it, and every mainstream Rust HTTP framework runs on
it.

The canonical HTTP integration is axum (default `axum` feature):
[`servers::axum::serve_with_backends`](../mizzle/src/servers/axum.rs)
dispatches git smart-HTTP requests to a `StorageBackend` and
`info/lfs/…` requests to an `LfsStore` (pass `NoLfs` for git-only
serving).  The core protocol functions in
[`serve.rs`](../mizzle/src/serve.rs) (`serve_upload_pack`,
`serve_receive_pack`, `serve_git_protocol_1`, `serve_git_protocol_2`)
take generic `AsyncRead`/`AsyncWrite` types and can be called from any
tokio-based framework directly.  See
[`mizzle/examples/axum.rs`](../mizzle/examples/axum.rs) for a complete
server.

Mizzle is a library that can be included into a wider HTTP rust
application. This should offer forge APIs that use git data to directly
access the mizzle backend, rather than proxying/translating API calls.

### Auth — `RepoAccess`

See [`mizzle/src/traits.rs`](../mizzle/src/traits.rs) and
[auth.md](auth.md) for the full design.

**`RepoAccess` construction is where expensive work happens.** The caller
resolves the user, loads permissions, evaluates branch-protection rules, and
stores results in the `RepoAccess` value before handing it to mizzle. Every
subsequent call mizzle makes is a cheap value comparison against
already-loaded data.  Push-time policies that need commit data read it
through the `Comparison` handle, which computes walks, diffs and
signature checks lazily via the storage backend — the authoriser itself
never opens the repository.

This is faster and simpler than hook-callback patterns (Gitea, GitLab)
where the server opens the repository and runs `git rev-list` or makes HTTP
round-trips on every push. In those systems the complexity of auth bleeds
into the hot path. In mizzle it is front-loaded at connection/request time,
invisible to the library.

Received pack data is first staged in a temporary local file.  A push
rejected by `authorize_preliminary` never reaches the backend at all.
A push rejected by `authorize_push` has its ingested pack rolled back
and no refs move — see [Push receive flow](#push-receive-flow) for the
exact ordering.

### Auth — `SshAuth`

SSH authenticates the user before the repository path is known (the path
arrives in the exec request).  All public keys are accepted at the SSH
layer; real auth is deferred to a single `SshAuth::authorize` call that
receives the user, public key, and repository path together.  This is where
expensive work happens — the returned `RepoAccess` must be cheap to
interrogate thereafter.

See [dos-protection.md](dos-protection.md) for the exec timeout that
mitigates the accept-all-keys approach.

### Auth–storage coupling

`RepoAccess` is not a pure auth object — it is a *resolved request
context*: by the time mizzle holds one, the caller has already looked up the
user, identified the repository, and loaded all permission state. The repo
identifier therefore belongs on `RepoAccess`, not on the storage backend.

The identifier is an associated type so that auth and storage stay
orthogonal while the type system enforces they speak the same language:

```rust
trait RepoAccess {
    type RepoId;
    type PushContext: Default;
    fn repo_id(&self) -> &Self::RepoId;
    // authorize_preliminary, authorize_push, post_receive, auto_init,
    // authorize_lfs …
}

trait StorageBackend {
    type RepoId;
    type Repo;          // per-request handle from `open`
    type IngestedPack;  // handle for rollback on auth failure
    fn open(&self, id: &Self::RepoId) -> impl Future<Output = Result<Self::Repo>> + Send;
    fn list_refs(&self, repo: &Self::Repo) -> impl Future<…> + Send;
    // resolve_ref, update_refs, init_repo, has_object(s),
    // compute_push_kind, build_pack, ingest_pack, inspect_ingested,
    // rollback_ingest, reachable_excluding, tree_diff,
    // read_commit_info, read_blob, read_object_raw …
}
```

The serve entry-points carry the constraints
`B: StorageBackend<RepoId = A::RepoId>` and
`L: LfsStore<RepoId = A::RepoId>`. Pairing a mismatched auth and
backend is a compile error.

`RepoAccess` is constructed per-request by the caller. The storage backend
is a shared singleton (connection pool, cluster client, etc.). The id
flows from the per-request auth object into the shared backend's `open`,
which returns a per-request `Repo` handle used for every subsequent call.

All the shipped backends use `RepoId = PathBuf`; for SQL and KV the path
is just a lookup key.  A forge-specific backend could use a
`(OwnerId, RepoId)` pair or a UUID.

### Protocol layer

The protocol layer parses requests (via `mizzle-proto`), drives
negotiation, and calls the storage backend for everything that touches
repository data:

- **Fetch**: ref listing, have/want negotiation, then
  `StorageBackend::build_pack`, which is responsible for graph traversal,
  partial clone filters and shallow depth limiting.  The resulting pack
  is streamed to the client over side-band.
- **Push**: parses ref updates, runs preliminary auth, stages the pack
  to a temp file, and hands it to the backend to ingest, inspect and
  classify (create / delete / fast-forward / force-push).  Auth is
  called with the classified `PushKind` values and the `Comparison`
  handle.  Refs are only updated after auth passes.

Each backend implements push classification and graph walks with its
own machinery: gitoxide for `FsGitoxide`, `git merge-base` /
`git rev-list` for `FsGitCli`, recursive CTEs for `SqlBackend`, and
in-process BFS over `par/` keys for `KvBackend`.  Pack-metadata
extraction for auth (`inspect.rs`) and signed-payload reconstruction
(`sigverify.rs`) are shared, gitoxide-based code.

### Storage backends

One trait, [`StorageBackend`](../mizzle/src/backend/mod.rs), is
implemented by every backend.  All methods are async
(`-> impl Future<…> + Send`).  The filesystem backends mostly do their
work synchronously on the calling task and return a ready future; only
pack inspection and rollback go through `spawn_blocking`.

| Backend | Feature | Objects / refs | Notes |
|---|---|---|---|
| `FsGitoxide` | — | bare repo via gitoxide | Production FS backend; bitmap have-set and whole-pack reuse fast paths |
| `FsGitCli` | — | bare repo via `git` subprocesses | Correctness oracle |
| `SqlBackend` | `sql` | SQLite via sqlx | See [sql-backend-plan.md](sql-backend-plan.md) |
| `KvBackend` | `tikv` | TiKV transactions | See [kv-backend-plan.md](kv-backend-plan.md) |

SQL and KV build packs by materialising the objects into a temporary
gitoxide repo and share an on-disk pack cache
([`backend/pack_cache.rs`](../mizzle/src/backend/pack_cache.rs)).

A `BypassBackend` marker trait exists in `backend/mod.rs` for a
hypothetical backend that handles the whole protocol itself.  Nothing
implements it; `FsGitCli` uses the ordinary `StorageBackend` trait.

Git LFS is a separate trait, [`LfsStore`](../mizzle/src/lfs/mod.rs),
joined to the rest only by `RepoId` — see
[lfs-backend-plan.md](lfs-backend-plan.md).

---

## `mizzle-proto` crate

Protocol primitives extracted into a standalone crate with no storage
dependency:

- capability-advertisement and command parsing
- fetch (v1 and v2), ls-refs and receive-pack request parsing
- filter and shallow handling utilities
- `ProtocolLimits` caps on request sizes
- Git LFS batch API types, `LfsOid`, and pointer-blob parsing

pkt-line encoding/decoding itself comes from `gix-packetline`.

---

## Push receive flow

```
1. Client sends ref update headers (old OID, new OID, refname) + pack data

2. Preliminary auth (authorize_preliminary) on ref names and a cheap
   PushKind guess — can reject before any pack data is read

3. Pack streamed into a temporary local file

4. Backend ingest_pack — the pack becomes readable by the backend
   (FS backends: indexed into objects/pack; SQL/KV: objects inserted)
   so that the Comparison handle can walk it

5. inspect_ingested extracts commit/tag metadata; failure rejects the
   push.  compute_push_kind classifies each ref (Create / Delete /
   FastForward / ForcePush)

6. Full auth: authorize_push(PushContext, &dyn Comparison)
   On Err: rollback_ingest, `ng` for every ref, no refs move

7. update_refs — one CAS transaction for all refs

8. post_receive() callback (CI triggers, notifications, audit log)
```

Step 2 costs nothing — just a check against data already in
`RepoAccess` — and catches obvious rejections before any pack data is
read.

Rollback after step 6 differs by backend: the filesystem backends
unlink the pack they installed, while `SqlBackend` and `KvBackend`
leave the inserted objects in place as unreferenced orphans.  In every
case no ref points at a rejected push's objects.

---

## Backend comparison

The two filesystem backends exist specifically to measure and validate:

| | `FsGitoxide` | `FsGitCli` |
|---|---|---|
| **Object reads** | gitoxide | `git cat-file` |
| **Push-kind classification** | gitoxide | `git merge-base --is-ancestor` |
| **Pack building** | gitoxide | `git pack-objects` |
| **Pack indexing** | gitoxide | `git index-pack` |
| **Ref updates** | gitoxide | `git update-ref --stdin` |
| **Purpose** | production use, no CLI dependency | correctness baseline |

The SQL and KV backend comparison is more significant: object reads,
graph traversal for fetch negotiation, and ref listing are all against
the database.  The same integration tests run against them
(`backend_parity.rs`, `sql_backend_access_test!`, and the TiKV CI job).
