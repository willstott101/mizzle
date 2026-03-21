use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

pub use mizzle_proto::types::{PushKind, PushRef};

/// Boxed future returned by [`RepoAccess::post_receive`].
pub type PostReceiveFut<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Per-request authorisation handle.
///
/// # Design contract
///
/// **Construction is where expensive work happens.**  Your framework resolves
/// the authenticated user, loads their permissions, evaluates branch-protection
/// rules, and stores the results in the `RepoAccess` value before handing it to
/// mizzle.  By the time mizzle holds a `RepoAccess`, every subsequent call on
/// it must be reducible to cheap value comparisons against data already in
/// memory — no database queries, no HTTP calls, no file I/O.
///
/// This is intentional: it means auth adds no per-operation latency inside the
/// hot path, and it keeps authorisation logic entirely in your code rather than
/// spread across callbacks and hooks.
///
/// **Authorisers must never open the repository.**  [`authorize_push`] receives
/// all the information needed to make a decision as plain values — ref name and
/// a [`PushKind`] enum computed by mizzle.  The internal structure of the object
/// graph is not visible to authorisers, and that is a feature: branch-protection
/// rules, glob patterns, team membership, and any other policy are your concern,
/// not mizzle's.  If an authoriser needs to inspect the object graph it is a bug
/// in mizzle's callback interface, not in the authoriser.
///
/// [`authorize_push`]: RepoAccess::authorize_push
pub trait RepoAccess {
    /// Identifier for the repository to serve.
    ///
    /// For filesystem backends this is typically a [`PathBuf`]; other backends
    /// may use a UUID, database key, etc.
    type RepoId: Send + Sync + Clone + std::fmt::Debug + 'static;

    /// Return the identifier of the repository to serve.
    fn repo_id(&self) -> &Self::RepoId;

    /// Called once per push with all requested ref updates, after mizzle has
    /// computed the [`PushKind`] for each ref.  Return `Err(reason)` to reject
    /// the entire push; `reason` is forwarded to the client.
    ///
    /// This must be cheap — see the [design contract](RepoAccess#design-contract).
    fn authorize_push(&self, _refs: &[PushRef<'_>]) -> Result<(), String> {
        Ok(())
    }

    /// Called after all refs have been successfully updated on a push.
    /// Cannot abort — refs are already written.  Use this for CI triggering,
    /// notifications, audit logging, etc.
    fn post_receive<'a>(&'a self, _refs: &'a [PushRef<'a>]) -> PostReceiveFut<'a> {
        Box::pin(async {})
    }

    /// Return `true` to have mizzle initialise a bare repository at
    /// [`repo_id`](RepoAccess::repo_id) if none exists yet when the first push
    /// arrives.
    fn auto_init(&self) -> bool {
        false
    }
}

/// Convenience impl for deny-all access objects that are never constructed.
impl RepoAccess for Infallible {
    type RepoId = PathBuf;
    fn repo_id(&self) -> &PathBuf {
        match *self {}
    }
}

impl<T: RepoAccess + ?Sized> RepoAccess for Box<T> {
    type RepoId = T::RepoId;
    fn repo_id(&self) -> &T::RepoId {
        (**self).repo_id()
    }
    fn authorize_push(&self, refs: &[PushRef<'_>]) -> Result<(), String> {
        (**self).authorize_push(refs)
    }
    fn post_receive<'a>(&'a self, refs: &'a [PushRef<'a>]) -> PostReceiveFut<'a> {
        (**self).post_receive(refs)
    }
    fn auto_init(&self) -> bool {
        (**self).auto_init()
    }
}
