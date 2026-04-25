# Auth implementation plan

Companion to [auth.md](auth.md). Covers the staged work to extend
mizzle's `RepoAccess` surface with per-ref new-commit lists,
signature-verification plumbing, and the example/test rules that prove
the surface is expressive enough.

## Staging

Each phase is independently shippable and testable. Earlier phases
unblock the more advanced rules later.

### Phase A — Enrich pack inspection

**Goal:** `inspect_ingested` returns structured commit metadata plus the
reconstructed signed payload, so later phases can verify without
re-reading objects.

Changes:

* `mizzle/src/backend/mod.rs` — restructure `CommitInfo`:
    * `oid`, `parents: Vec<ObjectId>`, `tree: ObjectId`
    * `author: Identity { name, email }`, `committer: Identity`
    * `author_time: Time`, `committer_time: Time`
    * `message: BString`
    * `signature: Option<RawSignature>` — `{ format, raw_bytes, signed_payload }`
* Same shape for `TagInfo`.
* `mizzle/src/inspect.rs` — emit the new fields. The signed payload is the
  canonical commit bytes with the `gpgsig` (and any continuation) header
  removed; produce it once during parsing.
* `signature` stays internal until Phase C; do not expose
  `RawSignature` on `RepoAccess` yet.

Tests:

* Extend `inspect.rs::tests::inspect_pack_extracts_commit_metadata` to
  assert the new fields.
* New test using a fixture PGP-signed commit (committed under
  `tests/fixtures/`): assert `signature.raw_bytes` decodes as an OpenPGP
  packet stream and `signed_payload` round-trips byte-identical to the
  commit object minus the `gpgsig` header.

### Phase B — `PushRef` with new-commits

**Goal:** authoriser receives, per ref, the list of `CommitInfo` it is
expected to apply policy to, with no graph-walking on the auth side.

Changes:

* `mizzle-proto/src/types.rs` — extend `PushRef`:
    * `old_oid: ObjectId`
    * `new_oid: ObjectId`
    * `new_commits: &'a [CommitInfo]`
* `mizzle/src/serve.rs` — between `inspect_ingested` and the full
  `authorize_push`, run a reachability walk:
    * for each ref in topological push order, collect commits reachable
      from `new_oid` minus those reachable from any pre-existing ref or
      from any earlier ref's `new_oid` in the same push
    * resolve each oid in the resulting set against the `PackMetadata`
      to attach the `CommitInfo` payload
* `StorageBackend` gets one new method:
  `reachable_excluding(repo, from: &[ObjectId], excluding: &[ObjectId])
  -> impl Iterator<Item = ObjectId>`. Both backends implement via
  gitoxide; this is the same walk used to compute `PushKind`.

Tests:

* `tests/auth.rs` — `new_commits_excludes_existing_branches`: push a
  branch whose tip is reachable from an existing ref; assert
  `new_commits` is empty.
* `tests/auth.rs` — `new_commits_topological_order`: assert parent-first
  ordering.
* `tests/auth.rs` — `multi_ref_push_dedupes_new_commits`: pushing two
  refs that both introduce the same commit assigns it to the
  topologically-earlier ref only.

### Phase C — Verification plumbing (no verifiers yet)

**Goal:** wire `verification_keys` and `verify_external` through the
serve pipeline; mark every signed commit as `UnknownKey` until Phase D
adds real verifiers.

Changes:

* `mizzle/src/traits.rs` — add `verification_keys`, `verify_external`,
  `Signer`, `SignerKey`, `VerificationKey`, `ExternalSig`,
  `VerificationStatus`, `SignedIdentity`.
* `mizzle/src/verify/mod.rs` (new) — define `Verifier` trait and the
  `VerificationStatus` enum; placeholder dispatch that returns
  `UnknownKey` for everything except `UnsupportedFormat`.
* `mizzle/src/serve.rs` — after `inspect_ingested`, batch signers, call
  `verification_keys`, dispatch each signature through `verify::run` or
  `verify_external`, attach status to each `CommitInfo`.

Tests:

* `tests/auth.rs` — `signed_commit_without_keys_is_unknown_key`: push a
  commit with a real PGP signature, `verification_keys` returns empty,
  assert `VerificationStatus::UnknownKey` reaches `authorize_push`.
* `tests/auth.rs` — `verify_external_overrides_status`: forge that
  always returns `Verified` from `verify_external`; assert it wins over
  the default `UnsupportedFormat`.

### Phase D — Real verifiers

**Goal:** mizzle natively verifies PGP, SSH, and static X.509 commit
signatures.

Changes:

* `mizzle/src/verify/pgp.rs` — sequoia-openpgp.
* `mizzle/src/verify/ssh.rs` — `ssh-key`, namespace `git`.
* `mizzle/src/verify/x509.rs` — `rustls-pki-types` + `webpki` for chain
  validation; CRL/OCSP gated behind a non-default feature.
* Wire each into the `verify::run` dispatch from Phase C.
* Add the `mizzle-verify-pgp`, `mizzle-verify-ssh`, `mizzle-verify-x509`
  Cargo features so users can drop verifiers they don't want — keeps
  binary size down and avoids pulling in heavyweight deps for forges
  that only do SSH-signed commits.

Tests: covered by Phase E examples.

### Phase E — Example forge rules as integration tests

These are the proof that the trait surface is expressive enough. Each
is one `RepoAccess` impl plus one or two integration test cases against
both backends (using `dual_backend_access_test!`).

See [§ Example rules](#example-rules) below.

### Phase F — Reference Sigstore adapter (out of tree)

A `mizzle-sigstore` crate alongside `mizzle` (not part of the core
library) that wraps the [`sigstore`](https://crates.io/crates/sigstore)
crate to implement `verify_external` for X.509 CMS signatures with
embedded Rekor SETs. Defers all the Fulcio root + Rekor key plumbing out
of the core library. Ship later; not blocking.

---

## Example rules

Each example is a one-screen `RepoAccess` impl that demonstrates a
typical forge policy, plus the integration test that exercises it
end-to-end through `git push` against a live mizzle server.

All examples live in `mizzle/tests/auth_rules.rs` and use the existing
`axum_access_server_with_backend` helper from `tests/common`.

### 1. Refs allow-list

The simplest possible rule. No commit inspection.

```rust
struct OnlyHeads { repo: PathBuf }

impl RepoAccess for OnlyHeads {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        _pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
        for r in refs {
            if !r.refname.starts_with("refs/heads/") {
                return Err(format!("pushes to {} are not allowed", r.refname));
            }
        }
        Ok(())
    }
}
```

Tests:

* push to `refs/heads/feature` → success
* push to `refs/internal/secret` → rejected with the expected message

### 2. Protected branches: no force-push, no delete

```rust
struct ProtectMain { repo: PathBuf }

impl RepoAccess for ProtectMain {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        _pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
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
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
        if pack.is_none() { return Ok(()); }    // preliminary call
        for r in refs {
            for c in r.new_commits {
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
* push that introduces no commits (delete-only) → success
* preliminary call (`pack=None`) does not reject (`new_commits` is
  empty at that stage)

### 4. DCO sign-off required

```rust
struct RequireDco { repo: PathBuf }

impl RepoAccess for RequireDco {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
        if pack.is_none() { return Ok(()); }
        for r in refs {
            for c in r.new_commits {
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
}
```

Tests:

* commit with matching `Signed-off-by:` trailer → success
* commit with no trailer → rejected
* commit with trailer for a different identity → rejected

### 5. Signed commits required on `main` (PGP/SSH/X.509)

The forge keeps a map `email → Vec<VerificationKey>` derived from user
profile keys. Mizzle does the crypto.

```rust
struct SignedMain {
    repo: PathBuf,
    keys: HashMap<String, Vec<VerificationKey>>,
}

impl RepoAccess for SignedMain {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn verification_keys(
        &self,
        signers: &[Signer<'_>],
    ) -> HashMap<SignerKey, Vec<VerificationKey>> {
        signers.iter().filter_map(|s| {
            self.keys.get(s.email).map(|ks| (s.key(), ks.clone()))
        }).collect()
    }

    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
        if pack.is_none() { return Ok(()); }
        for r in refs {
            if r.refname != "refs/heads/main" { continue; }
            for c in r.new_commits {
                if !matches!(c.verification, VerificationStatus::Verified { .. }) {
                    return Err(format!(
                        "commit {} on main is {:?}; signed commit required",
                        c.oid, c.verification,
                    ));
                }
            }
        }
        Ok(())
    }
}
```

Tests (one per signature format, all using fixture keys committed under
`tests/fixtures/keys/`):

* `signed_pgp_commit_to_main_succeeds`
* `signed_ssh_commit_to_main_succeeds`
* `signed_x509_commit_to_main_succeeds`
* `unsigned_commit_to_main_rejected`
* `pgp_commit_with_unknown_key_rejected` (forge omits the key from
  `verification_keys`)
* `tampered_pgp_signature_rejected` (fixture commit edited after signing)
* `unsigned_commit_to_feature_branch_succeeds` (rule scoped to `main`)

These are the strongest tests in the suite — they cover the full
inspect → verify → authorize pipeline for all three native formats.

### 6. Path-glob block: no changes under `/migrations` without a flag commit

Demonstrates use of `PackMetadata` for blob/path policy. This rule needs
a per-ref diff which Phase B does not include — the example uses
`PackMetadata.objects` (already exposed) plus the parent-tree lookup
that `CommitInfo.tree` enables.

```rust
struct GuardMigrations { repo: PathBuf }

impl RepoAccess for GuardMigrations {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf { &self.repo }

    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        pack: Option<&PackMetadata>,
    ) -> Result<(), String> {
        let Some(pack) = pack else { return Ok(()); };
        for r in refs {
            for c in r.new_commits {
                if c.message.starts_with(b"migration: ") { continue; }
                let touched = paths_changed_for(c, pack)?;   // helper, see below
                if touched.iter().any(|p| p.starts_with(b"migrations/")) {
                    return Err(format!(
                        "commit {} touches migrations/ without `migration:` prefix",
                        c.oid,
                    ));
                }
            }
        }
        Ok(())
    }
}
```

`paths_changed_for` walks `c.tree` against `c.parents[0]`'s tree using
the trees already present in `PackMetadata.objects`. (If the parent tree
isn't in the pack — which it usually won't be, having been part of
earlier history — this rule needs the future `RefDiff` summary from
auth.md §4d. Mark this example as **stub-only until Phase G**.)

Tests:

* commit touching `src/foo.rs` → success
* commit touching `migrations/001.sql` with `migration:` prefix → success
* commit touching `migrations/001.sql` without prefix → rejected

### 7. Sigstore / gitsign verification via `verify_external`

```rust
struct GitsignedMain {
    repo: PathBuf,
    fulcio_roots: TrustStore,
    rekor_pubkey: PublicKey,
    allowed_oidc_emails: HashSet<String>,
}

impl RepoAccess for GitsignedMain {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf { &self.repo }

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

    fn authorize_push(/* same as example 5 */) { … }
}
```

Tests live in the future `mizzle-sigstore` crate, not in core mizzle.
Keep a **smoke test** in `tests/auth_rules.rs` using a stub
`verify_external` that always returns `Verified` to prove the dispatch
wiring works.

---

## Test scaffolding

* New helper module `tests/common/sig_fixtures.rs` exposing:
    * `pgp_keypair() -> (SecretKey, PublicKey)` — generated once via a
      build script, persisted under `tests/fixtures/keys/`
    * `ssh_keypair() -> (SecretKey, PublicKey)`
    * `x509_keypair() -> (SecretKey, Certificate, RootCa)`
    * `commit_signed_with(work_dir, key, paths, message)` — drives `git
      commit -S` with the fixture key configured, returning the new
      commit oid
* `dual_backend_access_test!` already covers the both-backends matrix.
* Fixtures are deterministic (fixed timestamps via `GIT_*_DATE` env, as
  in the existing `common::run_git`).

## Done criteria

Phases A–E are complete when:

1. The full `RepoAccess` trait surface from auth.md is in place.
2. Every example rule (1–6) ships as a `RepoAccess` impl in
   `tests/auth_rules.rs` with passing tests against both backends.
3. Example 7's smoke test passes, demonstrating `verify_external`
   integration without requiring real Sigstore infrastructure in the
   core test suite.
4. `cargo test --all-features` is green; `cargo fmt` clean; rustdoc
   builds with the new types.
5. `design/auth.md` cross-references every example by section number.
