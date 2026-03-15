use std::convert::Infallible;

/// Describes how a push changes a ref.  Computed by mizzle before calling
/// [`RepoAccess::authorize_push`] so that authorisers never need to open the
/// repository themselves.
pub enum PushKind {
    /// This ref is being created for the first time.
    Create,
    /// This ref is being deleted.
    Delete,
    /// A normal fast-forward update.
    FastForward,
    /// A non-fast-forward (force) update.
    ForcePush,
}

/// A single ref update within a push, passed to [`RepoAccess::authorize_push`].
pub struct PushRef<'a> {
    pub refname: &'a str,
    pub kind: PushKind,
}

/// Returned by your auth implementation.  Carries the resolved filesystem path
/// and any per-request state needed for push checks.
pub trait RepoAccess {
    /// Filesystem path of the repository to serve.
    fn repo_path(&self) -> &str;

    /// Called once per push with all requested ref updates, after push kinds
    /// have been computed.  Return `Err(reason)` to reject the entire push;
    /// `reason` is sent back to the client.
    fn authorize_push(&self, _refs: &[PushRef<'_>]) -> Result<(), String> {
        Ok(())
    }
}

/// Convenience impl for deny-all access objects that are never constructed.
impl RepoAccess for Infallible {
    fn repo_path(&self) -> &str {
        match *self {}
    }
}
