# Next steps

## 1. Extract `mizzle-proto` (Phase 1)

Move pkt-line, capability parsing, filter/shallow utilities into a standalone crate with no storage dependency. Mechanical refactor — no behaviour change. Do this first so the storage trait boundary is drawn against a clean protocol layer.

## 2. Plan storage trait shape (on paper)

Resolve the three hard sub-problems before writing any code:

- **Streaming `write_objects`**: currently buffers the whole pack into `Vec<u8>` (`receive.rs:65`). Needs to accept a stream for non-filesystem backends.
- **Async graph traversal**: `compute_push_kind` uses `gix::objs::Find` which is sync. SQL backends are async. Decide: block, pre-cache objects from the pack, or restructure as async-native.
- **Staging/atomicity**: FS stages in a temp dir and moves on auth success. SQL can write objects then withhold `update_refs`. Distributed KV may use a transaction. The trait contract needs to express this clearly.

## 3. Define storage traits (Phase 2)

`RepoAccess` is a resolved request context (user + repo + permissions), not a pure auth object — repo identification belongs on it. Replace `repo_path() -> &str` with an associated type:

```rust
trait RepoAccess {
    type RepoHandle;
    fn repo_handle(&self) -> &Self::RepoHandle;
}

trait StorageBackend {
    type RepoHandle;
    // list_refs, read_object, has_object, write_objects, update_refs, init
}
```

Serve entry-point requires `B: StorageBackend<RepoHandle = A::RepoHandle>`.

Move the current gitoxide implementation behind the thin storage trait as `FsGitoxide` with `RepoHandle = PathBuf`. All existing tests must pass unchanged.
