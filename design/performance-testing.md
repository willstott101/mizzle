# Performance Testing

## Goal

Measure and compare mizzle's performance across backends and against
baseline git servers, covering bandwidth efficiency, latency, throughput,
resource consumption, and scalability.  The results should make it obvious
whether a backend is production-viable and where the bottlenecks are.

---

## What we already have

`benches/backends.rs` runs criterion microbenchmarks for clone, push,
fetch, and ls-remote against both filesystem backends.  These use a small
two-commit repo and measure wall-clock time.  This is a useful starting
point but has gaps:

- No bandwidth measurement — a fast clone that sends 10× the minimum
  pack is hiding a problem.
- Single repo shape — two commits doesn't exercise negotiation, delta
  compression, or shallow boundaries meaningfully.
- No concurrency — measures single-client latency only.
- No resource profiling — memory and CPU are invisible.
- No comparison to a vanilla `git-http-backend` or Gitea baseline.

---

## Dimensions

### 1. Bandwidth efficiency

The most important and least obvious metric.  A server can be fast in
wall-clock time while sending packs that are far larger than necessary.
Bandwidth matters because clone/fetch traffic is the dominant cost at
scale (egress bills, CI cache pressure, slow networks).

**What to measure:**

| Scenario | Metric | Why it matters |
|---|---|---|
| Full clone | Pack bytes transferred | Baseline pack quality |
| Incremental fetch (1 new commit) | Pack bytes transferred | Negotiation effectiveness — should send ≈ one object |
| Incremental fetch (100 new commits) | Pack bytes transferred | Delta chain quality at moderate distance |
| Thin-pack fetch | Pack bytes vs non-thin | Verifies thin-pack delta savings |
| Shallow clone (`--depth 1`) | Pack bytes transferred | Should be ≪ full clone |
| Partial clone (`--filter=blob:none`) | Pack bytes transferred | Should exclude blobs entirely |
| Partial clone (`--filter=tree:0`) | Pack bytes transferred | Should exclude trees |

**How to measure:**

Instrument at the transport layer — wrap the response body in a
counting reader that records total bytes written.  The test harness
already has a `SniffingProxy` in `fetch.rs` that intercepts traffic;
extend this pattern to capture byte counts.  Compare against a
reference value from `git-http-backend` serving the same repo.

**Reference repos (shared across all dimensions):**

| Name | Shape | Purpose |
|---|---|---|
| `tiny` | 2 commits, 1 branch | Sanity check, catches regressions in overhead |
| `medium` | ~1,000 commits, 5 branches, tags, merge history | Exercises negotiation and delta chains |
| `large-blobs` | medium history + 10 binary blobs (1–50 MB) | Tests blob-heavy repos (game assets, ML models) |
| `wide-refs` | 100 commits, 500 branches, 500 tags | Ref advertisement and negotiation with many tips |
| `deep` | 10,000 linear commits | Deep history traversal, shallow boundary computation |

Build these deterministically in the test harness (fixed timestamps,
fixed content) so byte counts are reproducible across runs.

### 2. Latency

Wall-clock time from request to completion, broken into phases where
possible.

**What to measure:**

| Operation | Phases |
|---|---|
| Clone (full) | Ref advertisement → negotiation → pack stream start → pack stream end |
| Clone (shallow) | Same phases, expect faster pack stream |
| Fetch (incremental) | Negotiation rounds → pack stream |
| Push (small) | Pack upload → ingestion → auth → ref update → response |
| Push (large) | Same, expect ingestion to dominate |
| ls-remote | Ref advertisement only |

**How to measure:**

Timestamps at phase boundaries inside the serve layer, emitted as
structured log events or returned in a `PerfTrace` struct.  The
benchmark harness collects these alongside wall-clock time.  Criterion
remains the driver for statistical rigour (warm-up, iterations,
outlier detection).

### 3. Throughput

Operations per second under concurrent load.  This is where backend
differences will be most visible — contention on the filesystem, lock
behaviour on ref updates, connection overhead.

**What to measure:**

- Concurrent clones: N clients clone the same repo simultaneously.
  Measure p50/p95/p99 latency and total throughput as N increases
  (1, 2, 4, 8, 16, 32).
- Concurrent fetches: N clients fetch with different have-sets.
- Concurrent pushes to different refs: N clients push to distinct
  branches.  Should scale well.
- Concurrent pushes to the same ref: N clients push to `main`.
  Expect serialisation — measure rejection rate and winner latency.
- Mixed workload: readers and writers simultaneously.

**How to measure:**

A load-generation harness that spawns N tokio tasks, each running a
git CLI operation against the mizzle server.  Collect per-operation
timing.  This is separate from criterion (which targets
single-operation statistics).

Each task prepares its own client repo (pre-cloned, with a commit
ready to push) before the measurement window opens.  All N tasks
start simultaneously via a shared barrier.  The harness records
per-operation wall-clock time and computes p50/p95/p99 and
operations/second from the collected timings.  This is a simple
custom binary (`benches/load.rs` or `examples/loadtest.rs`), not a
generic HTTP load tool — git's multi-round-trip protocol doesn't
map well to `wrk`-style drivers.

### 4. Resource consumption

**What to measure:**

- **Peak RSS** during pack generation for large repos.  The streaming
  architecture should keep this bounded, but delta computation and
  object caching in gitoxide may spike.
- **Peak RSS** during pack ingestion (push).  Staging to temp storage
  should bound this, but `inspect_pack` inflates every object.
- **CPU time** for negotiation on deep repos.  `build_have_set`
  materialises the full reachable graph — this is called out in the
  roadmap 5.2b as a known scaling issue.
- **Temp disk usage** during push staging.  Important for operators
  sizing ephemeral storage.
- **File descriptor count** under concurrent load.  Each repo open
  may hold pack file descriptors.

**How to measure:**

For memory: run benchmarks under `/usr/bin/time -v` or use
`jemalloc`'s profiling (`MALLOC_CONF=prof:true`).  For CPU: `perf
stat` or criterion's built-in CPU-time measurement.  For disk:
monitor the temp directory size during large pushes.  These are
one-off profiling runs, not part of CI.

### 5. Backend comparison

Every measurement above runs against all backends.  The comparison
matrix:

| Backend | Role |
|---|---|
| `FsGitoxide` | Primary — expected to be fastest |
| `FsGitCli` | Correctness baseline — may be slower due to process spawning |
| `git-http-backend` | External baseline — what users are migrating from |

For bandwidth, `git-http-backend` is the reference.  Mizzle should
produce packs of equal or smaller size.  For latency and throughput,
comparisons are between mizzle backends; `git-http-backend` via CGI
is not a meaningful latency baseline.

---

## Test repo generation

All repos are generated by a shared `RepoBuilder` in the benchmark
harness with deterministic content:

```
RepoBuilder::new(path)
    .linear_commits(n)        // n commits on main, fixed content
    .branches(names)          // branch from commit at offset
    .tags(names)              // annotated tags at offsets
    .merge(source, target)    // merge commit
    .large_blob(name, size)   // binary blob at HEAD
    .build()
```

Fixed author/committer timestamps and names (the test harness already
does this).  Content derived from commit index so packs are
deterministic.

---

## Bandwidth measurement detail

Bandwidth is the metric most likely to reveal real bugs (bad
negotiation, missing delta compression, unnecessary objects in pack).
The measurement approach:

1. **Counting proxy.**  The test harness spins up a TCP proxy between
   the git client and the mizzle server.  The proxy counts bytes in
   each direction and tags them by operation phase (ref advertisement
   vs pack data).  The existing `SniffingProxy` in `fetch.rs`
   demonstrates this pattern.

2. **Reference values.**  For each (repo, operation) pair, record the
   byte count from `git-http-backend` serving the same repo.  Store
   these as checked-in expected values.  The test asserts that
   mizzle's byte count is within a tolerance of the reference.

   **Open question:** what tolerance?  For deterministic repos the pack
   byte count *should* be exactly reproducible across runs of the same
   server, but gitoxide and git may choose different delta bases, so
   the cross-server comparison may not be exact.  Start with exact
   match for same-server regression detection, and determine the
   cross-server tolerance empirically once Step 6 is running.

3. **Regression detection.**  Run bandwidth benchmarks in CI.  If pack
   size increases by more than the tolerance, fail the build.  This
   catches negotiation regressions, missing filter support, and delta
   compression changes.

---

## Variance control

Sources of measurement noise to control before trusting numbers:

**Reference repo lifecycle.**  The `medium` and `deep` repos must be
built once per benchmark process and shared across all groups — not
rebuilt per-group or per-iteration.  Use `std::sync::OnceLock` (or leak
the `TempDir` as the existing bench already does for `temprepo`).  A
cold build of the `deep` repo takes several seconds and would dwarf
measurements if it falls inside the iteration loop.

**Push bench state drift.**  The existing `bench_push` includes a full
clone inside each Criterion iteration, so it measures clone + push
together.  It also mutates the server-side repo on every iteration,
so later iterations exercise a larger have-set.  Fix: pre-clone once
outside the loop (matching what `bench_fetch` already does), and either
reset the server-side repo to its initial state between iterations or
document the drift explicitly.

**`tempdir()` in the hot path.**  `bench_clone` and `bench_push`
allocate a new `TempDir` on each iteration.  Tmpfs allocation is fast
but not free.  Pre-allocate a staging directory and
remove/recreate its contents between iterations instead.

**Page-cache warm-up.**  Criterion's default 3 s warm-up may not be
sufficient to warm the pack-file page cache for `deep` or `large-blobs`.
Add an explicit pre-measurement pass that reads through the pack file
before the timing window opens.

**Server / load-generator CPU sharing.**  In Step 4, the mizzle server
and the load-generator tasks share the same machine and compete for
cores.  P99 latency under high concurrency will reflect scheduler
contention as well as protocol cost — document this rather than treating
the numbers as production-representative.

**Latency baseline.**  `git-http-backend` served via a CGI adapter
introduces process-spawn overhead absent in production deployments.
Use `git-http-backend` only as a bandwidth reference (Step 6); latency
comparisons should be between mizzle backends only.

## Implementation plan

### Step 1 — Deterministic repo builder

Extract repo creation from `tests/common` and `benches/backends.rs`
into a shared `RepoBuilder` that supports the five reference repo
shapes.  Both the test harness and benchmarks use it.

### Step 2 — Bandwidth benchmarks

Add a `benches/bandwidth.rs` that measures transfer size for each
(repo, operation) pair.  Use the counting-proxy approach.  No
statistical iteration needed — byte counts are deterministic for a
given repo.  Assert against reference values.

### Step 3 — Phased latency instrumentation

Replace the `log` dependency with `tracing`, bridging existing `log::*`
call sites via `tracing-log`.  Annotate the key internal functions with
`#[tracing::instrument(skip_all)]`:

- `build_have_set` in `pack.rs` — the full object-graph walk (roadmap 5.2b candidate)
- `objects_for_fetch_filtered` — wraps have-set build and want traversal
- the pack stream / compression step in `fetch.rs`

Spans are zero-cost when no subscriber is installed, so the annotations
ship unconditionally without a feature flag.  In the benchmark harness,
install a timing subscriber before each benchmark group that accumulates
per-span `Duration` via its `on_close` hook, then read the totals after
`b.iter(...)` completes.  This gives sub-operation breakdowns independent
of transport noise and aligned with Criterion's iteration model.

`#[instrument]` is safe for the sync functions in `pack.rs`.  For async
functions in `serve.rs` and `fetch.rs`, use `#[instrument(skip_all)]`
and verify no non-`Send` types are held across await points before adding
spans there.

Extend `benches/backends.rs` to include the `medium` and `deep`
reference repos.  The `deep`-repo incremental-fetch span data is the
primary signal for whether roadmap 5.2b is needed.

### Step 3.1 — Bitmap (5.2b) implementation + comparison bench

Roadmap 5.2b is implemented.  `src/bitmap.rs` reads git's `.bitmap` +
`.rev` files (format v1, sha1) directly — gitoxide 0.67/0.68 doesn't
expose a reachability-bitmap reader, only the lower-level EWAH primitive
in `gix-bitmap`.  `fs_gitoxide::try_bitmap_have_set` probes the repo's
pack directory, loads each pack's bitmap if present, and returns a
complete have-set when every have OID is covered.  On any miss the
backend falls back to `pack::build_have_set` (the walker).

The `fetch_incremental` bench in `benches/backends.rs` exercises both
paths by building two copies of the `deep` repo (10,000 linear commits):
one plain, one post-processed with `git repack -adb`.  Bench IDs encode
the variant: `fetch_incremental/<backend>/{nobitmap,bitmap}/{1,100}_behind`.
The span-totals subscriber emits per-span timings to
`target/criterion/bitmap-spans.jsonl` and to stderr.

Spans to watch:

- `build_have_set` — fires only on the walker path; absent on the bitmap
  path means `try_bitmap_have_set` covered all haves.
- `try_bitmap_have_set` — ~60-100µs when no bitmap is present (directory
  probe + early exit), hundreds of µs to low ms when it actually loads a
  bitmap and answers.
- `objects_for_fetch_with_have_set` — the shared tail work after the
  have-set is resolved, identical code on both paths.

FsGitCli uses git's native bitmap support via `git pack-objects --revs`.
Its span totals are always empty (it doesn't go through `pack::*`), but
its wall-clock delta across the two variants is a useful external
reference for the gitoxide path.

### Step 4 — Concurrency harness

Build a load generator that runs N concurrent git operations.  Start
with clone and push throughput.  Report p50/p95/p99 and
operations/second.  Run as a separate binary (`benches/load.rs` or
`examples/loadtest.rs`).

### Step 5 — Resource profiling

Add a profiling script (`scripts/profile.sh`) that runs key operations
under `perf`, `/usr/bin/time`, and jemalloc profiling.  Document the
expected resource profile for each reference repo so regressions are
visible.

### Step 6 — External baseline

Add `git-http-backend` as a bandwidth comparison target.  The test
harness starts a `git-http-backend` server alongside the mizzle servers,
running against the same repos.  Use for bandwidth results only — see
the variance control section for why latency comparisons against a CGI
adapter are not meaningful.

**Open question:** what's the lightest way to run `git-http-backend`
in the test harness?  Options: spawn `git http-backend` behind a
minimal CGI adapter (avoids nginx/lighttpd dependency), use a
container, or require system-installed packages.  A CGI adapter in
Rust (tiny HTTP server that translates to CGI env vars) would keep it
self-contained but is effort to write.  This step is profiling-only,
not CI, so a system dependency may be acceptable.

---

## What success looks like

- **Bandwidth:** Same-server regression: pack byte counts are exactly
  reproducible for deterministic repos.  Cross-server: mizzle produces
  packs comparable to `git-http-backend` (tolerance TBD — see open
  questions).  Thin packs, partial clones, and shallow clones transfer
  strictly less data than full clones by the expected margin.
- **Latency:** FsGitoxide is faster than FsGitCli for clone and fetch
  (process-spawn overhead is measurable).  Push latency is dominated
  by pack ingestion, not auth or protocol overhead.
- **Throughput:** Concurrent read operations scale linearly up to
  available cores.  Concurrent writes to distinct refs show no
  contention.
- **Resources:** Peak RSS during clone of a 1 GB repo (on-disk
  packfile size) stays under 200 MB (streaming, not buffering).
  `inspect_pack` memory scales with pack object count, not pack data
  size (blocked on roadmap 5.2a).

---

## Open questions

- **Bandwidth tolerance:** Should cross-server bandwidth comparison
  (mizzle vs `git-http-backend`) use a fixed tolerance or be
  determined empirically?  See bandwidth measurement detail above.

- **`git-http-backend` harness:** What's the simplest way to run it
  without requiring nginx?  See Step 6 above.

- **Degradation under memory pressure:** The resource section measures
  peak RSS, but what happens when the server is memory-constrained
  (e.g. container with a 256 MB limit)?  Does gitoxide degrade
  gracefully or OOM?  This matters for operators setting container
  limits but may be out of scope for initial benchmarking.

- **Startup / cold-path cost:** For future non-filesystem backends
  (SQL, distributed), the cost of the first request against a repo
  (connection setup, cache priming) may dominate.  Not relevant for
  filesystem backends today, but the harness should be structured so
  cold-start measurement can be added later.

- **Roadmap cross-reference:** The `deep` repo is specifically
  designed to measure whether bitmap-accelerated have-set (roadmap
  5.2b) is needed.  The `large-blobs` repo will show whether lazy
  pack inspection (5.2a) matters.  Results from these benchmarks
  should inform prioritisation of those optimisations.
