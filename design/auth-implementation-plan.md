# Auth implementation plan

Companion to [auth.md](auth.md). Covers the staged work to extend
mizzle's `RepoAccess` surface with the typed `PushContext`, the
`Comparison` handle, signature-verification plumbing, and the example
rules that prove the surface is expressive enough.

## Staging

Each phase is independently shippable and testable. Earlier phases
unblock the more advanced rules later. Only the high-level shape and
acceptance criteria are pinned here; specific crates, file paths, and
feature names are implementation details to be settled at PR time.

### Phase A — `Comparison` handle and typed `PushContext`

**Goal:** the forge's `authorize_push` receives a lazy handle that
exposes per-ref deltas, reachability walks, ref-diffs, and pack
contents on demand. State resolved by the preliminary call flows
through a typed value, not interior mutability.

Trait shape changes:

* `RepoAccess` gains an associated `PushContext: Default` type.
* `authorize_preliminary(refs) -> Result<Self::PushContext, String>`
  replaces the preliminary `authorize_push(_, None)` call. Default
  impl returns `Ok(Default::default())`.
* `authorize_push(ctx, push: &dyn Comparison<'_>) -> Result<(), String>`
  replaces the full call.
* `post_receive` takes the same `Comparison` handle for symmetry.

`CommitInfo` and `TagInfo` are restructured at the same time, since
the rules that consume the `Comparison` accessors need real metadata:
`oid`, `parents`, `tree`, `author` and `committer` (each with name +
email), author/committer times, and message. No public signature
field on `CommitInfo` — signature handling is lazy and lives behind
`Comparison::verify` (see Phase B). This means commits whose
signatures are never inspected pay zero verification cost, and
nothing carries kilobytes of payload bytes around the trait surface
unnecessarily.

The `Comparison` trait itself exposes:

* `refs() -> &[PushRef]` with `refname`, `kind`, `old_oid`, `new_oid`.
* `new_commits(ref)` — reachability walk from `new_oid` excluding
  pre-existing refs and earlier refs in the same push, parent-first.
  Cached. Subject to a configurable cap; over-cap returns a structured
  error the forge can map to a rejection message.
* `dropped_commits(ref)` — same shape, walking from `old_oid`
  excluding `new_oid`. Required for force-push loss-prevention rules.
* `ref_diff(ref)` — added / modified / removed paths with mode and
  oid, computed against the parent tree.
* `verify(commit)` and `verify_tag(tag)` — lazy signature
  verification (see Phase B); stub that returns `UnsupportedFormat`
  until verifiers land.
* `read_blob(oid, cap)` — bytes for a blob in the staged pack, capped
  to discourage forges from grepping gigabytes.
* `pack_metadata()` and `tags()` — direct passthroughs to the
  inspection results.

Backend additions:

* `reachable_excluding(repo, from: &[ObjectId], excluding: &[ObjectId], cap)`
  returning a bounded iterator. Used by both `new_commits` and
  `dropped_commits` (with arguments swapped).
* `tree_diff(repo, parent_tree, child_tree)` returning the per-path
  delta. Used by `ref_diff`.

Multi-ref dedup: when two refs in the same push both introduce the
same commit, it is assigned to the ref earliest in push order
(documented and tested).

Tests in `tests/auth.rs`:

* extend `inspect_pack_extracts_commit_metadata` to assert the new
  structured fields on `CommitInfo` and `TagInfo`,
* `new_commits_excludes_existing_branches` — push a branch whose tip
  is reachable from an existing ref; assert empty,
* `new_commits_topological_order` — assert parent-first ordering,
* `multi_ref_push_dedupes_new_commits` — pushing two refs that both
  introduce the same commit assigns it to the push-order-earlier ref,
* `dropped_commits_on_force_push` — force-push that rewinds three
  commits; assert they appear on `dropped_commits` for that ref,
* `ref_diff_reports_added_modified_removed_paths`,
* `comparison_accessor_cap_returns_structured_error`.

### Phase B — Verification plumbing (no verifiers yet)

**Goal:** wire `verification_keys` and `verify_external` through the
`Comparison::verify` accessor, and reconstruct the signed payload
lazily at verify time. Without any native verifier present, every
signed commit verifies as `UnknownKey` (or `UnsupportedFormat` for
formats with no registered verifier).

Changes:

* Add `verification_keys`, `verify_external`, `Signer`, `SignerKey`,
  `VerificationKey`, `ExternalSig`, `VerificationStatus`,
  `SignedIdentity` to the public surface (all `#[non_exhaustive]`
  where forward-compat matters).
* Internal verifier-dispatch module with a placeholder that returns
  `UnknownKey` for everything until Phase C.
* `Comparison::verify` is now functional: on first access for a
  commit, mizzle re-reads the commit object from the staged pack,
  produces the canonical signed payload by stripping the `gpgsig`
  header (and any continuation lines), runs the dispatch, then offers
  the result to `verify_external` for override. Result is cached on
  the `Comparison`.

Lazy signed-payload reconstruction matters: most commits in most
pushes will never have their signatures inspected, so paying the
gpgsig-strip + parse cost up front during `inspect_ingested` would
be waste.

Tests:

* `signed_payload_strips_gpgsig_header` — fixture PGP-signed commit;
  assert the canonical payload round-trips byte-identical to the
  commit object minus the `gpgsig` header (and continuation lines),
* `signed_tag_payload_strips_signature` — same for an annotated
  signed tag,
* `signed_commit_without_keys_is_unknown_key` — push a real
  PGP-signed commit, `verification_keys` returns empty, assert
  `Comparison::verify` returns `UnknownKey`,
* `verify_external_overrides_status` — forge that always returns
  `Verified` from `verify_external`; assert it wins over the native
  default,
* `verify_external_none_falls_through_to_native` — forge returns
  `None`; assert native verdict stands.

### Phase C — Real verifiers

**Goal:** mizzle natively verifies PGP, SSH, and static X.509 commit
signatures.

Per-format verifiers wired into the dispatch, each behind its own
opt-out Cargo feature so forges that only handle one format don't
pull in the others. Specific crate choices and feature names are PR-time
decisions; constraints are:

* PGP: stable Rust crate that can verify against an armoured public
  key with cleartext / detached payloads and report a key id.
* SSH: namespace `git`, matching `gpg.ssh.allowedSignersFile`
  semantics.
* X.509: chain validation against trust roots; CRL/OCSP gated behind
  a non-default feature (network access in the verifier is a
  significant policy change).

Tests: covered by Phase D examples.

### Phase D — Example forge rules as integration tests

These are the proof that the trait surface is expressive enough. Each
is a one-screen `RepoAccess` impl plus integration tests against both
storage backends (using `dual_backend_access_test!`).

See [§ Example rules](#example-rules) below.

### Phase E — Reference Sigstore adapter (out of tree)

A separate crate alongside mizzle wraps a Sigstore implementation to
provide `verify_external` for X.509 CMS signatures with embedded Rekor
SETs. Defers all the Fulcio root + Rekor key plumbing out of the core
library. Ship later; not blocking.

---

## Example rules

Each example is a one-screen `RepoAccess` impl plus an integration test
that exercises it end-to-end through `git push` against a live mizzle
server. All examples live in `mizzle/tests/auth_rules.rs` and use the
existing `axum_access_server_with_backend` helper from `tests/common`.

The examples below show the `Comparison` handle pattern from Phase A.
They are written for clarity, not literal copy-paste — actual code may
differ once the trait stabilises.

### 1. Refs allow-list

The simplest possible rule. No commit inspection.

```rust
struct OnlyHeads { repo: PathBuf }

impl RepoAccess for OnlyHeads {
    type RepoId = PathBuf;
    type PushContext = ();
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_preliminary(&self, refs: &[PushRef<'_>])
        -> Result<(), String>
    {
        for r in refs {
            if !r.refname.starts_with("refs/heads/") {
                return Err(format!("pushes to {} are not allowed", r.refname));
            }
        }
        Ok(())
    }
}
```

The decision is made at the preliminary stage, so the pack is never
transferred. The default `authorize_push` is a no-op.

Tests:

* push to `refs/heads/feature` → success
* push to `refs/internal/secret` → rejected with the expected message

### 2. Protected branches: no force-push, no delete

```rust
struct ProtectMain { repo: PathBuf }

impl RepoAccess for ProtectMain {
    type RepoId = PathBuf;
    type PushContext = ();
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_preliminary(&self, refs: &[PushRef<'_>])
        -> Result<(), String>
    {
        for r in refs {
            if r.refname == "refs/heads/main" {
                match r.kind {
                    PushKind::ForcePush => return Err("main is protected: no force-push".into()),
                    PushKind::Delete    => return Err("main is protected: no delete".into()),
                    _ => {}
                }
            }
        }
        Ok(())
    }
}
```

Tests:

* fast-forward to `main` → success
* `git push --force` to `main` → rejected
* `git push :main` → rejected
* force-push to `feature` → success (rule scoped to `main`)

### 3. Committer email must match the authenticated pusher

The most common "username matches push auth" rule. The forge baked the
authenticated user's verified emails into `RepoAccess` at construction.

```rust
struct CommitterMatchesPusher {
    repo: PathBuf,
    verified_emails: HashSet<String>,
}

impl RepoAccess for CommitterMatchesPusher {
    type RepoId = PathBuf;
    type PushContext = ();
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>)
        -> Result<(), String>
    {
        for r in push.refs() {
            for c in push.new_commits(r)? {
                let email = c.committer.email.to_str().unwrap_or("");
                if !self.verified_emails.contains(email) {
                    return Err(format!(
                        "commit {} has committer {} not in your verified emails",
                        c.oid, email,
                    ));
                }
            }
        }
        Ok(())
    }
}
```

Tests:

* push two commits both committed by `alice@co` with `alice@co` in the
  verified set → success
* push a commit committed by `mallory@evil` → rejected, push aborted
  before refs move
* delete-only push → success (`new_commits` empty)

### 4. DCO sign-off required

```rust
fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>)
    -> Result<(), String>
{
    for r in push.refs() {
        for c in push.new_commits(r)? {
            let trailer = format!(
                "Signed-off-by: {} <{}>",
                c.author.name, c.author.email,
            );
            if !c.message.windows(trailer.len()).any(|w| w == trailer.as_bytes()) {
                return Err(format!(
                    "commit {} missing DCO sign-off matching its author", c.oid,
                ));
            }
        }
    }
    Ok(())
}
```

Tests:

* commit with matching `Signed-off-by:` trailer → success
* commit with no trailer → rejected
* commit with trailer for a different identity → rejected

### 5. Signed commits required on `main`, signed *by* the pusher

The forge keeps a map `email → Vec<VerificationKey>` derived from user
profile keys, plus the authenticated pusher's email. Mizzle does the
crypto.

```rust
struct SignedByPusher {
    repo: PathBuf,
    pusher_email: String,
    keys: HashMap<String, Vec<VerificationKey>>,
}

impl RepoAccess for SignedByPusher {
    type RepoId = PathBuf;
    type PushContext = ();
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn verification_keys(&self, signers: &[Signer<'_>])
        -> HashMap<SignerKey, Vec<VerificationKey>>
    {
        signers.iter().filter_map(|s| {
            self.keys.get(s.email).map(|ks| (s.key(), ks.clone()))
        }).collect()
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>)
        -> Result<(), String>
    {
        for r in push.refs() {
            if r.refname != "refs/heads/main" { continue; }
            for c in push.new_commits(r)? {
                match push.verify(c) {
                    VerificationStatus::Verified { identity, .. }
                        if identity.matches_email(&self.pusher_email) => {}
                    other => return Err(format!(
                        "commit {} on main: {:?}; must be signed by {}",
                        c.oid, other, self.pusher_email,
                    )),
                }
            }
        }
        Ok(())
    }
}
```

This is the rule most "signed commits" forges actually want — signed
*and* signed by the user pushing, not anyone in the keyring. Mizzle's
verification runs lazily here: only commits on `main` trigger crypto.

Tests (one per signature format, all using fixture keys committed
under `tests/fixtures/keys/`):

* `signed_pgp_commit_to_main_succeeds`
* `signed_ssh_commit_to_main_succeeds`
* `signed_x509_commit_to_main_succeeds`
* `unsigned_commit_to_main_rejected`
* `pgp_commit_signed_by_other_user_rejected`
* `pgp_commit_with_unknown_key_rejected`
* `tampered_pgp_signature_rejected`
* `unsigned_commit_to_feature_branch_succeeds` (rule scoped to `main`)

These cover the full inspect → verify → authorize pipeline for all
three native formats, with and without pusher-binding.

### 6. Topology rules (no-ff merges only on `main`)

Demonstrates use of `CommitInfo.parents` for merge-shape policy.

```rust
fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>)
    -> Result<(), String>
{
    for r in push.refs() {
        if r.refname != "refs/heads/main" { continue; }
        for c in push.new_commits(r)? {
            if c.parents.len() < 2 {
                return Err(format!(
                    "commit {} on main must be a merge (no fast-forward)",
                    c.oid,
                ));
            }
        }
    }
    Ok(())
}
```

The opposite policy (linear history only — `parents.len() == 1`) and
"first-parent must be `old_oid`" are one-line variants.

Tests:

* fast-forward of a single non-merge commit → rejected
* merge commit (two parents) → success
* octopus merge (three parents) → success unless additionally capped

### 7. Force-push loss-prevention

Demonstrates `dropped_commits`. Reject force-pushes that would drop a
signed commit, or that drop more than N commits.

```rust
fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>)
    -> Result<(), String>
{
    for r in push.refs() {
        let dropped = push.dropped_commits(r)?;
        if dropped.len() > 50 {
            return Err(format!(
                "force-push to {} would drop {} commits (limit 50)",
                r.refname, dropped.len(),
            ));
        }
        for c in dropped {
            if matches!(push.verify(c), VerificationStatus::Verified { .. }) {
                return Err(format!(
                    "force-push to {} would drop signed commit {}",
                    r.refname, c.oid,
                ));
            }
        }
    }
    Ok(())
}
```

Tests:

* force-push that drops 3 unsigned commits → success
* force-push that drops 60 commits → rejected
* force-push that drops a signed commit → rejected
* fast-forward (no dropped commits) → success

### 8. Path-glob block: no changes under `migrations/` without a flag commit

Demonstrates `Comparison::ref_diff` for path-level policy.

```rust
fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>)
    -> Result<(), String>
{
    for r in push.refs() {
        for c in push.new_commits(r)? {
            if c.message.starts_with(b"migration: ") { continue; }
            let diff = push.ref_diff(r)?;
            if diff.touched_paths().any(|p| p.starts_with(b"migrations/")) {
                return Err(format!(
                    "commit {} touches migrations/ without `migration:` prefix",
                    c.oid,
                ));
            }
        }
    }
    Ok(())
}
```

Tests:

* commit touching `src/foo.rs` → success
* commit touching `migrations/001.sql` with `migration:` prefix → success
* commit touching `migrations/001.sql` without prefix → rejected

### 9. Submodule URL allow-list

Demonstrates `Comparison::read_blob` for content-inspection policy.
The forge reads any `.gitmodules` blob added or modified in this
push and rejects URLs outside its allow-list.

```rust
fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>)
    -> Result<(), String>
{
    for r in push.refs() {
        let diff = push.ref_diff(r)?;
        for entry in diff.added_or_modified() {
            if entry.path != b".gitmodules" { continue; }
            let bytes = push.read_blob(entry.oid, 64 * 1024)
                .ok_or_else(|| ".gitmodules too large".to_string())?;
            for url in parse_submodule_urls(bytes) {
                if !self.submodule_allowlist.is_match(&url) {
                    return Err(format!("submodule URL not allowed: {url}"));
                }
            }
        }
    }
    Ok(())
}
```

Tests:

* `.gitmodules` adding `https://github.com/acme/lib` (allowed) → success
* `.gitmodules` adding `https://evil.example/exfil` → rejected
* push with no `.gitmodules` change → success

### 10. Sigstore / gitsign verification via `verify_external`

```rust
fn verify_external(&self, sig: &ExternalSig<'_>) -> Option<VerificationStatus> {
    if sig.format != SignatureFormat::X509Cms { return None; }
    match sigstore_verify(sig, &self.fulcio_roots, &self.rekor_pubkey) {
        Ok(ident) if self.allowed_oidc_emails.contains(&ident.email) =>
            Some(VerificationStatus::Verified {
                identity: SignedIdentity::X509 { /* … */ },
                format:   SignatureFormat::X509Cms,
            }),
        Ok(_)  => Some(VerificationStatus::UnknownKey),
        Err(_) => Some(VerificationStatus::BadSignature),
    }
}
```

The `authorize_push` body is the same as example 5. Real Sigstore
tests live in the out-of-tree adapter crate. Keep a smoke test in
`tests/auth_rules.rs` using a stub `verify_external` that always
returns `Verified` to prove the dispatch wiring works.

---

## Test scaffolding

* New helper module `tests/common/sig_fixtures.rs` exposing keypair
  generators (PGP, SSH, X.509) persisted under `tests/fixtures/keys/`
  via a build script, plus a `commit_signed_with` helper.
* X.509 fixtures need an out-of-band signer (git itself does not sign
  X.509 natively); plan to use the same crate the verifier uses, or
  shell out to a fixed-version external tool from the build script.
* `dual_backend_access_test!` already covers the both-backends matrix.
* Fixtures are deterministic (fixed timestamps via `GIT_*_DATE` env,
  as in the existing `common::run_git`).

## Done criteria

Phases A–D are complete when:

1. The `RepoAccess` trait surface from auth.md is in place, including
   `PushContext` and the `Comparison` handle.
2. Every example rule (1–9) ships as a `RepoAccess` impl in
   `tests/auth_rules.rs` with passing tests against both backends.
3. Example 10's smoke test passes, demonstrating `verify_external`
   integration without requiring real Sigstore infrastructure in the
   core test suite.
4. `cargo test --all-features` is green; `cargo fmt` clean; rustdoc
   builds with the new types.
5. `design/auth.md` cross-references every example by section number.
