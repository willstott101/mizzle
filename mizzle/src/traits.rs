use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

pub use crate::auth::Comparison;
pub use crate::auth_types::{
    CommitInfo, ComparisonError, ExternalSig, Identity, PushKind, PushRef, RefDiff, RefDiffChange,
    RefDiffEntry, SignatureFormat, SignedIdentity, Signer, SignerKey, TagInfo, VerificationKey,
    VerificationStatus,
};
pub use crate::backend::PackMetadata;

/// Boxed future returned by [`RepoAccess::post_receive`].
///
/// The future is `'static`: if the implementation needs data from the
/// [`Comparison`] handle or from `&self`, it should extract / clone it
/// synchronously before constructing the future.
pub type PostReceiveFut = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

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
/// **Authorisers must never open the repository.**  Everything an authoriser
/// needs to make a decision is delivered through plain values on
/// [`PushRef`] or via the [`Comparison`] handle whose accessors are
/// explicit and bounded.
pub trait RepoAccess {
    /// Identifier for the repository to serve.
    type RepoId: Send + Sync + Clone + std::fmt::Debug + 'static;

    /// State carried from [`authorize_preliminary`](Self::authorize_preliminary)
    /// into [`authorize_push`](Self::authorize_push).  Use `()` if you have no
    /// state to thread between the two calls.
    type PushContext: Default + Send + 'static;

    /// Return the identifier of the repository to serve.
    fn repo_id(&self) -> &Self::RepoId;

    /// Cheap classification before pack data is transferred.
    ///
    /// Returns a typed value carried into [`authorize_push`](Self::authorize_push).
    /// Forges that need no preliminary state can leave this at the default
    /// (returns `Ok(Default::default())`).
    fn authorize_preliminary(&self, _refs: &[PushRef<'_>]) -> Result<Self::PushContext, String> {
        Ok(Default::default())
    }

    /// Full authorisation.  The forge inspects whatever it needs through the
    /// [`Comparison`] handle; everything is lazy and cached.  Return
    /// `Err(reason)` to reject the entire push.
    ///
    /// This must be cheap — see the [design contract](RepoAccess#design-contract).
    fn authorize_push(
        &self,
        _ctx: &Self::PushContext,
        _push: &dyn Comparison<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Resolve candidate verification keys for a batch of `(email, format)`
    /// signers.  Default: no keys → every signed commit verifies as
    /// [`VerificationStatus::UnknownKey`](crate::auth_types::VerificationStatus::UnknownKey).
    fn verification_keys(
        &self,
        _signers: &[Signer<'_>],
    ) -> HashMap<SignerKey, Vec<VerificationKey>> {
        HashMap::new()
    }

    /// Per-signature override / escape hatch.  The native verifier runs first.
    /// If this returns `Some`, that result wins; if `None`, the native verdict
    /// stands.
    fn verify_external(&self, _sig: &ExternalSig<'_>) -> Option<VerificationStatus> {
        None
    }

    /// Called after all refs have been successfully updated on a push.
    /// Cannot abort.  Receives the same [`Comparison`] handle as
    /// [`authorize_push`](Self::authorize_push); see [`PostReceiveFut`] for
    /// the lifetime contract.
    fn post_receive(&self, _push: &dyn Comparison<'_>) -> PostReceiveFut {
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
    type PushContext = ();
    fn repo_id(&self) -> &PathBuf {
        match *self {}
    }
}

impl<T: RepoAccess + ?Sized> RepoAccess for Box<T> {
    type RepoId = T::RepoId;
    type PushContext = T::PushContext;
    fn repo_id(&self) -> &T::RepoId {
        (**self).repo_id()
    }
    fn authorize_preliminary(&self, refs: &[PushRef<'_>]) -> Result<Self::PushContext, String> {
        (**self).authorize_preliminary(refs)
    }
    fn authorize_push(
        &self,
        ctx: &Self::PushContext,
        push: &dyn Comparison<'_>,
    ) -> Result<(), String> {
        (**self).authorize_push(ctx, push)
    }
    fn verification_keys(
        &self,
        signers: &[Signer<'_>],
    ) -> HashMap<SignerKey, Vec<VerificationKey>> {
        (**self).verification_keys(signers)
    }
    fn verify_external(&self, sig: &ExternalSig<'_>) -> Option<VerificationStatus> {
        (**self).verify_external(sig)
    }
    fn post_receive(&self, push: &dyn Comparison<'_>) -> PostReceiveFut {
        (**self).post_receive(push)
    }
    fn auto_init(&self) -> bool {
        (**self).auto_init()
    }
}
