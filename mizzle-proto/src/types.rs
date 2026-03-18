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

/// A single ref update within a push, passed to [`RepoAccess::authorize_push`]
/// and [`RepoAccess::post_receive`].
pub struct PushRef<'a> {
    pub refname: &'a str,
    pub kind: PushKind,
}
