//! Cooperative cancellation shared by every layer.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::ResourceError;

/// Cooperative cancellation; cheap to check and cancellable from any thread.
///
/// A token derived with [`CancellationToken::child`] is cancelled when it or any of its ancestors
/// is cancelled; cancelling a child never cancels its parent. Clones share one cancellation state.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    node: Arc<Node>,
}

impl CancellationToken {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A new token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels this token and every token derived from it.
    pub fn cancel(&self) {
        self.node.is_cancelled.store(true, Ordering::Release);
    }

    /// # Returns
    ///
    /// Whether this token or one of its ancestors is cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        let mut node = Some(&self.node);
        while let Some(current) = node {
            if current.is_cancelled.load(Ordering::Acquire) {
                return true;
            }
            node = current.parent.as_ref();
        }
        false
    }

    /// # Returns
    ///
    /// A new token that is cancelled when it or `self` is cancelled.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            node: Arc::new(Node {
                is_cancelled: AtomicBool::new(false),
                parent: Some(Arc::clone(&self.node)),
            }),
        }
    }

    /// Checks whether the token is cancelled.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ResourceError::Cancelled`] if this token or one of its ancestors is cancelled.
    pub fn check(&self) -> Result<(), ResourceError> {
        if self.is_cancelled() {
            return Err(ResourceError::Cancelled);
        }
        Ok(())
    }
}

/// The cancellation state of one token and the link to its parent's.
#[derive(Debug, Default)]
struct Node {
    is_cancelled: AtomicBool,
    parent: Option<Arc<Self>>,
}

#[cfg(test)]
mod tests {
    use super::CancellationToken;
    use crate::ResourceError;

    #[test]
    fn new_tokens_are_not_cancelled() {
        for token in [CancellationToken::new(), CancellationToken::default()] {
            assert!(!token.is_cancelled());
            assert!(token.check().is_ok());
        }
    }

    #[test]
    fn cancelling_is_visible_through_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        clone.cancel();
        assert!(token.is_cancelled());
        assert!(matches!(token.check(), Err(ResourceError::Cancelled)));
    }

    #[test]
    fn cancellation_flows_from_ancestors_to_descendants_only() {
        let root = CancellationToken::new();
        let child = root.child();
        let grandchild = child.child();
        let sibling = root.child();

        grandchild.cancel();
        assert!(grandchild.is_cancelled());
        assert!(!child.is_cancelled());
        assert!(!root.is_cancelled());

        child.cancel();
        assert!(child.is_cancelled());
        assert!(!sibling.is_cancelled());
        assert!(!root.is_cancelled());

        root.cancel();
        assert!(sibling.is_cancelled());
        assert!(matches!(
            sibling.child().check(),
            Err(ResourceError::Cancelled)
        ));
    }

    #[test]
    fn tokens_can_be_cancelled_from_another_thread() {
        let token = CancellationToken::new();
        let child = token.child();
        std::thread::scope(|scope| {
            scope.spawn(|| token.cancel());
        });
        assert!(child.is_cancelled());
    }
}
