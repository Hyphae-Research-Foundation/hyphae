// SPDX-License-Identifier: AGPL-3.0-only

//! Cooperative cancellation shared by embedded and dispatched operations.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cloneable cooperative cancellation state for one product request.
///
/// Cancellation is observed at operation admission and before every product
/// mutation is published. Runtime calls that have already started remain
/// atomic and are never interrupted in the middle of a mutation.
#[derive(Clone, Debug, Default)]
pub struct ProductCancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl ProductCancellationToken {
    /// Creates an uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation for every clone of this token.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
