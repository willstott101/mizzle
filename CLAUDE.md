Use `cargo fmt` before committing.

## Hard architecture rules

- Auth never opens the repository. `RepoAccess` must resolve everything at construction time. If an authoriser needs to inspect the object graph, that is a bug in the callback interface.
- Received pack data is staged in temporary local storage and never written into the repository until auth has passed.
- Never shell out to `git` in the protocol layer or auth layer. Only `FsGitCli` is allowed to do that, and only after auth passes.
- `mizzle-proto` must have no storage dependency. Do not pull storage traits or backends into it.