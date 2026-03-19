use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use log::{error, info};
use russh::server::{Auth, Msg, Server, Session};
use russh::{Channel, ChannelId};
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::serve::{serve_receive_pack, serve_upload_pack};
use crate::traits::RepoAccess;

// TODO(config): make this configurable via run()/run_on_socket().
/// Maximum time between SSH auth and exec request.  Connections that
/// authenticate but never send a command are dropped after this deadline.
const EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// SSH authentication trait.
///
/// Because SSH authenticates the user before the repository path is known
/// (the path arrives later in the exec request), all public keys are accepted
/// at the SSH layer and the real authorisation is deferred to
/// [`authorize`](SshAuth::authorize), which is called once the exec request
/// reveals the user, key, and repository path together.
///
/// This is where expensive work should happen — database lookups, permission
/// loading, etc.  The returned [`RepoAccess`] must then be cheap to
/// interrogate for the remainder of the request (see the
/// [`RepoAccess` design contract](RepoAccess#design-contract)).
pub trait SshAuth: Send + Sync + 'static {
    type Access: RepoAccess + Send + 'static;

    /// Authorize a git operation.  Called once per exec request with the
    /// authenticated user, their public key, and the repository path.
    /// Return `Some(access)` to allow, `None` to reject.
    fn authorize(
        &self,
        user: &str,
        public_key: &russh::keys::PublicKey,
        repo_path: &str,
    ) -> impl Future<Output = Option<Self::Access>> + Send;
}

struct MizzleSshHandler<A: SshAuth> {
    auth: Arc<A>,
    user: Option<String>,
    public_key: Option<russh::keys::PublicKey>,
    exec_timeout: Option<tokio::task::JoinHandle<()>>,
    git_protocol_version: u32,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl<A: SshAuth> russh::server::Handler for MizzleSshHandler<A> {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.user = Some(user.to_string());
        self.public_key = Some(public_key.clone());
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.channels.insert(channel.id(), channel);

        // Start the exec timeout — if the client never sends an exec
        // request, disconnect after EXEC_TIMEOUT.
        if self.exec_timeout.is_none() {
            let handle = session.handle();
            self.exec_timeout = Some(tokio::spawn(async move {
                tokio::time::sleep(EXEC_TIMEOUT).await;
                error!("SSH client did not send exec request within deadline, disconnecting");
                let _ = handle
                    .disconnect(
                        russh::Disconnect::ByApplication,
                        "no git command received within deadline".into(),
                        "en".into(),
                    )
                    .await;
            }));
        }

        Ok(true)
    }

    async fn env_request(
        &mut self,
        _channel_id: ChannelId,
        variable_name: &str,
        variable_value: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if variable_name == "GIT_PROTOCOL" && variable_value.contains("version=2") {
            self.git_protocol_version = 2;
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Cancel the exec timeout — the client sent a command in time.
        if let Some(timeout) = self.exec_timeout.take() {
            timeout.abort();
        }

        let command_str = std::str::from_utf8(data)?;
        let (cmd, repo_path) = parse_git_command(command_str)?;

        let user = self
            .user
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no authenticated user"))?;
        let public_key = self
            .public_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no public key stored"))?;

        let access = match self.auth.authorize(user, public_key, repo_path).await {
            Some(a) => a,
            None => {
                error!("SSH auth rejected for user={} repo={}", user, repo_path);
                let handle = session.handle();
                let msg = format!(
                    "ERROR: permission denied for '{}' on '{}'\n",
                    user, repo_path,
                );
                // ext=1 is stderr in the SSH protocol.
                let _ = handle
                    .extended_data(channel_id, 1, msg.into_bytes())
                    .await;
                let _ = handle.exit_status_request(channel_id, 1).await;
                let _ = handle.eof(channel_id).await;
                let _ = handle.close(channel_id).await;
                return Ok(());
            }
        };

        let channel = self
            .channels
            .remove(&channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown channel {}", channel_id))?;

        let version = self.git_protocol_version;
        let handle = session.handle();
        let stream = channel.into_stream();

        tokio::spawn(async move {
            let compat = stream.compat();
            // Use futures_lite split to get independent read/write halves.
            let (reader, mut writer) = futures_lite::io::split(compat);

            let result = match cmd {
                GitCommand::UploadPack => {
                    serve_upload_pack(access, reader, &mut writer, version).await
                }
                GitCommand::ReceivePack => serve_receive_pack(access, reader, &mut writer).await,
            };

            let exit_code = match &result {
                Ok(()) => 0u32,
                Err(e) => {
                    error!("SSH error: {:#}", e);
                    1
                }
            };

            // Close the write half to flush pending data.
            let _ = futures_lite::AsyncWriteExt::close(&mut writer).await;
            drop(writer);

            // Send exit-status via the session handle (the ChannelStream
            // was consumed by split, so we use the handle instead).
            let _ = handle.exit_status_request(channel_id, exit_code).await;
            let _ = handle.eof(channel_id).await;
            let _ = handle.close(channel_id).await;
        });

        Ok(())
    }
}

enum GitCommand {
    UploadPack,
    ReceivePack,
}

/// Parse an SSH exec command like `git-upload-pack '/path/to/repo.git'` or
/// `git-receive-pack '/path/to/repo.git'`.
fn parse_git_command(command: &str) -> anyhow::Result<(GitCommand, &str)> {
    let command = command.trim();
    let (cmd, rest) = command
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("invalid git command: {}", command))?;

    let git_cmd = match cmd {
        "git-upload-pack" => GitCommand::UploadPack,
        "git-receive-pack" => GitCommand::ReceivePack,
        _ => anyhow::bail!("unsupported git command: {}", cmd),
    };

    // Strip surrounding quotes and leading slash.
    let repo_path = rest.trim_matches('\'').trim_matches('"');
    let repo_path = repo_path.strip_prefix('/').unwrap_or(repo_path);

    Ok((git_cmd, repo_path))
}

struct MizzleSshServer<A: SshAuth> {
    auth: Arc<A>,
}

impl<A: SshAuth> russh::server::Server for MizzleSshServer<A> {
    type Handler = MizzleSshHandler<A>;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        MizzleSshHandler {
            auth: self.auth.clone(),
            user: None,
            public_key: None,
            exec_timeout: None,
            git_protocol_version: 1,
            channels: HashMap::new(),
        }
    }
}

/// Start an SSH server on the given address.
///
/// Start an SSH server on the given address.
pub async fn run<A: SshAuth>(
    addr: impl tokio::net::ToSocketAddrs + Send,
    config: russh::server::Config,
    auth: A,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    run_on_socket(&listener, config, auth).await
}

/// Start an SSH server on an already-bound listener.
///
/// Useful when you need to know the port before starting the server
/// (e.g. port 0 for tests).
pub async fn run_on_socket<A: SshAuth>(
    listener: &tokio::net::TcpListener,
    config: russh::server::Config,
    auth: A,
) -> anyhow::Result<()> {
    let config = Arc::new(config);
    let mut server = MizzleSshServer {
        auth: Arc::new(auth),
    };

    info!("Starting SSH server");
    server.run_on_socket(config, listener).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_upload_pack_with_quotes() {
        let (cmd, path) = parse_git_command("git-upload-pack '/foo/bar.git'").unwrap();
        assert!(matches!(cmd, GitCommand::UploadPack));
        assert_eq!(path, "foo/bar.git");
    }

    #[test]
    fn parse_receive_pack_no_quotes() {
        let (cmd, path) = parse_git_command("git-receive-pack /my/repo").unwrap();
        assert!(matches!(cmd, GitCommand::ReceivePack));
        assert_eq!(path, "my/repo");
    }

    #[test]
    fn parse_unknown_command_fails() {
        assert!(parse_git_command("git-foo '/repo'").is_err());
    }
}
