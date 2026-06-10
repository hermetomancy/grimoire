//! A LIFO rollback ledger: registered undo actions run in reverse order on drop unless
//! the transaction commits.

/// Best-effort, RAII-style rollback for a multi-step install. Rollback actions run in
/// reverse registration order when the transaction is dropped without [`commit`](Transaction::commit)ting,
/// e.g. when an install step returns an error via `?`.
#[derive(Default)]
pub(crate) struct Transaction {
    rollbacks: Vec<Box<dyn FnOnce()>>,
    committed: bool,
}

impl Transaction {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn on_rollback(&mut self, action: impl FnOnce() + 'static) {
        self.rollbacks.push(Box::new(action));
    }

    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        while let Some(rollback) = self.rollbacks.pop() {
            rollback();
        }
    }
}
