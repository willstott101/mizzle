#![cfg(feature = "ssh")]

mod common;

use anyhow::Result;
use common::{init_logging, Config};
use mizzle::servers::ssh::SshAuth;
use russh::keys::{ssh_key, PrivateKey, PublicKey};
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};

/// Accept-all SshAuth for testing.
struct TestSshAuth {
    bare_repo_path: PathBuf,
}

impl SshAuth for TestSshAuth {
    type Access = Config;

    async fn authorize(
        &self,
        _user: &str,
        _public_key: &PublicKey,
        _repo_path: &str,
    ) -> Option<Config> {
        Some(Config {
            bare_repo_path: self.bare_repo_path.clone(),
        })
    }
}

/// SshAuth that rejects all operations.
struct RejectSshAuth;

impl SshAuth for RejectSshAuth {
    type Access = Config;

    async fn authorize(
        &self,
        _user: &str,
        _public_key: &PublicKey,
        _repo_path: &str,
    ) -> Option<Config> {
        None
    }
}

struct SshTestServer {
    port: u16,
    client_key_path: PathBuf,
    _key_dir: TempDir,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl SshTestServer {
    fn start(auth: impl SshAuth) -> Self {
        init_logging();

        let key_dir = tempdir().unwrap();
        let client_key =
            PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
                .unwrap();
        let client_key_path = key_dir.path().join("id_ed25519");
        client_key
            .write_openssh_file(&client_key_path, ssh_key::LineEnding::LF)
            .unwrap();
        // ssh requires 600 permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&client_key_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }

        let server_key =
            PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
                .unwrap();

        let config = russh::server::Config {
            keys: vec![server_key],
            auth_rejection_time: std::time::Duration::from_millis(100),
            auth_rejection_time_initial: Some(std::time::Duration::from_millis(0)),
            ..Default::default()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();

        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        std::thread::spawn(move || {
            rt.block_on(async move {
                tokio::select! {
                    result = mizzle::servers::ssh::run_on_socket(&listener, config, auth, Default::default()) => {
                        if let Err(e) = result {
                            tracing::error!("SSH server error: {:#}", e);
                        }
                    }
                    _ = rx => {}
                }
            });
        });

        // Give the server a moment to start accepting
        std::thread::sleep(std::time::Duration::from_millis(100));

        SshTestServer {
            port,
            client_key_path,
            _key_dir: key_dir,
            shutdown: tx,
        }
    }

    fn git_ssh_command(&self) -> String {
        format!(
            "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {} -p {}",
            self.client_key_path.display(),
            self.port,
        )
    }

    fn clone_url(&self, repo_path: &str) -> String {
        format!("ssh://git@127.0.0.1:{}/{}", self.port, repo_path)
    }

    fn stop(self) {
        let _ = self.shutdown.send(());
    }
}

fn ssh_git(server: &SshTestServer, cwd: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_SSH_COMMAND", server.git_ssh_command())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .env("GIT_TRACE_PACKET", "1")
        .env("GIT_TRACE", "2")
        .stdin(std::process::Stdio::null())
        .output()?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git {} failed (status {}):\nSTDOUT:\n{}\nSTDERR:\n{}",
            args.join(" "),
            output.status,
            stdout,
            stderr,
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn test_ssh_clone() -> Result<()> {
    let repo = common::temprepo()?;
    let server = SshTestServer::start(TestSshAuth {
        bare_repo_path: repo.path(),
    });

    let clone_dir = tempdir()?;
    let clone_path = clone_dir.path().join("cloned");
    ssh_git(
        &server,
        clone_dir.path(),
        &[
            "clone",
            &server.clone_url("repo"),
            clone_path.to_str().unwrap(),
        ],
    )?;

    // Verify the clone has the expected files.
    assert!(clone_path.join("README.md").exists());
    assert!(clone_path.join("hello.txt").exists());

    server.stop();
    Ok(())
}

#[test]
fn test_ssh_push() -> Result<()> {
    let repo = common::temprepo()?;
    let server = SshTestServer::start(TestSshAuth {
        bare_repo_path: repo.path(),
    });

    // Clone first.
    let clone_dir = tempdir()?;
    let clone_path = clone_dir.path().join("cloned");
    ssh_git(
        &server,
        clone_dir.path(),
        &[
            "clone",
            &server.clone_url("repo"),
            clone_path.to_str().unwrap(),
        ],
    )?;

    // Make a change and push.
    std::fs::write(clone_path.join("pushed.txt"), "pushed via ssh\n")?;
    ssh_git(&server, &clone_path, &["add", "pushed.txt"])?;
    ssh_git(&server, &clone_path, &["commit", "-m", "SSH push"])?;
    ssh_git(&server, &clone_path, &["push", "origin", "main"])?;

    // Verify the push landed in the bare repo.
    let verify_dir = tempdir()?;
    let verify_path = verify_dir.path().join("verify");
    ssh_git(
        &server,
        verify_dir.path(),
        &[
            "clone",
            &server.clone_url("repo"),
            verify_path.to_str().unwrap(),
        ],
    )?;
    assert!(verify_path.join("pushed.txt").exists());

    server.stop();
    Ok(())
}

#[test]
fn test_ssh_fetch() -> Result<()> {
    let repo = common::temprepo()?;
    let server = SshTestServer::start(TestSshAuth {
        bare_repo_path: repo.path(),
    });

    // Clone.
    let clone_dir = tempdir()?;
    let clone_path = clone_dir.path().join("cloned");
    ssh_git(
        &server,
        clone_dir.path(),
        &[
            "clone",
            &server.clone_url("repo"),
            clone_path.to_str().unwrap(),
        ],
    )?;

    // Push a new commit from another clone.
    let clone2_dir = tempdir()?;
    let clone2_path = clone2_dir.path().join("cloned2");
    ssh_git(
        &server,
        clone2_dir.path(),
        &[
            "clone",
            &server.clone_url("repo"),
            clone2_path.to_str().unwrap(),
        ],
    )?;
    std::fs::write(clone2_path.join("fetched.txt"), "fetch test\n")?;
    ssh_git(&server, &clone2_path, &["add", "fetched.txt"])?;
    ssh_git(&server, &clone2_path, &["commit", "-m", "fetch commit"])?;
    ssh_git(&server, &clone2_path, &["push", "origin", "main"])?;

    // Fetch from first clone.
    ssh_git(&server, &clone_path, &["pull", "origin", "main"])?;
    assert!(clone_path.join("fetched.txt").exists());

    server.stop();
    Ok(())
}

#[test]
fn test_ssh_auth_rejected() -> Result<()> {
    let repo = common::temprepo()?;
    // Use the server but with RejectSshAuth that rejects all keys.
    let server = SshTestServer::start(RejectSshAuth);

    let clone_dir = tempdir()?;
    let clone_path = clone_dir.path().join("cloned");
    let result = ssh_git(
        &server,
        clone_dir.path(),
        &[
            "clone",
            &server.clone_url(repo.path().to_str().unwrap()),
            clone_path.to_str().unwrap(),
        ],
    );

    let err = result.expect_err("clone should have failed with rejected auth");
    assert!(
        err.to_string().contains("permission denied"),
        "client should see permission denied message, got: {err}"
    );

    server.stop();
    Ok(())
}
