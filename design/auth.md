# Auth

How mizzle hands authorisation decisions back to the embedding forge,
and how commit-signature verification fits into that.

For the wider architecture see [architecture.md](architecture.md).

## Principles

Three rules govern every interface in this document:

1. **Auth never opens the repository.** Everything an authoriser needs to
   make a decision is delivered as plain values on the call.
2. **Construction is where expensive work happens.** Per-call methods are
   value comparisons against data already loaded into `RepoAccess`.
3. **Pack data is staged, not stored, until auth passes.** A rejected push
   leaves no trace in the object store.

## Layers

```
┌──────────────────────────────────────────────────────────────┐
│  Transport     HTTP smart protocol · SSH                     │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Identity     HTTP: forge resolves headers/cookies/tokens    │
│               SSH:  russh accepts all keys, defers to        │
│                     SshAuth::authorize at exec time          │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  RepoAccess   per-request, constructed by forge with all     │
│               permissions pre-resolved                       │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│  Mizzle pipeline                                             │
│   fetch  → list_refs · negotiate · build_pack                │
│   push   → preliminary auth · stage · ingest · inspect ·     │
│            verify signatures · authorize_push · commit       │
└──────────────────────────────────────────────────────────────┘
```

## Identity vs authorisation

Identity is established before mizzle is called.

* **HTTP** — the forge runs in front of mizzle (axum handler, etc.) and
  decides who the caller is from headers, cookies, tokens, or mTLS. The
  outcome is encoded into the `RepoAccess` value passed to `serve_*`.
* **SSH** — russh accepts every public key, then defers to
  `SshAuth::authorize(user, key, repo_path)` at exec time. That single
  call resolves identity *and* loads permissions; the returned
  `RepoAccess` carries both.

Mizzle has no notion of "logged-in user" beyond what the forge baked into
the `RepoAccess`. The trait surface is deliberately silent on identity —
the forge models it however it likes (numeric user id, opaque token,
verified-email set) inside its own `RepoAccess` impl.

## The push pipeline

```
1. read_receive_request           parse ref-update headers
2. authorize_push (preliminary)   refnames + PushKind only, pack=None
3. stage_pack                     stream to a temp file
4. ingest_pack                    write into object store as quarantined data
5. inspect_ingested               extract commit/tag metadata + signatures
6. verify signatures              mizzle resolves keys via RepoAccess,
                                  runs crypto, attaches VerificationStatus
7. compute_push_kind (per ref)    reachability walk → new_commits per ref
8. authorize_push (full)          refs with new_commits + verification
9. update_refs                    on Ok; otherwise rollback_ingest
10. post_receive                  CI triggers, audit log
```

Steps 2 and 8 share the same `authorize_push` method. Step 2 catches
obviously-rejected pushes before any pack data is transferred (`pack` is
`None`); step 8 carries the full per-ref `new_commits` and pack
metadata.

## Committer verification — supported flows

Mizzle ships verifiers for the three commit-signature formats that
account for essentially all real-world signed commits:

| Format        | Verifier                    | Forge supplies        |
|---------------|-----------------------------|-----------------------|
| **OpenPGP**   | `mizzle::verify::pgp`       | armoured public keys  |
| **SSH**       | `mizzle::verify::ssh`       | SSH pubkeys, namespace `git` (per `git config gpg.ssh.allowedSignersFile`) |
| **X.509 / S/MIME (static)** | `mizzle::verify::x509` | trust roots, optional CRL/OCSP policy |

For each new commit and tag in a push, mizzle:

1. Detects the signature format from the header bytes.
2. Collects `(email, format)` pairs into a single batch.
3. Calls `RepoAccess::verification_keys(&signers)` once — the forge
   returns its pre-loaded map of candidate keys.
4. Runs the matching verifier against the commit's reconstructed signed
   payload.
5. Attaches a `VerificationStatus` to the `CommitInfo` that flows into
   `authorize_push`.

The forge never sees raw signature bytes or signed payloads on the
common path — the trait surface stays narrow and the bandwidth cost of
shipping kilobytes-per-commit across the auth boundary is paid in-process
once, by mizzle, against keys the forge already had cached.

### Sigstore / gitsign — the escape hatch

Sigstore-signed commits (gitsign) cannot be verified against a static
key. The signing certificate is short-lived (10 minutes), issued by
Fulcio against an OIDC identity, and audited via a Rekor transparency
log entry whose Signed Entry Timestamp is embedded in the signature.
Verification needs:

- Fulcio root CA + Rekor public key (static trust anchors)
- the certificate chain extracted from the CMS blob
- the SET / inclusion proof, also in the CMS blob
- a policy mapping OIDC issuer + SAN → forge identity

This is more than mizzle should bake in. Forges that want gitsign
implement `RepoAccess::verify_external`, which receives the
`(format, raw_signature, signed_payload)` tuple and returns a
`VerificationStatus` directly. Mizzle's own verifier is bypassed for
that commit.

A reference `mizzle-sigstore` adapter crate may live alongside mizzle
later, but it is not part of the core library.

### Out of scope

- **Long-lived X.509 with custom revocation infrastructure** —
  forge-implementable via `verify_external`.
- **Bespoke signature formats in non-`gpgsig` headers** — forges that
  ship custom git clients can use `verify_external`.
- **Timestamping authorities other than Rekor** — `verify_external`.

## Trait surface

```rust
pub trait RepoAccess {
    type RepoId: …;
    fn repo_id(&self) -> &Self::RepoId;

    /// Called twice: once before pack ingest (pack=None, new_commits empty),
    /// once after (pack=Some, new_commits populated, verification filled in).
    fn authorize_push(
        &self,
        refs: &[PushRef<'_>],
        pack: Option<&PackMetadata>,
    ) -> Result<(), String> { Ok(()) }

    /// Resolve candidate verification keys for a batch of (email, format)
    /// signers. Default: no keys → every signed commit verifies as
    /// `UnknownKey`. Forges that don't enforce signatures leave it alone.
    fn verification_keys(
        &self,
        _signers: &[Signer<'_>],
    ) -> HashMap<SignerKey, Vec<VerificationKey>> { HashMap::new() }

    /// Sigstore / gitsign / custom escape hatch. Called per-signature for
    /// formats mizzle does not natively verify, or whenever the forge wants
    /// to override mizzle's verdict. Default: not handled, falls back to
    /// `UnsupportedFormat`.
    fn verify_external(
        &self,
        _sig: &ExternalSig<'_>,
    ) -> Option<VerificationStatus> { None }

    fn post_receive<'a>(&'a self, _refs: &'a [PushRef<'a>])
        -> PostReceiveFut<'a> { … }

    fn auto_init(&self) -> bool { false }
}
```

```rust
pub struct PushRef<'a> {
    pub refname: &'a str,
    pub kind: PushKind,                     // Create | Delete | FastForward | ForcePush
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    /// Commits introduced on this ref by this push, parent-first.
    /// Already excludes commits reachable from any pre-existing ref or
    /// from another ref earlier in this same atomic push.
    pub new_commits: &'a [CommitInfo],
}

pub struct CommitInfo {
    pub oid: ObjectId,
    pub parents: Vec<ObjectId>,
    pub tree: ObjectId,
    pub author: Identity,
    pub committer: Identity,
    pub author_time: Time,
    pub committer_time: Time,
    pub message: BString,
    pub verification: VerificationStatus,
}

pub enum VerificationStatus {
    Unsigned,
    Verified { identity: SignedIdentity, format: SignatureFormat },
    BadSignature,
    UnknownKey,
    UnsupportedFormat,
}

pub enum SignedIdentity {
    Pgp  { key_id: KeyId,        email: BString },
    Ssh  { fingerprint: String,  principal: BString },
    X509 { subject_email: BString, issuer: BString,
           sans: Vec<BString>,   extensions: Vec<X509Extension> },
}
```

`SignedIdentity::X509` carries SANs and extensions so a Sigstore-aware
forge implementing `verify_external` can express policies like
"signer SAN must match `https://accounts.google.com` issuer plus
`@company.com` email" or, for CI, "must be signed by workflow `release.yml`
on branch `main`".

## What lives where

| Concern                          | Owner                                   |
|----------------------------------|-----------------------------------------|
| Resolving HTTP identity           | Forge handler, before calling `serve_*` |
| Resolving SSH identity            | `SshAuth::authorize` in forge code      |
| Loading branch-protection rules   | Forge, into `RepoAccess` at construction |
| Computing `PushKind` per ref      | Mizzle (via storage backend)            |
| Computing `new_commits` per ref   | Mizzle (gitoxide reachability walk)     |
| Extracting commit metadata        | Mizzle (`inspect_ingested`)             |
| Reconstructing signed payload     | Mizzle (`inspect.rs`)                   |
| Signature crypto (PGP/SSH/X.509)  | Mizzle (`mizzle::verify`)               |
| Resolving keys for an email       | Forge (`verification_keys`)             |
| Sigstore / gitsign verification   | Forge (`verify_external`), reference adapter optional |
| DCO / sign-off / message regex    | Forge (`authorize_push`, reads `CommitInfo.message`) |
| Path / size rules                 | Forge (`authorize_push`, reads `PackMetadata`) |

## Failure model

- **Pack inspection fails** — the entire push is rejected with
  `pack inspection failed: …`. Proceeding without metadata could let a
  crafted pack bypass verification.
- **A signature verifier panics or errors internally** — that commit is
  marked `BadSignature` (treat as untrusted), the push continues to
  `authorize_push`. The forge decides whether to allow.
- **`verification_keys` returns no key for a signer** — that commit is
  marked `UnknownKey`. Same handling: the forge decides.
- **`verify_external` returns `None` for an unsupported format** — that
  commit is marked `UnsupportedFormat`. The forge decides.
- **`authorize_push` returns `Err`** — `rollback_ingest` is called,
  every ref in the push receives `ng <refname> <reason>`, no refs are
  updated, no `post_receive` fires.

## Cross-references

- [architecture.md](architecture.md) — high-level layers
- [dos-protection.md](dos-protection.md) — limits applied before auth runs
- [auth-implementation-plan.md](auth-implementation-plan.md) —
  implementation plan and example forge rules
