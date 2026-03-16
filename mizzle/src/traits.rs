use std::convert::Infallible;

/// Describes how a push changes a ref.  Computed by mizzle before calling
/// [`RepoAccess::authorize_push`] so that authorisers never need to open the
/// repository themselves.
#[derive(Debug, PartialEq, Clone)]
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

    /// Called after all refs have been successfully updated on a push.
    /// Cannot abort — refs are already written.  Use this for CI triggering,
    /// notifications, audit logging, etc.  Spawning your own async task here
    /// is fine if you need non-blocking behaviour.
    fn post_receive(&self, _refs: &[PushRef<'_>]) {}

    /// Return `true` to have mizzle initialise a bare repository at
    /// [`repo_path`] if none exists yet when the first push arrives.
    fn auto_init(&self) -> bool {
        false
    }
}

/// Convenience impl for deny-all access objects that are never constructed.
impl RepoAccess for Infallible {
    fn repo_path(&self) -> &str {
        match *self {}
    }
}

impl<T: RepoAccess + ?Sized> RepoAccess for Box<T> {
    fn repo_path(&self) -> &str {
        (**self).repo_path()
    }
    fn authorize_push(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        (**self).authorize_push(refs)
    }
    fn post_receive(&self, refs: &[PushRef<'_>]) {
        (**self).post_receive(refs)
    }
    fn auto_init(&self) -> bool {
        (**self).auto_init()
    }
}
