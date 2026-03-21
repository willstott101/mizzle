# Roadmap

## Implementation phases

### Phase 1 — Extract `mizzle-proto` ✓

Pkt-line, capability parsing, filter/shallow utilities in a standalone
crate with no storage dependency.

### Phase 2 — SSH transport ✓

russh-based SSH server with deferred auth (`SshAuth` trait).

### Phase 3 — Runtime consolidation ✓

Commit to tokio.  Remove trillium/actix/rocket adapters, keep axum as
canonical HTTP integration.  See [runtime-consolidation.md](runtime-consolidation.md).

### Phase 4 — Define storage traits ✓

Audit all gitoxide calls in `fetch.rs`, `pack.rs`, `ls_refs.rs`,
`receive.rs`. Define the thin storage trait and the full-bypass backend
trait. Move the current gitoxide implementation behind the thin storage
trait as `FsGitoxide`. Existing tests must all pass unchanged.

The trait shape is the most consequential design decision in the project.
Worth prototyping on paper before writing code — particularly streaming
pack data (must not buffer), async graph traversal, and atomic
receive-pack (write + ref update).

### Phase 5 — `FsGitCli` backend

Full-bypass backend that hands off to git CLI after auth. Use as the
correctness oracle: run the integration tests against both `FsGitoxide`
and `FsGitCli` and verify identical behaviour.

### Phase 6 — Cross-backend test harness

Parameterise the integration tests over backends. Add benchmarks.
After this phase every subsequent backend gets full coverage for free.

### Phase 7 — Fuzzing

libfuzzer/AFL harness over the protocol parsing layer. Seed corpus from
traffic captures. Run against a minimal in-memory stub.

### Phase 8 — SQL backend (PoC)

SQLite first, then Postgres. Schema:
- `objects(repo, oid, type, data)`
- `refs(repo, name, oid)`
- `commit_parents(repo, commit_oid, parent_oid)` — materialised for
  graph traversal

The cross-backend harness from Phase 6 immediately validates correctness
and surfaces performance characteristics.

---

## Testing strategy

**Cross-backend integration harness** — the same test suite runs against
every backend. Tests make real `git clone`, `git fetch`, `git push` calls
against a live mizzle server backed by each backend in turn. Correctness
parity is verified; timing is recorded for performance comparison.

**Fuzzing** — the protocol parsing layer (pkt-line, fetch args, push
headers) is fuzzed against a minimal in-memory backend stub, independent
of storage.
