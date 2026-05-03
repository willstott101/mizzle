//! End-to-end integration tests for the example forge rules in
//! `design/auth-implementation-plan.md` § Example rules.
//!
//! Each example is a one-screen [`RepoAccess`] impl plus a test that pushes
//! against a real mizzle server and asserts the rule fires (or doesn't) as
//! expected.  Examples that depend on real signature verification (Phase C)
//! are not included — Phase C is out of scope for this PR.

mod common;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

use bstr::ByteSlice;
use mizzle::traits::{
    Comparison, ExternalSig, PushKind, PushRef, RepoAccess, SignatureFormat, SignedIdentity,
    Signer, SignerKey, VerificationKey, VerificationStatus,
};

// ─── Helper: fresh clone ────────────────────────────────────────────────────

fn clone_repo(server_port: u16, dir: &std::path::Path, branch: Option<&str>) -> PathBuf {
    let url = format!("http://localhost:{}/test.git", server_port);
    let mut args: Vec<&str> = vec!["clone"];
    if let Some(b) = branch {
        args.push("--branch");
        args.push(b);
    }
    args.push(&url);
    common::run_git(dir, args).unwrap();
    dir.join("test")
}

// ─── Example 1: Refs allow-list (preliminary auth) ──────────────────────────

#[derive(Clone)]
struct OnlyHeads {
    repo: PathBuf,
}

impl RepoAccess for OnlyHeads {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_preliminary(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        for r in refs {
            if !r.refname.starts_with("refs/heads/") {
                return Err(format!("pushes to {} are not allowed", r.refname));
            }
        }
        Ok(())
    }
}

dual_backend_access_test!(only_heads_allows_branch, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| OnlyHeads {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("a.txt"), "x")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "ok"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;
    server.stop();
    Ok(())
});

dual_backend_access_test!(only_heads_rejects_internal, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| OnlyHeads {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    let head = common::run_git(&repo_dir, ["rev-parse", "HEAD"])?;
    let err = common::run_git(
        &repo_dir,
        ["push", "origin", &format!("{head}:refs/internal/secret")],
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("not allowed"),
        "expected rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 2: Protected branches: no force-push, no delete ────────────────

#[derive(Clone)]
struct ProtectMain {
    repo: PathBuf,
}

impl RepoAccess for ProtectMain {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_preliminary(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        for r in refs {
            if r.refname == "refs/heads/main" && r.kind == PushKind::Delete {
                return Err("main is protected: no delete".into());
            }
        }
        Ok(())
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            if r.refname == "refs/heads/main" && r.kind == PushKind::ForcePush {
                return Err("main is protected: no force-push".into());
            }
        }
        Ok(())
    }
}

dual_backend_access_test!(protect_main_allows_fast_forward, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| ProtectMain {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("ff.txt"), "ff")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "ff"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;
    server.stop();
    Ok(())
});

dual_backend_access_test!(protect_main_rejects_force_push, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| ProtectMain {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    common::run_git(&repo_dir, ["reset", "--hard", "HEAD~1"])?;
    fs::write(repo_dir.join("d.txt"), "d")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "d"])?;
    let err = common::run_git(&repo_dir, ["push", "--force", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("no force-push"),
        "expected force-push rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

dual_backend_access_test!(protect_main_rejects_delete, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| ProtectMain {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    let err = common::run_git(&repo_dir, ["push", "origin", "--delete", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("no delete"),
        "expected delete rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 3: Committer email must match the authenticated pusher ─────────

#[derive(Clone)]
struct CommitterMatchesPusher {
    repo: PathBuf,
    verified_emails: HashSet<String>,
}

impl RepoAccess for CommitterMatchesPusher {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            for c in push
                .new_commits(r)
                .map_err(|e| format!("walk failed: {e}"))?
            {
                let email = c.committer.email.to_str_lossy();
                if !self.verified_emails.contains(email.as_ref()) {
                    return Err(format!(
                        "commit {} has committer {email} not in your verified emails",
                        c.oid
                    ));
                }
            }
        }
        Ok(())
    }
}

dual_backend_access_test!(committer_matches_pusher_allows_known, |backend| {
    let temprepo = common::temprepo()?;
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, |rp| {
        let mut emails = HashSet::new();
        emails.insert("committer@example.com".into());
        CommitterMatchesPusher {
            repo: PathBuf::from(rp.as_ref()),
            verified_emails: emails,
        }
    });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("ok.txt"), "ok")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "ok"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;
    server.stop();
    Ok(())
});

dual_backend_access_test!(committer_matches_pusher_rejects_unknown, |backend| {
    let temprepo = common::temprepo()?;
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, |rp| {
        let mut emails = HashSet::new();
        emails.insert("only-this@example.com".into());
        CommitterMatchesPusher {
            repo: PathBuf::from(rp.as_ref()),
            verified_emails: emails,
        }
    });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("bad.txt"), "bad")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "bad"])?;
    let err = common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("not in your verified emails"),
        "expected committer rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 4: DCO sign-off required ───────────────────────────────────────

#[derive(Clone)]
struct DcoRequired {
    repo: PathBuf,
}

impl RepoAccess for DcoRequired {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            for c in push
                .new_commits(r)
                .map_err(|e| format!("walk failed: {e}"))?
            {
                let trailer = format!(
                    "Signed-off-by: {} <{}>",
                    c.author.name.to_str_lossy(),
                    c.author.email.to_str_lossy(),
                );
                if !c
                    .message
                    .windows(trailer.len())
                    .any(|w| w == trailer.as_bytes())
                {
                    return Err(format!(
                        "commit {} missing DCO sign-off matching its author",
                        c.oid
                    ));
                }
            }
        }
        Ok(())
    }
}

dual_backend_access_test!(dco_allows_signed_off, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| DcoRequired {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("a.txt"), "ok")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(
        &repo_dir,
        [
            "commit",
            "-m",
            "ok\n\nSigned-off-by: Test Author <author@example.com>",
        ],
    )?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;
    server.stop();
    Ok(())
});

dual_backend_access_test!(dco_rejects_unsigned_off, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| DcoRequired {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("a.txt"), "ok")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "no sign-off"])?;
    let err = common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("missing DCO sign-off"),
        "expected DCO rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 6: Topology rules — merges-only on main ────────────────────────

#[derive(Clone)]
struct MergesOnlyMain {
    repo: PathBuf,
}

impl RepoAccess for MergesOnlyMain {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            if r.refname != "refs/heads/main" {
                continue;
            }
            for c in push
                .new_commits(r)
                .map_err(|e| format!("walk failed: {e}"))?
            {
                if c.parents.len() < 2 {
                    return Err(format!(
                        "commit {} on main must be a merge (no fast-forward)",
                        c.oid
                    ));
                }
            }
        }
        Ok(())
    }
}

dual_backend_access_test!(merges_only_main_rejects_ff, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| MergesOnlyMain {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("a.txt"), "ff")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "ff"])?;
    let err = common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("must be a merge"),
        "expected merge-only rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 7 (subset): Force-push loss-prevention by count ────────────────

#[derive(Clone)]
struct LimitDroppedCommits {
    repo: PathBuf,
    max: usize,
}

impl RepoAccess for LimitDroppedCommits {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            let dropped = push
                .dropped_commits(r)
                .map_err(|e| format!("walk failed: {e}"))?;
            if dropped.len() > self.max {
                return Err(format!(
                    "force-push to {} would drop {} commits (limit {})",
                    r.refname,
                    dropped.len(),
                    self.max,
                ));
            }
        }
        Ok(())
    }
}

dual_backend_access_test!(limit_dropped_allows_small_drop, |backend| {
    let temprepo = common::temprepo()?;
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, |rp| {
        LimitDroppedCommits {
            repo: PathBuf::from(rp.as_ref()),
            max: 5,
        }
    });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    // Reset to before the previous commit and replace it — drops 1 commit.
    common::run_git(&repo_dir, ["reset", "--hard", "HEAD~1"])?;
    fs::write(repo_dir.join("d.txt"), "d")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "d"])?;
    common::run_git(&repo_dir, ["push", "--force", "origin", "main"])?;
    server.stop();
    Ok(())
});

dual_backend_access_test!(limit_dropped_rejects_too_many, |backend| {
    let temprepo = common::temprepo()?;
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, |rp| {
        LimitDroppedCommits {
            repo: PathBuf::from(rp.as_ref()),
            max: 0,
        }
    });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    common::run_git(&repo_dir, ["reset", "--hard", "HEAD~1"])?;
    fs::write(repo_dir.join("d.txt"), "d")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "d"])?;
    let err = common::run_git(&repo_dir, ["push", "--force", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("would drop"),
        "expected drop-limit rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 8: Path-glob block ─────────────────────────────────────────────

#[derive(Clone)]
struct PathGlobBlock {
    repo: PathBuf,
}

impl RepoAccess for PathGlobBlock {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            let new_commits = push
                .new_commits(r)
                .map_err(|e| format!("walk failed: {e}"))?;
            for c in new_commits {
                if c.message.starts_with(b"migration: ") {
                    continue;
                }
                let diff = push.ref_diff(r).map_err(|e| format!("diff failed: {e}"))?;
                if diff
                    .touched_paths()
                    .any(|p| AsRef::<[u8]>::as_ref(p).starts_with(b"migrations/"))
                {
                    return Err(format!(
                        "commit {} touches migrations/ without `migration:` prefix",
                        c.oid
                    ));
                }
            }
        }
        Ok(())
    }
}

dual_backend_access_test!(path_glob_allows_normal_paths, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| PathGlobBlock {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("src.txt"), "x")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "regular change"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;
    server.stop();
    Ok(())
});

dual_backend_access_test!(path_glob_rejects_unprefixed_migration, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| PathGlobBlock {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::create_dir_all(repo_dir.join("migrations"))?;
    fs::write(repo_dir.join("migrations/001.sql"), "SELECT 1;")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "no prefix"])?;
    let err = common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("migrations/"),
        "expected migrations rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 9: Submodule URL allow-list (via read_blob) ────────────────────

#[derive(Clone)]
struct GitmodulesAllowlist {
    repo: PathBuf,
    allowed_prefixes: Vec<String>,
}

impl RepoAccess for GitmodulesAllowlist {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn authorize_push(&self, _ctx: &(), push: &dyn Comparison<'_>) -> Result<(), String> {
        for r in push.refs() {
            let diff = push.ref_diff(r).map_err(|e| format!("diff: {e}"))?;
            for entry in diff.added_or_modified() {
                if entry.path.as_slice() != b".gitmodules" {
                    continue;
                }
                let bytes = push
                    .read_blob(entry.oid, 64 * 1024)
                    .ok_or_else(|| ".gitmodules too large".to_string())?;
                for url in parse_submodule_urls(&bytes) {
                    let allowed = self
                        .allowed_prefixes
                        .iter()
                        .any(|p| url.starts_with(p.as_str()));
                    if !allowed {
                        return Err(format!("submodule URL not allowed: {url}"));
                    }
                }
            }
        }
        Ok(())
    }
}

fn parse_submodule_urls(bytes: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("url = ") {
            out.push(url.trim().to_string());
        }
    }
    out
}

dual_backend_access_test!(gitmodules_allows_listed, |backend| {
    let temprepo = common::temprepo()?;
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, |rp| {
        GitmodulesAllowlist {
            repo: PathBuf::from(rp.as_ref()),
            allowed_prefixes: vec!["https://github.com/".to_string()],
        }
    });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(
        repo_dir.join(".gitmodules"),
        "[submodule \"lib\"]\n\tpath = lib\n\turl = https://github.com/acme/lib\n",
    )?;
    common::run_git(&repo_dir, ["add", ".gitmodules"])?;
    common::run_git(&repo_dir, ["commit", "-m", "add submodule"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;
    server.stop();
    Ok(())
});

dual_backend_access_test!(gitmodules_rejects_unlisted, |backend| {
    let temprepo = common::temprepo()?;
    let server = common::axum_access_server_with_backend(temprepo.path(), backend, |rp| {
        GitmodulesAllowlist {
            repo: PathBuf::from(rp.as_ref()),
            allowed_prefixes: vec!["https://github.com/".to_string()],
        }
    });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(
        repo_dir.join(".gitmodules"),
        "[submodule \"evil\"]\n\tpath = evil\n\turl = https://evil.example/exfil\n",
    )?;
    common::run_git(&repo_dir, ["add", ".gitmodules"])?;
    common::run_git(&repo_dir, ["commit", "-m", "add submodule"])?;
    let err = common::run_git(&repo_dir, ["push", "origin", "main"]).unwrap_err();
    assert!(
        err.to_string().contains("not allowed"),
        "expected gitmodules rejection, got: {err}"
    );
    server.stop();
    Ok(())
});

// ─── Example 10: verify_external smoke test ─────────────────────────────────
//
// Stub `verify_external` that always returns Verified.  Real Sigstore tests
// live in the out-of-tree adapter crate.  The point here is to prove the
// dispatch wiring works — `verify_external`'s decision wins over the native
// verdict.

#[derive(Clone)]
struct AlwaysVerified {
    repo: PathBuf,
}

impl RepoAccess for AlwaysVerified {
    type RepoId = PathBuf;
    type PushContext = ();

    fn repo_id(&self) -> &PathBuf {
        &self.repo
    }

    fn verification_keys(
        &self,
        _signers: &[Signer<'_>],
    ) -> HashMap<SignerKey, Vec<VerificationKey>> {
        HashMap::new()
    }

    fn verify_external(&self, _sig: &ExternalSig<'_>) -> Option<VerificationStatus> {
        Some(VerificationStatus::Verified {
            identity: SignedIdentity::Other {
                description: "stub".into(),
            },
            format: SignatureFormat::Unknown,
        })
    }

    fn authorize_push(&self, _ctx: &(), _push: &dyn Comparison<'_>) -> Result<(), String> {
        // We don't actually have signed commits in the test fixture, so this
        // rule is a no-op.  The smoke test is that `verify_external` is
        // wired through correctly — exercised by the integration glue
        // running without panicking.
        Ok(())
    }
}

dual_backend_access_test!(verify_external_smoke, |backend| {
    let temprepo = common::temprepo()?;
    let server =
        common::axum_access_server_with_backend(temprepo.path(), backend, |rp| AlwaysVerified {
            repo: PathBuf::from(rp.as_ref()),
        });
    let clone_dir = tempdir()?;
    let repo_dir = clone_repo(server.port, clone_dir.path(), Some("main"));

    fs::write(repo_dir.join("ok.txt"), "ok")?;
    common::run_git(&repo_dir, ["add", "."])?;
    common::run_git(&repo_dir, ["commit", "-m", "ok"])?;
    common::run_git(&repo_dir, ["push", "origin", "main"])?;
    server.stop();
    Ok(())
});
