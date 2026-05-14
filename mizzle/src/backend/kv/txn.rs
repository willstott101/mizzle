//! Transaction lifecycle helpers.
//!
//! `tikv-client`'s `Transaction::drop` panics if the txn was never committed
//! or rolled back.  Bare `?` propagation through an active txn therefore
//! aborts the process, so every txn needs an explicit cleanup arm on the
//! error path.
//!
//! The two functions here collapse the (body → commit-or-rollback) flow into
//! one line at each call site:
//!
//! ```ignore
//! let mut txn = db.begin_optimistic().await.context("begin")?;
//! let result = async { /* body uses &mut txn */ Ok(value) }.await;
//! finalize_read(&mut txn, result).await
//! ```
//!
//! The async block borrows `&mut txn` only for its own duration; once it
//! resolves the borrow is released and the txn is moved into the finalizer.

use anyhow::{Context, Result};
use tikv_client::Transaction;

/// Commit on success, rollback on failure.  Surfaces the commit error if any.
/// Use for writes / CAS / pessimistic txns where the commit can meaningfully
/// fail and the caller needs to know.
pub(super) async fn finalize_write<T>(txn: &mut Transaction, result: Result<T>) -> Result<T> {
    match result {
        Ok(v) => {
            txn.commit().await.context("commit txn")?;
            Ok(v)
        }
        Err(e) => {
            let _ = txn.rollback().await;
            Err(e)
        }
    }
}

/// Same shape, but swallows commit errors.  Read-only snapshot reads have
/// already produced their value by commit time, so a failed commit is just
/// noise — we don't want it masking real read errors either.
pub(super) async fn finalize_read<T>(txn: &mut Transaction, result: Result<T>) -> Result<T> {
    match result {
        Ok(v) => {
            let _ = txn.commit().await;
            Ok(v)
        }
        Err(e) => {
            let _ = txn.rollback().await;
            Err(e)
        }
    }
}
