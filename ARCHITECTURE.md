# Architecture

This document covers the reasoning behind mizzle's design, the planned layer structure, and the implementation roadmap. For the high-level pitch see [README.md](README.md).

---

## Layers

```
┌──────────────────────────────────────────────────────────────┐
│  Transport                                                    │
│  HTTP smart protocol v1/v2 · SSH                             │
│  Thin per-framework integration crates (Axum, Actix, ...)    │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Auth  —  RepoAccess trait                                    │
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

Thin integration crates map each web framework's request/response types to mizzle's internal `GitRequest` / `GitResponse` types. No framework is required — you can drive mizzle from a raw TCP stream if you want.

Currently implemented: Axum, Actix-web, Rocket, Trillium.
Planned: SSH (russh).

### Auth — `RepoAccess`

See [`mizzle/src/traits.rs`](mizzle/src/traits.rs) and the design rules in the README.

The key point: **`RepoAccess` construction is where expensive work happens.** The caller resolves the user, loads permissions, evaluates branch-protection rules, and stores results in the `RepoAccess` value before handing it to mizzle. Every subsequent call mizzle makes is a cheap value comparison against already-loaded data.

This is faster and simpler than hook-callback patterns (Gitea, GitLab) where the server opens the repository and runs `git rev-list` or makes HTTP round-trips on every push. In those systems the complexity of auth bleeds into the hot path. In mizzle it is front-loaded at connection/request time, invisible to the library.

Unlike Gitea and GitLab, which accept and index the full pack before running auth in a `pre-receive` hook, mizzle stages received pack data in temporary local storage and does not write it into the repository until auth has passed. An unauthorised push leaves no trace in the object store.

### Protocol layer

gitoxide is always in the middle:

- **Fetch**: ref listing, commit graph traversal for have/want negotiation, pack construction, partial clone filters, shallow clone depth limiting.
- **Push**: receives the pack into temporary local storage, inflates thin packs, classifies each ref update as create/delete/fast-forward/force-push via graph traversal. Auth is called with the classified `PushKind` values. Only after auth passes does control move to the storage backend.

Push-kind classification always uses gitoxide regardless of which storage backend is in use, because the received objects must be locally available before the graph can be walked. This means `FsGitCli` still uses gitoxide for classification — it hands off to the git CLI only for the authoritative pack indexing and ref update step.

### Auth–storage coupling

`RepoAccess` is not a pure auth object — it is a *resolved request context*: by the time mizzle holds one, the caller has already looked up the user, identified the repository, and loaded all permission state. The repo identifier therefore belongs on `RepoAccess`, not on the storage backend.

The identifier is an associated type so that auth and storage stay orthogonal while the type system enforces they speak the same language:

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

The serve entry-point carries the constraint `B: StorageBackend<RepoHandle = A::RepoHandle>`. Pairing a mismatched auth and backend is a compile error.

`RepoAccess` is constructed per-request by the caller. The storage backend is a shared singleton (connection pool, cluster client, etc.). The handle simply flows from the per-request auth object into the shared backend's methods — the asymmetry is intentional and expected.

For filesystem backends `RepoHandle = PathBuf` and the change is trivial. For a SQL backend `RepoHandle` might be a `(OwnerId, RepoId)` pair or a UUID — whatever the database uses as its primary key. Auth resolves the HTTP path and credentials into that value; every storage method receives it.

### Storage backends

Two trait levels, for two kinds of backend:

**Thin storage trait** — for backends where gitoxide does the protocol work and the backend just stores and retrieves objects and refs:
- `list_refs(repo)`
- `read_object(repo, oid)`
- `has_object(repo, oid)`
- `write_objects(repo, iter)`
- `update_refs(repo, updates)`
- `init(repo)`

**Full-bypass backend trait** — for backends that want to handle the storage step themselves after auth passes. These can call into `mizzle-proto` (see below) for any protocol primitives they want to reuse, but are not forced through the gitoxide protocol layer.

`FsGitCli` is the only planned full-bypass backend. After mizzle's protocol layer has classified push kinds and auth has passed, it pipes the pack and ref updates directly to `git receive-pack`.

---

## `mizzle-proto` crate

Protocol primitives extracted into a standalone crate with no storage dependency:

- pkt-line encoding/decoding
- capability advertisement parsing
- fetch/push argument parsing
- filter and shallow handling utilities

This lets full-bypass backends reuse the fiddly protocol parts without pulling in the rest of mizzle.

---

## Backend comparison

The two filesystem backends exist specifically to measure and validate:

| | `FsGitoxide` | `FsGitCli` |
|---|---|---|
| **Object reads** | gitoxide | gitoxide |
| **Push-kind classification** | gitoxide | gitoxide |
| **Pack indexing** | gitoxide | `git index-pack` |
| **Ref updates** | gitoxide | `git update-ref` |
| **Auth hooks** | none needed | none needed |
| **Purpose** | production use, no CLI dependency | correctness baseline, perf comparison |

The interesting comparison is pack indexing and ref update performance. Push-kind classification is identical in both, so the auth path is not a variable.

The SQL backend comparison is more significant: object reads, graph traversal for fetch negotiation, and ref listing are all against the database. This is where novel hosting properties (horizontal reads, SQL queries over history) live or die.

---

## Push receive flow

```
1. Client sends ref update headers (old OID, new OID, refname) + pack data

2. Preliminary auth on ref names only — can reject before any pack is received
   (e.g. pushing to a ref the user has no access to at all)

3. Pack received into temporary local storage — never written to the repository
   until auth has passed

4. gitoxide walks the object graph to classify each ref (Create / Delete /
   FastForward / ForcePush), resolving thin-pack deltas against the local
   repo on demand as the walk requires them

5. Full auth: authorize_push(refs with PushKind) — cheap value comparison

6a. FsGitoxide: gitoxide indexes pack, updates refs
6b. FsGitCli:   pipe pack + ref updates to `git receive-pack`
6c. SqlBackend: gitoxide explodes pack, write_objects() + update_refs() to DB

7. post_receive() callback (CI triggers, notifications, audit log)
```

Step 2 (preliminary auth on ref names) is always performed regardless of backend. It costs nothing — just a check against data already in `RepoAccess` — and catches obvious rejections before any pack data is transferred.

---

## Testing strategy

**Cross-backend integration harness** — the same test suite runs against every backend. Tests make real `git clone`, `git fetch`, `git push` calls against a live mizzle server backed by each backend in turn. Correctness parity is verified; timing is recorded for performance comparison.

This follows the same pattern already used for web framework integrations (`test_with_servers!` macro) extended to also parameterise over backends.

**Fuzzing** — the protocol parsing layer (pkt-line, fetch args, push headers) is fuzzed with a corpus built from traffic captured by the sniffer. Run against a minimal in-memory backend stub, independent of storage.

---

## Implementation phases

### Phase 1 — Extract `mizzle-proto`

Move pkt-line, capability parsing, filter/shallow utilities into a standalone crate with no storage dependency. No behaviour change — this is a reorganisation that unblocks everything else.

### Phase 2 — Define storage traits

Audit all gitoxide calls in `fetch.rs`, `pack.rs`, `ls_refs.rs`, `receive.rs`. Define the thin storage trait and the full-bypass backend trait. Move the current gitoxide implementation behind the thin storage trait as `FsGitoxide`. Existing tests must all pass unchanged.

The trait shape is the most consequential design decision in the project. Worth prototyping on paper before writing code — particularly streaming pack data (must not buffer), async graph traversal, and atomic receive-pack (write + ref update).

### Phase 3 — `FsGitCli` backend

Implement the full-bypass backend trait by handing off to git CLI after auth. Use as the correctness oracle: run the integration tests against both `FsGitoxide` and `FsGitCli` and verify identical behaviour.

### Phase 4 — Cross-backend test harness

Parameterise the integration tests over backends. Add benchmarks. Wire the sniffer corpus into replay tests. After this phase every subsequent backend gets full coverage for free.

### Phase 5 — Fuzzing

libfuzzer/AFL harness over the protocol parsing layer. Seed corpus from sniffer captures. Run against a minimal in-memory `Repository` stub.

### Phase 6 — SQL backend (PoC)

SQLite first, then Postgres. Schema:
- `objects(repo, oid, type, data)`
- `refs(repo, name, oid)`
- `commit_parents(repo, commit_oid, parent_oid)` — materialised for graph traversal

The cross-backend harness from Phase 4 immediately validates correctness and surfaces performance characteristics.

### Phase 7 — SSH transport

russh. Auth trait extension needed for SSH key fingerprints vs. HTTP tokens.

---

## Distributed backend candidates

The thin storage trait has two distinct access patterns worth separating when targeting distributed systems:

- **Objects** (`read_object`, `has_object`, `write_objects`): immutable, content-addressed — any CAS or blob store works.
- **Refs** (`update_refs`): mutable pointers requiring compare-and-swap — the hard constraint that rules out eventually-consistent stores.

The candidates below are evaluated against both.

### FoundationDB

Strictly serializable ACID transactions, used by Apple at iCloud scale. Objects stored as immutable KV pairs (`oid → serialized object`). Refs use FoundationDB's transactional CAS — `update_refs` becomes a transaction that reads the current ref value, validates the old OID, and atomically sets the new one. Racing pushes cause one transaction to abort; the client receives a clean rejection. No external lock manager needed — the transaction layer is the lock.

[Tigris](https://www.tigrisdata.com/) is an S3-compatible object store built on FoundationDB and would expose the same strong consistency with a familiar API.

### Cloudflare Durable Objects + R2

The most direct path to the "serverless git" model mentioned in the README. Each repo becomes a Durable Object — single-actor serialized execution means ref updates are implicitly serialized without any explicit locking. R2 (or any blob store) holds the objects. Durable Objects have strong consistency and global availability with edge-local latency for reads.

This maps onto the thin storage trait cleanly: one Durable Object per repo handles `list_refs` and `update_refs`; R2 handles the object side.

### Google Spanner

TrueTime gives external consistency — push ordering is globally agreed upon across datacenters without clock skew ambiguity. For the SQL backend schema (`objects`, `refs`, `commit_parents`), Spanner means the commit graph and ref state are globally consistent with serializable transactions. A ref update is a Spanner mutation with read-your-writes guarantees across regions. Expensive to operate but architecturally the most correct option if linearizability across regions is a hard requirement.

### CockroachDB / YugabyteDB

Distributed PostgreSQL that lands directly on Phase 6's schema. The `commit_parents` graph traversal for fetch negotiation becomes a distributed recursive CTE. Geographic read replicas serve fast fetches; the primary region handles pushes. `update_refs` is a `SELECT ... FOR UPDATE` + `UPDATE` in a serializable transaction — standard SQL, no special APIs. The cross-backend harness from Phase 4 would validate a CockroachDB backend for free.

### etcd (refs) + object storage (objects)

Architecturally clean because it mirrors git's own model. etcd is purpose-built for distributed locking and CAS — its native `txn` operation does compare-and-swap with watches, mapping directly to git's old-OID/new-OID ref update semantics. Only refs live in etcd (tiny data volume); objects go to any blob store. etcd's watch API also gives free ref-change event streaming for CI trigger hooks without a separate pubsub system.

### NATS JetStream KV

Lighter operational footprint than the above. JetStream KV provides CAS via revision-based `Update(key, value, revision)` — optimistic locking that maps directly to git's ref update semantics. JetStream is also a streaming system, which aligns with pack data: in principle the object store and pack streaming layer could share the same infrastructure.

### TiKV

Exposes two APIs over the same cluster: a raw KV API (high throughput, no transactions) and a transactional API (Percolator-based). Objects go via raw KV. Refs go via the transactional API with CAS semantics. One cluster, two access patterns, no operational split between an object store and a ref store.

---

### Locking model shared by the ACID options

FoundationDB, Spanner, CockroachDB, and TiKV all handle concurrent pushes the same way at the storage level: `update_refs` is a serializable transaction that reads current ref values, validates old OIDs against the incoming push, and commits or aborts. If two clients push to the same ref simultaneously, one transaction wins and the other gets a conflict error, which mizzle surfaces as a standard ref update rejection. This matches git's own semantics and requires no additional lock infrastructure in mizzle itself.
