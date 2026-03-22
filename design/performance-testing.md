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
  roadmap (5.1b) as a known scaling issue.
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
`git-http-backend` is the bar to beat (or at least match).

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

Add timing hooks in the serve layer.  A few `Instant::now()` calls at
phase boundaries are cheap enough to leave on unconditionally — avoid
a feature flag unless profiling shows measurable overhead.  Extend
`benches/backends.rs` to report per-phase timings alongside total
wall-clock time.  Add the medium and deep repos to the benchmark
suite.

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

Add `git-http-backend` as a comparison target.  The test harness
starts a `git-http-backend` server alongside the mizzle servers,
running against the same repos.  Bandwidth and latency results include
this baseline.

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
- **Latency:** FsGitoxide is no slower than `git-http-backend` for
  clone and fetch.  Push latency is dominated by pack ingestion, not
  auth or protocol overhead.
- **Throughput:** Concurrent read operations scale linearly up to
  available cores.  Concurrent writes to distinct refs show no
  contention.
- **Resources:** Peak RSS during clone of a 1 GB repo (on-disk
  packfile size) stays under 200 MB (streaming, not buffering).
  `inspect_pack` memory scales with pack object count, not pack data
  size (blocked on roadmap 5.1a).

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
  5.1b) is needed.  The `large-blobs` repo will show whether lazy
  pack inspection (5.1a) matters.  Results from these benchmarks
  should inform prioritisation of those optimisations.
