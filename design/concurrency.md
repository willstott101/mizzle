# Concurrency

How concurrent reads and writes interact in mizzle, and which of those
interactions are safe under the locking guarantees gitoxide actually
provides.

This document is specific to the filesystem backends.  SQL and other
backends will have their own concurrency stories rooted in the
transactional guarantees of the underlying store.

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

Note that `gix-ref` also exposes a *multi-edit* transaction API
(`Transaction::prepare`/`commit` with a vec of `RefEdit`) that locks all
affected refs in the prepare phase and commits them as a unit.  mizzle
does not currently use this — see the
[multi-ref non-atomicity gap](#non-atomic-multi-ref-updates-fsgitoxide)
below.

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
(see [fs_gitoxide.rs:266-301](../mizzle/src/backend/fs_gitoxide.rs#L266)).
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
([fs_gitoxide.rs:379](../mizzle/src/backend/fs_gitoxide.rs#L379)) so the
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

## Gaps and races

### Lost updates on `refs/*` (FsGitoxide only)

`FsGitoxide::update_refs` calls

```rust
local.reference(name, oid, PreviousValue::Any, "push")
```

at [fs_gitoxide.rs:131-141](../mizzle/src/backend/fs_gitoxide.rs#L131).
`PreviousValue::Any` tells `gix-ref` to overwrite whatever value is
currently in the ref without comparison.  The protocol-supplied
`old_oid` (the value the client believed the ref had when it started its
push) is discarded.

This breaks the lost-update guarantee that the git wire protocol
otherwise provides.  Two pushes racing for `refs/heads/main`:

1. Both observe `main = A` in the receive-pack ref advertisement.
2. Push #1 sends `A → B`, push #2 sends `A → C`.
3. mizzle classifies push #1 as fast-forward (B descends from A) and
   push #2 as fast-forward (C also descends from A).
4. Both authorisations pass.
5. Push #1 takes the `.lock`, writes `B`, releases.
6. Push #2 takes the `.lock`, writes `C`, releases — **silently
   overwriting B**.

The same hole bypasses branch protection: the push-kind classification
(`compute_push_kind`) walks from the *client's* `new_oid` looking for
the *client's* `old_oid`, not the live server value.  A user whose
authoriser only allows fast-forwards to `main` can still clobber a
concurrent push by pushing a fast-forward of a stale tip.

`FsGitCli` does not have this bug.  `update_refs` pipes
`update <ref> <new> <old>` lines into `git update-ref --stdin`, and
git's update-ref enforces CAS against `<old>` under the per-ref lock.
A racing push fails with `cannot lock ref … is at … but expected …`.

The fix is a one-liner per case in `FsGitoxide::update_refs`: translate
the protocol's `old_oid` into the right `PreviousValue` variant —
`MustExistAndMatch(old)` for a normal update, `MustNotExist` for a
create, `MustExistAndMatch(old)` paired with the deletion variant for a
delete.  Then surface the resulting `ReferenceOutOfDate` error to the
client as an `ng <ref> stale info` line.

A subtlety: `compute_push_kind` walks at most `MAX_FF_WALK` commits from
`new_oid` looking for `old_oid`
([fs_gitoxide.rs:174-178](../mizzle/src/backend/fs_gitoxide.rs#L174)).
A legitimate fast-forward whose ancestry chain is longer than the cap
will be misclassified as a force-push.  Today that just affects which
authorisation path runs.  Once CAS lands, it would also cause the gix
transaction to reject the update with `ReferenceOutOfDate` even though
the client had the right `old_oid`, so the cap should either be lifted
for this check or the misclassification accepted as a known false
negative documented in the trait.

### Non-atomic multi-ref updates (FsGitoxide)

`FsGitoxide::update_refs` iterates over `updates` and issues one
single-ref transaction per update via `local.reference(...)`
([fs_gitoxide.rs:133-142](../mizzle/src/backend/fs_gitoxide.rs#L133)).
There is no enclosing transaction.  A push that updates several refs
atomically on the wire — say `refs/heads/main` plus `refs/tags/v1.0` —
can land partially: the first ref commits, the second fails (lock
contention, CAS mismatch once that's fixed, disk error), and the
repository is left in a state the client never asked for.

`gix-ref` exposes the right primitive: a single `Transaction` carrying a
vec of `RefEdit` locks every affected ref in `prepare` and commits them
as a batch.  Either every edit lands or none do.

`FsGitCli::update_refs` already gets this behaviour for free by piping
all `update <ref> <new> <old>` lines into one
`git update-ref --stdin` invocation, which git treats as a single
transaction.

The fix is to build a `gix_ref::transaction::Transaction` with one
`RefEdit` per update and commit it once.  This change pairs naturally
with the CAS fix — both are about translating the protocol's intent
into the right `gix-ref` API surface.

### Inconsistent multi-ref snapshots in `list_refs`

`FsGitoxide::list_refs` walks `repo.refs.iter().all()` and peels each
ref one at a time.  Each individual ref read is atomic, but the iteration
is not snapshot-isolated against concurrent writers: if a transaction
commits during the walk, the returned `RefsSnapshot` may contain some
refs from before the commit and some from after.

In practice this is invisible to clients because the ref advertisement
is a flat list with no cross-ref invariants the protocol cares about.
Clients that re-list immediately afterwards just see the new state.
Documented here so it isn't surprising: `list_refs` does not promise a
linearisable snapshot of the ref space.

`FsGitCli::list_refs` runs `git for-each-ref` and inherits whatever
snapshotting `git` provides, which is similarly best-effort.

### `init_repo` TOCTOU

`FsGitoxide::init_repo` checks `repo_path.exists()` and then calls
`gix::init_bare`.  Two concurrent first-pushes to the same auto-init
repo could both pass the check.  `gix::init_bare` on an already-initialised
directory is idempotent enough not to corrupt anything (it rewrites the
skeleton files atomically), but the two inits will race on creating
`refs/`, `objects/`, `HEAD`, `config`, etc.  This is a latent issue more
than an active one; if it becomes a real problem the fix is to take a
filesystem-level lock on the repo path.

`FsGitCli::init_repo` has the same shape.

### Pack rollback on Windows

`rollback_ingest` `unlink`s the idx and then the pack file
unconditionally
([fs_gitoxide.rs:326-329](../mizzle/src/backend/fs_gitoxide.rs#L326)).
The order matters: removing the idx first means a concurrent reader
that rescans `objects/pack/*.idx` will stop seeing the pack before its
data file disappears.

On POSIX this is safe even if a concurrent fetch has either file
mapped.  On Windows, `DeleteFile` fails on a file that's open — and a
fetch holding `prevent_pack_unload` keeps both the pack and the idx
mapped for the duration of its streaming.  If/when mizzle is targeted
at Windows hosting, rollback needs to either defer until in-flight
fetches complete or use `MOVEFILE_DELAY_UNTIL_REBOOT`-style staging.

### Pack-name collision under concurrent ingest of identical content

Two pushes carrying the same set of objects will produce the same
`pack-<hash>.{pack,idx}` filenames.  mizzle's `move_file` uses
`std::fs::rename`, which on POSIX overwrites silently.  The second
ingest then issues `rollback_ingest` on a file path that the first
ingest still owns and considers durable, which would unlink the live
pack.

This is a vanishingly rare ordering — two clients pushing byte-identical
packs simultaneously — but worth recording.  A defensive fix would be
to skip the move (and skip rollback registration) if the destination
already exists with the same content hash.

## Recommendations

In rough priority order:

1. **Rewrite `FsGitoxide::update_refs` to use a single multi-ref
   transaction with per-ref CAS.**  Highest priority — covers two
   correctness bugs at once.  Build a `gix_ref::transaction::Transaction`
   with one `RefEdit` per update; set each edit's `expected` to
   `MustExistAndMatch(old)` / `MustNotExist` derived from the protocol's
   `old_oid`.  Commit as a unit.  Surface
   `Error::PreviousValueMismatch` to the client as `ng <ref> stale info`,
   and any other transaction error as `ng <ref> <reason>` for every ref
   in the batch.  The CLI backend already gets both behaviours via
   `git update-ref --stdin`, so it serves as a behavioural oracle.
2. **Either lift `MAX_FF_WALK` for `compute_push_kind` or document the
   false-negative.**  Once (1) lands, a stale walk cap turns a
   legitimate deep fast-forward into a CAS-rejected push.  Pick one:
   walk to history root for the FF check, or accept the misclassification
   and document it on the trait.
3. **Define the `list_refs` consistency contract in the storage trait
   docs.**  Make explicit that the snapshot is not linearisable and
   that callers must not derive cross-ref invariants from it.
4. **Tighten `init_repo` against double-init**, either with a path-level
   lock or by swallowing the "already initialised" race on the second
   `gix::init_bare` call.
5. **Defer pack-rollback until safe on Windows** if/when Windows
   hosting becomes a target.
6. **Skip the rename in `ingest_pack` when the destination pack already
   exists**, mirroring what `gix-pack` itself does inside its own
   tempdir flow, to close the duplicate-content race.

None of these block the SQL backend work — that backend will get its
concurrency story from the database's transaction model rather than
from `gix-lock`.
