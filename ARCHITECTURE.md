# Architecture

This document covers the reasoning behind mizzle's design, the planned layer structure, and the implementation roadmap. For the high-level pitch see [README.md](README.md).

---

## Layers

```
┌──────────────────────────────────────────────────────────────┐
│  Transport                                                    │
│  HTTP smart protocol v1/v2 · SSH · git://                    │
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
Planned: SSH (russh), git://.

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

### Storage backends

Two trait levels, for two kinds of backend:

**Thin storage trait** — for backends where gitoxide does the protocol work and the backend just stores and retrieves objects and refs:
- `list_refs()`
- `read_object(oid)`
- `has_object(oid)`
- `write_objects(iter)`
- `update_refs(updates)`
- `init()`

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

### Phase 8 — git:// transport

Simpler than SSH (no encryption, no auth). Useful for local/trusted network scenarios.
