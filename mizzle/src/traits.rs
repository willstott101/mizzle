/// Describes how a push changes a ref.  Computed by mizzle before calling
/// [`GitServerCallbacks::authorize_push`] so that authorisers never need
/// to open the repository themselves.
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

/// A single ref update within a push, passed to [`GitServerCallbacks::authorize_push`].
pub struct PushRef<'a> {
    pub refname: &'a str,
    pub kind: PushKind,
}

pub trait GitServerCallbacks: Clone {
    /// Maps a URL-derived repo path to a filesystem path.
    /// Return an empty string to deny access.
    fn auth(&self, repo_path: &str) -> Box<str>;

    /// Called once per push with all requested ref updates, after the
    /// ref-update commands have been parsed but before the packfile is
    /// written.  Return `Err(reason)` to reject the entire push; `reason` is
    /// sent back to the client.
    fn authorize_push(
        &self,
        _repo_path: &str,
        _refs: &[PushRef<'_>],
    ) -> Result<(), String> {
        Ok(())
    }
}
