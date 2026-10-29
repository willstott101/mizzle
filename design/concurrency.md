# Concurrency

How concurrent reads and writes interact in mizzle, and which of those
interactions are safe under the locking guarantees gitoxide actually
provides.

This document is specific to the filesystem backends.  The SQL and KV
backends get their concurrency story from the transactional guarantees
of the underlying store (see [SQL and KV backends](#sql-and-kv-backends)
at the end).

## Where concurrency comes from

Mizzle is a library and is hosted inside a tokio runtime owned by the
caller.  Each git request — clone, fetch, ls-refs, push — is handled by an
independent task on the runtime.  There is no per-repository serialisation
at the mizzle layer: a fetch and a push to the same repo can be in flight
simultaneously, two fetches can run side-by-side, and two pushes can race
against each other.

The shared state across requests is the on-disk repository.  The
[`StorageBackend`](../mizzle/src/backend/mod.rs) value itself
(`FsGitoxide`, `FsGitCli`) is a unit struct held by the server; the
per-request `Repo` handle is opened fresh by `backend.open(&repo_id)` at
the start of each request and dropped at the end.

For `FsGitoxide`, that handle wraps a `gix::ThreadSafeRepository`.
`gix::ThreadSafeRepository` is `Send + Sync` and uses internal Arcs, so
multiple tasks holding handles for the same repo share the same in-memory
ODB store and ref-store.  Each call into mizzle materialises a per-thread
view via `to_thread_local()`.

For `FsGitCli`, the handle is just a `PathBuf` and every operation forks
a `git` subprocess that opens the repo on its own — so concurrency is
mediated entirely by `git` itself plus the kernel's filesystem semantics.

## Operations that touch shared state

| Operation             | Reads           | Writes                                      |
|-----------------------|-----------------|---------------------------------------------|
| `list_refs`           | refs            | —                                           |
| `resolve_ref`         | refs            | —                                           |
| `has_object(s)`       | ODB             | —                                           |
| `build_pack`          | ODB only (refs resolved by caller) | —                       |
| `compute_push_kind`   | ODB             | —                                           |
| `ingest_pack`         | ODB             | new `pack-*.pack` + `.idx` in `objects/pack`|
| `inspect_ingested`    | the new pack    | —                                           |
| `update_refs`         | refs            | refs (loose files and/or `packed-refs`)     |
| `init_repo`           | filesystem      | repo skeleton                               |
| `rollback_ingest`     | —               | unlinks `pack-*.{pack,idx}`                 |

## Locking primitives gitoxide gives us

### Refs — `gix-ref` transactions

A ref update goes through a two-phase transaction:

1. **prepare** — for each affected loose ref, acquire a sentinel
   `refs/heads/foo.lock` file via `gix-lock`.  The lock is the dotlock
   convention: `O_CREAT|O_EXCL` on `<ref>.lock`.  If `packed-refs` is
   touched, `packed-refs.lock` is taken under the same scheme.  See the
   [`gix_lock`](https://docs.rs/gix-lock) crate docs for the acquisition
   contract.
2. **commit** — write the new ref bytes into the lock file, then
   `rename(2)` it over the live ref.  `rename` is atomic on POSIX
   filesystems, so concurrent readers see either the old or the new
   value and never a torn one.

`gix-ref` also evaluates a `PreviousValue` constraint inside the
prepared transaction: `MustExistAndMatch(old)` and
`ExistingMustMatch(old)` are CAS checks against the value observed under
the lock.  This is what prevents two concurrent updates from clobbering
each other.

The default failure mode for the lock is `Fail::Immediately` — if the
`.lock` file already exists, the transaction errors out instead of
blocking.

`gix-ref` also exposes a *multi-edit* transaction API
(`Transaction::prepare`/`commit` with a vec of `RefEdit`) that locks all
affected refs in the prepare phase and commits them as a unit.
`FsGitoxide::update_refs` uses this — see
[Ref updates](#ref-updates-cas-and-atomicity) below.

### Packs — content-addressed atomic install

`gix-pack::Bundle::write_to_directory` writes `pack-<hash>.pack` and
`pack-<hash>.idx` into a target directory using `gix-tempfile::persist`,
which is `rename(2)` of a temp file into place.  The pack name embeds
the content hash, so two writers producing the same pack collide on the
final name; the second writer's atomic rename overwrites identical
bytes (gix avoids the rename in this case to be friendly to Windows
mmaps).

mizzle stages each ingestion into a *fresh* `tempfile::tempdir()` and
then `move_file`s the produced files into `objects/pack`
(see `FsGitoxide::ingest_pack_sync` in
[fs_gitoxide.rs](../mizzle/src/backend/fs_gitoxide.rs)).
The move ordering matters: `.pack` first, then `.idx`.  Pack discovery
walks `.idx` files, so other readers never see an indexed pack whose
data file is missing.

### ODB — refresh-on-miss

A `gix::OdbHandle` holds a snapshot of the loaded pack indices.
On a lookup miss with the default `RefreshMode::AfterAllIndicesLoaded`,
the store rescans `objects/pack/*.idx` and retries.  This is what lets a
fetch that started before a concurrent push still find the
just-ingested objects if it ever needs them.

`Handle::prevent_pack_unload` upgrades the handle's token so that
already-mapped packs are *never* unloaded for the lifetime of the
handle, even if maintenance removes them from disk.  mizzle calls this
inside `stream_pack_to_channel`
([fs_gitoxide.rs](../mizzle/src/backend/fs_gitoxide.rs)) so the
streaming pack writer keeps a stable view of the pack files it's reading
from while the response streams to the client.

### Loose objects

Not exercised by the current backends — packs are the only object form
mizzle writes.  No race surface to discuss.

## What's safe today

**Concurrent fetches.**  Pure reads against the ODB and refs.  Each
request opens its own `OdbHandle`, gets refresh-on-miss, and `gix-ref`
reads see whichever atomic-renamed value is current.  Two fetches do
not interfere with each other.

**Fetch overlapping with push.**  The fetch's pack-streaming holds
`prevent_pack_unload`, so a push's ingestion adding a new `.pack` won't
disturb the fetch's existing readers.  The fetch's view of refs is
captured at the start of the request via `list_refs`, so the client
receives a consistent advertisement; the negotiated objects are
guaranteed to exist because `prevent_pack_unload` is in force during
streaming.

**Pack ingestion overlapping with another fetch.**  The pack/idx pair
is installed atomically via `rename(2)`.  Other readers either see
neither or both; they never read a half-installed pack.

**Pack rollback overlapping with another fetch.**  On Linux/POSIX,
`unlink(2)`-ing a mapped file leaves the mapping valid until the last
fd/mmap closes.  An in-flight fetch that has already opened the rolled-back
pack will continue to read from it.  On Windows this would fail — see
[Pack rollback on Windows](#pack-rollback-on-windows) below.

**Two pushes that touch disjoint refs.**  Each push locks its own
`<refname>.lock`.  `gix-ref` honours these per-ref locks even though
the transactions don't otherwise coordinate.

**Two pushes racing for the same ref.**  Covered by CAS — see the next
section.  Exercised by `concurrent_pushes_only_one_wins_*` in
[`tests/backend_parity.rs`](../mizzle/tests/backend_parity.rs) for every
backend.

## Ref updates: CAS and atomicity

The [`StorageBackend::update_refs`](../mizzle/src/backend/mod.rs)
contract requires an all-or-nothing transaction with compare-and-swap
against each update's `old_oid`.

`FsGitoxide::update_refs` builds one `gix_ref` transaction with a
`RefEdit` per update and commits it as a unit.  Each edit's `expected`
value is derived from the protocol:

| Update | `PreviousValue` |
|---|---|
| create (`old_oid` null) | `MustNotExist` |
| update | `MustExistAndMatch(old_oid)` |
| delete (`new_oid` null) | `MustExistAndMatch(old_oid)` |

Locks are acquired with `Fail::Immediately`, so a push that races
another push holding the same ref lock fails rather than waiting.
`FsGitCli::update_refs` gets the same behaviour by piping all
`update <ref> <new> <old>` lines into one `git update-ref --stdin`
invocation, which git treats as a single transaction; it serves as the
behavioural oracle.  `stale_oid_rejected` and `multi_ref_atomicity` in
`tests/backend_parity.rs` pin both properties.

`compute_push_kind` walks the full ancestry of `new_oid` looking for
`old_oid` (there is no walk cap), so a legitimate deep fast-forward is
never misclassified as a force-push.  Note that the classification is
computed against the *client's* `old_oid`; it is the CAS check in
`update_refs` that guarantees the ref still holds that value when the
update lands.

**Error surface.**  A CAS or lock failure is returned from
`update_refs` as an error.  The protocol layer currently logs it and
aborts the report-status response rather than emitting per-ref
`ng <ref> stale info` lines, so the client sees a failed push without a
ref-specific reason.  The ingested pack is not rolled back in this path
— it stays in `objects/pack` as unreferenced data until GC.

## Remaining gaps

### Inconsistent multi-ref snapshots in `list_refs`

`FsGitoxide::list_refs` walks `repo.refs.iter().all()` and peels each
ref one at a time.  Each individual ref read is atomic, but the iteration
is not snapshot-isolated against concurrent writers: if a transaction
commits during the walk, the returned `RefsSnapshot` may contain some
refs from before the commit and some from after.

In practice this is invisible to clients because the ref advertisement
is a flat list with no cross-ref invariants the protocol cares about.
This is documented as part of the `list_refs` contract on
`StorageBackend`: the snapshot is not linearisable and callers must not
derive cross-ref invariants from it.

`FsGitCli::list_refs` runs `git for-each-ref` and inherits whatever
snapshotting `git` provides, which is similarly best-effort.

### `init_repo` races

`FsGitoxide::init_repo` creates parent directories and calls
`gix::init_bare` unconditionally.  If a concurrent first-push to the
same auto-init repo wins the race, `init_bare` fails on the now
non-empty directory; mizzle treats that as success when `HEAD` exists.

`FsGitCli::init_repo` still has the original check-then-act shape
(`repo_path.exists()` followed by `git init --bare`).  Re-running
`git init --bare` on an existing repo is harmless, so this is a latent
issue rather than an active one.

### Pack rollback on Windows

`rollback_ingest` `unlink`s the idx and then the pack file.  The order
matters: removing the idx first means a concurrent reader that rescans
`objects/pack/*.idx` will stop seeing the pack before its data file
disappears.

On POSIX this is safe even if a concurrent fetch has either file
mapped.  On Windows, `DeleteFile` fails on a file that's open — and a
fetch holding `prevent_pack_unload` keeps both the pack and the idx
mapped for the duration of its streaming.  If/when mizzle is targeted
at Windows hosting, rollback needs to either defer until in-flight
fetches complete or use `MOVEFILE_DELAY_UNTIL_REBOOT`-style staging.

### Pack-name collision under concurrent ingest of identical content

Two pushes carrying the same set of objects produce the same
`pack-<hash>.{pack,idx}` filenames.  `ingest_pack` skips the move when
the destination already exists (identical content, by construction) and
records that it does not own the file, so a later `rollback_ingest`
never unlinks a pack another push installed.

## SQL and KV backends

Both get their guarantees from the store's transactions rather than
from `gix-lock`:

- **`SqlBackend`** applies every update inside one sqlx transaction;
  each update is a conditional `UPDATE`/`DELETE` (or `INSERT` for a
  create) and a zero `rows_affected` aborts the whole batch.
- **`KvBackend`** (TiKV) runs `update_refs` in a single pessimistic
  transaction that reads each ref, validates `old_oid`, and writes or
  clears the keys before committing.

Neither backend rolls back ingested objects on auth failure
(`rollback_ingest` is a no-op): orphan objects without a ref pointing at
them are harmless, matching unreferenced packs on the filesystem.
