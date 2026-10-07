//! Cancellation policy (design §7.3, RV-22, T-CONC-04).

/// The state of a request at the moment cancellation is observed.
#[derive(Debug, Clone, Copy)]
pub struct RequestState {
    pub committed: bool,
    pub canceled: bool,
}

/// The action to take when cancellation is observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationAction {
    Abort,
    Complete,
    KeepReceipt,
}

/// Decide the cancellation action from the request state.
///
/// - Not committed + canceled  -> Abort (nothing to undo, nothing created).
/// - Not committed + not canceled -> Complete.
/// - Committed + canceled      -> KeepReceipt (receipt is durable; a replay
///   must find it).
/// - Committed + not canceled  -> Complete.
pub fn decide(state: RequestState) -> CancellationAction {
    if state.committed {
        if state.canceled {
            CancellationAction::KeepReceipt
        } else {
            CancellationAction::Complete
        }
    } else if state.canceled {
        CancellationAction::Abort
    } else {
        CancellationAction::Complete
    }
}

/// Whether the durable receipt must be preserved (not hidden/undone).
pub fn receipt_must_survive(state: RequestState) -> bool {
    decide(state) == CancellationAction::KeepReceipt
}

/// Whether the request may be aborted without creating any memory.
pub fn may_abort(state: RequestState) -> bool {
    decide(state) == CancellationAction::Abort
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(committed: bool, canceled: bool) -> RequestState {
        RequestState {
            committed,
            canceled,
        }
    }

    #[test]
    fn canceled_before_commit_aborts() {
        // Canceled before publication: abort, no memory, no receipt.
        assert_eq!(decide(st(false, true)), CancellationAction::Abort);
        assert!(may_abort(st(false, true)));
        assert!(!receipt_must_survive(st(false, true)));
    }

    #[test]
    fn committed_receipt_survives_late_cancellation() {
        // Canceled after publication: the durable receipt must survive.
        assert_eq!(decide(st(true, true)), CancellationAction::KeepReceipt);
        assert!(receipt_must_survive(st(true, true)));
        assert!(!may_abort(st(true, true)));
    }

    #[test]
    fn normal_committed_request_completes() {
        assert_eq!(decide(st(true, false)), CancellationAction::Complete);
    }

    #[test]
    fn normal_uncommitted_request_completes() {
        assert_eq!(decide(st(false, false)), CancellationAction::Complete);
    }
}
