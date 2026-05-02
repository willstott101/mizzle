# Auth

How mizzle hands authorisation decisions back to the embedding forge,
and how commit-signature verification fits into that.

For the wider architecture see [architecture.md](architecture.md).

## Principles

Two rules govern every interface in this document:

1. **Auth never opens the repository.** Everything an authoriser needs to
   make a decision is delivered through the trait surface as plain values
   or via a `Comparison` handle whose accessors are explicit and bounded.
   The forge never has to call `git rev-list` itself or open the object
   graph behind mizzle's back.
2. **Pack data is staged, not stored, until auth passes.** A rejected push
   leaves no trace in the object store.

Mizzle does not legislate *when* the forge spends its latency budget — see
[§ Strategies](#strategies) for the common shapes.

## Layers

```
┌──────────────────────────────────────────────────────────────┐
│  Transport     e.g. HTTP smart protocol · SSH                │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Identity     Resolved by the forge before mizzle is called. │
│               HTTP: forge handler / middleware.              │
│               SSH:  russh accepts all keys, defers to        │
│                     SshAuth::authorize at exec time.         │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  RepoAccess   per-request, constructed by forge with         │
│               identity already resolved                      │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Mizzle pipeline                                             │
│   fetch  → list_refs · negotiate · build_pack                │
│   push   → preliminary auth · stage · ingest · inspect ·     │
│            authorize_push (lazy verification) · commit       │
└──────────────────────────────────────────────────────────────┘
```

The two transports listed in the diagram are the ones mizzle ships
integrations for; the trait surface is transport-agnostic and a forge
can wire any front-end that produces a `RepoAccess` value.

## Identity vs authorisation

Identity is established before mizzle is called. The trait surface is
deliberately silent on identity — the forge models it however it likes
(numeric user id, opaque token, verified-email set) inside its own
`RepoAccess` impl. The two integrations mizzle currently ships are:

* **HTTP** — the forge runs in front of mizzle (e.g. an axum handler
  that already authenticated the request via headers, cookies, tokens,
  or mTLS) and constructs the `RepoAccess` value passed to `serve_*`.
* **SSH** — russh accepts every public key, then defers to
  `SshAuth::authorize(user, key, repo_path)` at exec time. That single
  call resolves identity *and* loads permissions; the returned
  `RepoAccess` carries both.

Other transports (git-daemon, in-process callers, custom protocols)
follow the same pattern: resolve identity, construct `RepoAccess`, hand
it to the protocol functions.

## Strategies

Auth methods run on the push hot path; their latency lands on every
push. Mizzle imposes no shape on how the forge spends that budget. The
three common patterns:

* **Front-loaded.** Resolve every permission at `RepoAccess`
  construction (HTTP middleware, `SshAuth::authorize`). Per-call
  methods become pure value comparisons against pre-loaded data.
  Fastest steady-state; pays the cost even on rejected pushes.
  Good default for forges with cheap permission lookups (cached
  in-process, local DB).
* **Per-push resolved.** `authorize_preliminary` does the lookup
  and returns a typed `Self::PushContext` carrying the resolved
  state; the full `authorize_push` reads from `ctx`. Pays once per
  push, can short-circuit cheaply on obvious rejections, no
  interior mutability needed to share state between the two calls.
  Recommended when permission resolution is non-trivial (network
  round-trip, complex aggregation).
* **Lazy / pull-based.** The `Comparison` handle passed to
  `authorize_push` only computes a field (per-ref new commits,
  dropped commits, ref diffs, signature verification) when the
  forge actually accesses it. Useful when policy is sparse — e.g.
  only `refs/heads/main` triggers the expensive checks.

These compose. A typical forge front-loads permissions at construction,
uses `PushContext` to carry resolved branch-protection rules, and reads
from the lazy `Comparison` accessors only on the refs it cares about.

## The push pipeline

```
0. forge handler                  resolve identity, construct RepoAccess
1. read_receive_request           parse ref-update headers
2. authorize_preliminary          refnames + PushKind only;
                                  returns PushContext
3. stage_pack                     stream to a temp file
4. ingest_pack                    write into object store as quarantined data
5. inspect_ingested               extract commit/tag metadata + signatures
6. authorize_push                 forge gets a Comparison handle and the
                                  PushContext from step 2; signature
                                  verification, reachability walks, and
                                  ref-diffs are computed lazily on access
7. update_refs                    on Ok; otherwise rollback_ingest
8. post_receive                   CI triggers, audit log
```

The forge's own pipeline (TLS termination, identity resolution,
rate-limiting, request routing) runs at step 0; mizzle takes over at
step 1. Step 6's lazy computations all dispatch to the configured
storage backend.

## The `Comparison` handle

`Comparison` is the read-only view a forge sees during `authorize_push`.
It exposes lazy accessors over the staged pack and the existing repo
state:

| Accessor                       | What it gives the forge                                                                                  | Cost on first access                                              |
|--------------------------------|----------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------|
| `refs() -> &[PushRef]`         | The refs being updated (refname, kind, old_oid, new_oid).                                                | O(1).                                                             |
| `new_commits(ref)`             | Commits introduced on this ref by this push, parent-first, deduped against pre-existing refs and earlier refs in the same push. | One reachability walk; configurable cap.                          |
| `dropped_commits(ref)`         | Commits reachable from `old_oid` but not `new_oid` — what a force-push or delete would lose.             | One reachability walk; configurable cap.                          |
| `ref_diff(ref)`                | Path-level summary: added / modified / removed paths with mode and oid.                                  | Tree walk against the parent tree; cached.                        |
| `verify(commit)`               | `&VerificationStatus` for a commit signature; runs crypto on first access, batches `verification_keys`.  | One verifier dispatch; key resolution batched per push.           |
| `verify_tag(tag)`              | Same, for annotated tag signatures.                                                                      | Same.                                                             |
| `read_blob(oid, cap)`          | Blob bytes for content-inspection policies (e.g. `.gitmodules`, secret-scanning).                        | O(blob size); `None` if not in the staged pack or above the cap.  |
| `pack_metadata()`              | Object identities and sizes from pack inspection.                                                        | Already populated by step 5.                                      |
| `tags()`                       | Annotated tags introduced by this push.                                                                  | Populated by step 5.                                              |

A forge that touches only `refs()` pays nothing beyond what the pipeline
already did. A forge that calls `dropped_commits` for every ref pays for
the walks. The pattern is the same shape as GraphQL resolvers and
bevy_ecs queries: the *access pattern is the declaration of needs*, no
separate machinery.

The same `Comparison` shape is intended to back MR-time evaluation
(cross-branch, potentially cross-repo) so a forge can write
`fn check(c: &impl Comparison) -> Result<…>` once and reuse it for both
push-time and merge-request-time policies.

## Committer verification — supported flows

Mizzle ships verifiers for the three commit-signature formats that
account for essentially all real-world signed commits:

| Format                       | Forge supplies                                                                                              |
|------------------------------|-------------------------------------------------------------------------------------------------------------|
| **OpenPGP**                  | armoured public keys                                                                                        |
| **SSH**                      | SSH pubkeys, namespace `git` (per `git config gpg.ssh.allowedSignersFile`)                                  |
| **X.509 / S/MIME (static)**  | trust roots, optional CRL/OCSP policy                                                                       |

Verification is lazy. When the forge calls `comparison.verify(commit)`:

1. Mizzle detects the signature format from the header bytes.
2. If a native verifier is enabled for that format, mizzle batches
   `(email, format)` signers and calls
   `RepoAccess::verification_keys(&signers)` once per push to resolve
   candidate keys, then runs the matching verifier against the
   reconstructed signed payload.
3. `RepoAccess::verify_external` is then offered the signature; if it
   returns `Some(status)`, that result wins. If it returns `None`,
   mizzle's verdict stands (or `UnsupportedFormat` if no native
   verifier handled it).
4. The result is cached on the `Comparison` so subsequent
   `verify(commit)` calls are O(1).

The forge never sees raw signature bytes or signed payloads on the
common path — the trait surface stays narrow and the cost of shipping
kilobytes-per-commit across the auth boundary is paid in-process once,
by mizzle, against keys the forge already had cached.

### Sigstore / gitsign — the escape hatch

Sigstore-signed commits (gitsign) cannot be verified against a static
key. The signing certificate is short-lived, issued by Fulcio against an
OIDC identity, and audited via a Rekor transparency log entry whose
Signed Entry Timestamp is embedded in the signature. Verification
needs:

- Fulcio root CA + Rekor public key (static trust anchors)
- the certificate chain extracted from the CMS blob
- the SET / inclusion proof, also in the CMS blob
- a policy mapping OIDC issuer + SAN → forge identity

This is more than mizzle should bake in. Forges that want gitsign
implement `verify_external`, which receives the
`(format, raw_signature, signed_payload)` tuple and returns a
`VerificationStatus` directly.

A reference adapter crate may live alongside mizzle later; it is not
part of the core library.

### Out of scope

- **Long-lived X.509 with custom revocation infrastructure** —
  forge-implementable via `verify_external`.
- **Bespoke signature formats in non-`gpgsig` headers** — forges that
  ship custom git clients can use `verify_external`.
- **Timestamping authorities other than Rekor** — `verify_external`.
- **Read-side authorisation** (per-ref read protection, fetch deny on
  private branches). This document covers push only; v1 assumes
  "if you can reach the repo, you can fetch everything".
- **Asynchronous / queued approval** (e.g. "first push from a new
  account → human review → eventual decision"). `authorize_push` is
  synchronous; defer-to-human-review workflows belong outside mizzle,
  e.g. by accepting into a quarantine ref namespace and gating
  promotion via the forge UI.

## Trait surface

The canonical definition lives in
[`mizzle/src/traits.rs`](../mizzle/src/traits.rs). The shape below is
illustrative; the actual signatures, default impls, and bounds may
evolve. Types like `VerificationStatus`, `SignatureFormat`, and
`SignedIdentity` are marked `#[non_exhaustive]` so future variants are
not breaking.

```rust
pub trait RepoAccess {
    type RepoId;
    type PushContext: Default;

    fn repo_id(&self) -> &Self::RepoId;

    /// Cheap classification before pack data is transferred.
    /// Returns a typed value carried into authorize_push.
    fn authorize_preliminary(
        &self,
        refs: &[PushRef<'_>],
    ) -> Result<Self::PushContext, String>;

    /// Full authorisation. The forge inspects whatever it needs
    /// through the Comparison handle; everything is lazy and cached.
    fn authorize_push(
        &self,
        ctx: &Self::PushContext,
        push: &dyn Comparison<'_>,
    ) -> Result<(), String>;

    /// Resolve candidate verification keys for a batch of (email,
    /// format) signers. Default: no keys → every signed commit
    /// verifies as `UnknownKey`.
    fn verification_keys(
        &self,
        signers: &[Signer<'_>],
    ) -> HashMap<SignerKey, Vec<VerificationKey>>;

    /// Per-signature override / escape hatch. Native verifier runs
    /// first. If this returns Some, that result wins; if None, the
    /// native verdict stands.
    fn verify_external(
        &self,
        sig: &ExternalSig<'_>,
    ) -> Option<VerificationStatus>;

    fn post_receive<'a>(
        &'a self,
        push: &'a dyn Comparison<'a>,
    ) -> PostReceiveFut<'a>;

    fn auto_init(&self) -> bool;
}
```

`Signer` carries the identifying material mizzle extracted from each
signature: the email from the commit header plus the format-specific
identifier from the signature blob (PGP key id, SSH fingerprint, X.509
subject / SAN). Forges look up keys by whichever of these is meaningful
for their key store — e.g. SSH-only forges may key on fingerprint,
PGP-heavy ones on email.

`SignedIdentity` exposes format-specific identity material so a
Sigstore-aware forge implementing `verify_external` can express
policies like "signer SAN must match `https://accounts.google.com`
issuer plus `@company.com` email" or, for CI, "must be signed by
workflow `release.yml` on branch `main`".

## What lives where

| Concern                          | Owner                                                        |
|----------------------------------|--------------------------------------------------------------|
| Resolving HTTP identity           | Forge handler, before calling `serve_*`                      |
| Resolving SSH identity            | `SshAuth::authorize` in forge code                           |
| Loading branch-protection rules   | Forge — at `RepoAccess` construction, in `authorize_preliminary`, or per-ref via `PushContext` |
| Computing `PushKind` per ref      | Mizzle (via storage backend)                                 |
| Computing `new_commits` per ref   | Mizzle, lazily via `Comparison::new_commits`                 |
| Computing `dropped_commits`       | Mizzle, lazily via `Comparison::dropped_commits`             |
| Computing path-level `RefDiff`    | Mizzle, lazily via `Comparison::ref_diff`                    |
| Extracting commit metadata        | Mizzle (`inspect_ingested`)                                  |
| Reconstructing signed payload     | Mizzle (`inspect.rs`)                                        |
| Signature crypto (PGP/SSH/X.509)  | Mizzle, lazily via `Comparison::verify`                      |
| Resolving keys for a signer       | Forge (`verification_keys`)                                  |
| Sigstore / gitsign verification   | Forge (`verify_external`), reference adapter optional        |
| DCO / sign-off / message regex    | Forge (`authorize_push`, reads `CommitInfo.message`)         |
| Path / size / submodule rules     | Forge (`authorize_push`, reads `Comparison::ref_diff` and `Comparison::read_blob`) |

## Failure model

- **Pack inspection fails** — the entire push is rejected with
  `pack inspection failed: …`. Proceeding without metadata could let a
  crafted pack bypass verification.
- **A signature verifier panics or errors internally** — that commit
  surfaces as `BadSignature` (treat as untrusted) on
  `Comparison::verify`. The forge decides whether to allow.
- **`verification_keys` returns no key for a signer** — that commit
  surfaces as `UnknownKey`. Same handling: the forge decides.
- **`verify_external` returns `None` for an unsupported format** —
  surfaces as `UnsupportedFormat`. The forge decides.
- **A `Comparison` accessor exceeds its configured cap** (e.g. a walk
  hits `max_reachable_commits`) — the accessor returns a structured
  error. The forge can treat this as a policy violation ("push too
  large") or as an infrastructure failure (rejected with a log line).
- **`authorize_push` returns `Err`** — `rollback_ingest` is called,
  every ref in the push receives `ng <refname> <reason>`, no refs are
  updated, no `post_receive` fires. Auth is all-or-nothing for the
  push, not per-ref.

## Cross-references

- [architecture.md](architecture.md) — high-level layers
- [dos-protection.md](dos-protection.md) — limits applied before auth runs
- [auth-implementation-plan.md](auth-implementation-plan.md) —
  staged work plan and example forge rules
