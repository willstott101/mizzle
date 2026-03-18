# mizzle

Most software forges couple git hosting tightly to a specific storage model — bare repositories on a filesystem, managed by shelling out to git CLI plumbing. This works fine at small scale but creates hard limits: you can't query history in SQL, you can't shard repos across object storage, you can't run multiple writers without careful locking, and you can't swap in a faster implementation without rewriting the whole server.

**mizzle** is a Rust library for building git servers with clean boundaries between the wire protocol, authentication, and storage. Its goal is to make it possible to explore entirely new ways of hosting git — and to make comparing them easy.

## Why this might matter

Git's HTTP smart protocol is a well-defined wire format. The server's job is: advertise refs, negotiate a common ancestor, stream a packfile. Nothing in that contract requires bare repos on disk. By separating the protocol layer from the storage layer you can implement storage any way you like — SQL, object storage, a pure-Rust git library, or the traditional git CLI — and serve it all over the same protocol that every git client already speaks.

This unlocks things that are difficult or impossible with forge software today:

- **Horizontal scaling.** Storage backends backed by distributed databases or object storage can serve fetch requests from multiple nodes without shared filesystem state.
- **Rich querying.** Storing the object graph in a relational database means you can query commit history, file ownership, and ref state with SQL — the same data that powers code review, blame, and search features — without separate indexing pipelines.
- **Correctness and performance testing.** The same integration test suite and fuzz corpus runs against every backend. You can verify that a novel backend is protocol-correct, and measure whether it's faster or slower than the baseline.
- **Novel hosting models.** Serverless git, WASM-hosted git, git over a custom transport — all are possible when storage and protocol are decoupled.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    Transport                        │
│         (HTTP smart protocol v1/v2, SSH, git://)    │
└─────────────────────┬───────────────────────────────┘
                      │
┌─────────────────────▼───────────────────────────────┐
│                  Auth layer                         │
│   pluggable: OIDC, SSH key, token, anonymous, ...   │
└─────────────────────┬───────────────────────────────┘
                      │
┌─────────────────────▼───────────────────────────────┐
│               Repository trait                      │
│   list_refs · fetch_pack · receive_pack · hooks     │
└──────────────────┬──────────────┬───────────────────┘
                   │              │
      ┌────────────▼────────┐  ┌──▼──────────────────────┐
      │   Filesystem / NFS  │  │  Novel backends         │
      │  (bare git repos)   │  │  (SQL, object store...) │
      └─────────────────────┘  └─────────────────────────┘
```

The `Transport` layer speaks to any HTTP server framework (Axum, Actix, etc.) via a thin integration crate — mizzle is not coupled to a particular async runtime or web framework.

The `Auth` layer receives structured information about the operation being requested (repo path, ref name, push kind) and returns a decision. It never opens the repository.

The `Repository` trait is what backends implement. The same test suite runs against every backend, so correctness and performance can be compared directly.

### Filesystem backends

The filesystem backend has two implementations, both shipping with mizzle, primarily for validation and experimentation:

| Implementation | How it works | Use case |
|---|---|---|
| `FsGitCli` | mizzle handles auth and protocol; git CLI handles pack indexing and ref updates | Ground-truth correctness and performance baseline |
| `FsGitoxide` | Pure Rust via [gitoxide](https://github.com/Byron/gitoxide) for the full stack | Performance comparison, no git CLI dependency |

For most users these are interchangeable — both serve bare repos on a local or networked filesystem. Having both makes it easy to catch divergence and to benchmark pack indexing and ref update performance between the two.

Note: push-kind classification (fast-forward vs. force-push) always uses gitoxide regardless of backend, because the received pack must be inspected before mizzle can call auth. The CLI backend hands off to `git` only after auth has passed.

## Design rules

**Construction is where expensive auth work happens.**
`RepoAccess` is constructed by your code before being handed to mizzle. By that point your code has already resolved the user, loaded permissions, and evaluated any branch-protection rules. Every call mizzle makes into `RepoAccess` after that must be a cheap value comparison — no database queries, no HTTP calls, no file I/O. This is what makes mizzle's auth model faster and simpler than hook-callback approaches used by traditional forges.

**Authorisers must never open the repository.**
`RepoAccess::authorize_push` receives all the information needed to make an authorisation decision as plain values — repo path, ref name, and a `PushKind` enum that encodes create/delete/fast-forward/force. Branch-protection rules, glob patterns, team membership — all of that lives in your `RepoAccess` impl, resolved at construction time. If an authoriser needs to inspect the object graph it is a bug in mizzle's callback interface, not in the authoriser.

## Status

Protocol support (gitoxide backend, HTTP transport):

- [x] Shallow clone (`--depth N`) — essential for CI/CD workloads
- [x] Protocol v1 support — compatibility with older git clients and tooling that doesn't send `Git-Protocol: version=2`
- [x] Fetch negotiation — proper ACK/NAK handling so incremental fetches send minimal packs rather than always recomputing from scratch
- [x] Server-side hooks — `post_receive` callback on `RepoAccess` called after refs are updated
- [x] Repository auto-init — `auto_init()` on `RepoAccess`; mizzle calls `git init --bare` on first push if the path doesn't exist
- [x] Partial clone filters (`--filter=blob:none`, `--filter=tree:0`)
- [x] Ref-in-want
- [x] `wait-for-done`

Planned:

- [ ] SSH transport
- [ ] git:// transport
- [ ] FS + git CLI backend (baseline for correctness comparison)
- [ ] SQL backend (proof of concept)
- [ ] Cross-backend integration test harness
- [ ] Fuzzing against the protocol layer
- [ ] API Layer for non-git repo interaction

## Notes on the git protocol spec

Some underdocumented protocol behaviours discovered during implementation:

The capability advertisement response begins:

    S: 200 OK
    S: <headers>
    S:
    S: 000eversion 2\n
    S: <capability-advertisement>

The spec then says `<capability-advertisement>` contains `000eversion 2\n` — this is misleading; the version line is part of the preamble, not repeated inside the advertisement body.

Not all fetch arguments document that multiple entries of the same argument type can be specified in a single request.
