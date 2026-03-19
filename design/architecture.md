# Architecture

For the high-level pitch see [../README.md](../README.md).

## Layers

```
┌──────────────────────────────────────────────────────────────┐
│  Transport                                                    │
│  HTTP smart protocol v1/v2 · SSH                             │
│  Axum integration + generic protocol functions               │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Auth  —  RepoAccess trait (HTTP) · SshAuth trait (SSH)       │
│  Constructed by caller with all permissions pre-resolved.     │
│  All calls into it are pure value comparisons.               │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Protocol layer  —  always gitoxide                          │
│  Pack negotiation · push-kind classification · pkt-line I/O  │
│  Temporary FS staging of received packs                      │
└──────────┬───────────────────────────────┬───────────────────┘
           │                               │
┌──────────▼──────────┐        ┌───────────▼──────────────────┐
│  Thin storage trait │        │  Full-bypass backend trait   │
│  FsGitoxide         │        │  FsGitCli                    │
│  SqlBackend         │        │  (hands off after auth)      │
└─────────────────────┘        └──────────────────────────────┘
```

### Transport

Mizzle is pinned to tokio.  The SSH server (russh) requires it, and every
HTTP framework in the ecosystem uses it (axum, actix, rocket all run on
tokio).

The canonical HTTP integration is axum.  The core protocol functions
(`serve_upload_pack`, `serve_receive_pack`, `serve_git_protocol_1`,
`serve_git_protocol_2`) take generic `AsyncRead`/`AsyncWrite` types and
can be called from any tokio-based framework directly.
See [runtime-consolidation.md](runtime-consolidation.md) for the rationale.

The reason mizzle provides HTTP integration at all (rather than being a
standalone proxy) is so that forges can serve git and their own web API
from the same binary.  A forge that also exposes a REST API for browsing
files, viewing diffs, or querying commit history can embed mizzle as a
library alongside those routes — avoiding the overhead of proxying git
traffic between two processes.

### Auth — `RepoAccess`

See [`mizzle/src/traits.rs`](../mizzle/src/traits.rs).

**`RepoAccess` construction is where expensive work happens.** The caller
resolves the user, loads permissions, evaluates branch-protection rules, and
stores results in the `RepoAccess` value before handing it to mizzle. Every
subsequent call mizzle makes is a cheap value comparison against
already-loaded data.

This is faster and simpler than hook-callback patterns (Gitea, GitLab)
where the server opens the repository and runs `git rev-list` or makes HTTP
round-trips on every push. In those systems the complexity of auth bleeds
into the hot path. In mizzle it is front-loaded at connection/request time,
invisible to the library.

Unlike Gitea and GitLab, which accept and index the full pack before running
auth in a `pre-receive` hook, mizzle stages received pack data in temporary
local storage and does not write it into the repository until auth has
passed. An unauthorised push leaves no trace in the object store.

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
    type RepoHandle;
    fn repo_handle(&self) -> &Self::RepoHandle;
    // authorize_push, post_receive, auto_init …
}

trait StorageBackend {
    type RepoHandle;
    fn list_refs(&self, repo: &Self::RepoHandle) -> …;
    // read_object, write_objects, update_refs …
}
```

The serve entry-point carries the constraint
`B: StorageBackend<RepoHandle = A::RepoHandle>`. Pairing a mismatched auth
and backend is a compile error.

`RepoAccess` is constructed per-request by the caller. The storage backend
is a shared singleton (connection pool, cluster client, etc.). The handle
flows from the per-request auth object into the shared backend's methods.

For filesystem backends `RepoHandle = PathBuf`. For a SQL backend
`RepoHandle` might be a `(OwnerId, RepoId)` pair or a UUID.

### Protocol layer

gitoxide is always in the middle:

- **Fetch**: ref listing, commit graph traversal for have/want negotiation,
  pack construction, partial clone filters, shallow clone depth limiting.
- **Push**: receives the pack into temporary local storage, inflates thin
  packs, classifies each ref update as create/delete/fast-forward/force-push
  via graph traversal. Auth is called with the classified `PushKind` values.
  Only after auth passes does control move to the storage backend.

Push-kind classification always uses gitoxide regardless of which storage
backend is in use, because the received objects must be locally available
before the graph can be walked.

### Storage backends

Two trait levels, for two kinds of backend:

**Thin storage trait** — for backends where gitoxide does the protocol work
and the backend just stores and retrieves objects and refs:
- `list_refs(repo)`
- `read_object(repo, oid)`
- `has_object(repo, oid)`
- `write_objects(repo, iter)`
- `update_refs(repo, updates)`
- `init(repo)`

**Full-bypass backend trait** — for backends that want to handle the storage
step themselves after auth passes. `FsGitCli` is the only planned
full-bypass backend.

---

## `mizzle-proto` crate

Protocol primitives extracted into a standalone crate with no storage
dependency:

- pkt-line encoding/decoding
- capability advertisement parsing
- fetch/push argument parsing
- filter and shallow handling utilities

This lets full-bypass backends reuse the fiddly protocol parts without
pulling in the rest of mizzle.

---

## Push receive flow

```
1. Client sends ref update headers (old OID, new OID, refname) + pack data

2. Preliminary auth on ref names only — can reject before any pack is
   received (e.g. pushing to a ref the user has no access to at all)

3. Pack received into temporary local storage — never written to the
   repository until auth has passed

4. gitoxide walks the object graph to classify each ref (Create / Delete /
   FastForward / ForcePush), resolving thin-pack deltas against the local
   repo on demand

5. Full auth: authorize_push(refs with PushKind) — cheap value comparison

6a. FsGitoxide: gitoxide indexes pack, updates refs
6b. FsGitCli:   pipe pack + ref updates to `git receive-pack`
6c. SqlBackend: gitoxide explodes pack, write_objects() + update_refs()

7. post_receive() callback (CI triggers, notifications, audit log)
```

Step 2 (preliminary auth on ref names) costs nothing — just a check against
data already in `RepoAccess` — and catches obvious rejections before any
pack data is transferred.

---

## Backend comparison

The two filesystem backends exist specifically to measure and validate:

| | `FsGitoxide` | `FsGitCli` |
|---|---|---|
| **Object reads** | gitoxide | gitoxide |
| **Push-kind classification** | gitoxide | gitoxide |
| **Pack indexing** | gitoxide | `git index-pack` |
| **Ref updates** | gitoxide | `git update-ref` |
| **Purpose** | production use, no CLI dependency | correctness baseline |

The SQL backend comparison is more significant: object reads, graph
traversal for fetch negotiation, and ref listing are all against the
database.
