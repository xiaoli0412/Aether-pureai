//! Per-request retry budgets for "HTTP 200 without visible output" upstream
//! responses (e.g. Gemini risk-control blocks proxied through passthrough
//! upstreams). The tracker counts how many times a single gateway request has
//! observed an empty success response so the execution runtime can decide
//! between retrying and applying the provider's configured exhaustion action.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::orchestration::{LocalEmptyResponseExhaustion, LocalEmptyResponsePolicy};

const EMPTY_RESPONSE_BUDGET_TTL: Duration = Duration::from_secs(300);
const EMPTY_RESPONSE_BUDGET_MAX_ENTRIES: usize = 100_000;

#[derive(Debug)]
struct EmptyResponseBudgetEntry {
    count: u64,
    touched_at: Instant,
}

/// Counts empty-success observations per gateway request id.
#[derive(Debug, Default)]
pub(crate) struct EmptyResponseBudgetTracker {
    entries: Mutex<HashMap<String, EmptyResponseBudgetEntry>>,
}

impl EmptyResponseBudgetTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records one empty-success observation and returns the total number of
    /// observations for the request (including this one).
    pub(crate) fn record_empty_response(&self, request_id: &str) -> u64 {
        let mut entries = self.entries.lock();
        if entries.len() >= EMPTY_RESPONSE_BUDGET_MAX_ENTRIES {
            prune_locked_entries(&mut entries, EMPTY_RESPONSE_BUDGET_TTL);
        }
        let entry = entries
            .entry(request_id.to_string())
            .or_insert(EmptyResponseBudgetEntry {
                count: 0,
                touched_at: Instant::now(),
            });
        entry.count += 1;
        entry.touched_at = Instant::now();
        entry.count
    }

    /// Drops the budget once a request reached a terminal state.
    pub(crate) fn forget(&self, request_id: &str) {
        self.entries.lock().remove(request_id);
    }

    pub(crate) fn prune_stale(&self) {
        prune_locked_entries(&mut self.entries.lock(), EMPTY_RESPONSE_BUDGET_TTL);
    }

    #[cfg(test)]
    pub(crate) fn prune_entries_older_than(&self, max_age: Duration) {
        prune_locked_entries(&mut self.entries.lock(), max_age);
    }
}

fn prune_locked_entries(
    entries: &mut HashMap<String, EmptyResponseBudgetEntry>,
    max_age: Duration,
) {
    let now = Instant::now();
    entries.retain(|_, entry| now.duration_since(entry.touched_at) < max_age);
}

/// What the execution runtime should do with an observed empty success
/// response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptySuccessAction {
    /// No policy applies to this format/response; continue the normal flow.
    None,
    /// Rewrite the response into a retryable upstream failure so the
    /// candidate loop tries the next candidate (legacy Gemini behavior).
    RewriteRetryable,
    /// Deliver the empty success response to the client unchanged.
    Passthrough,
}

/// Decides how an observed empty success response is handled given the
/// provider's empty-response policy and how many empty observations this
/// request already collected (excluding the current one).
///
/// `provider_format_is_gemini_generate_content` keeps the legacy Gemini
/// detection active even without explicit configuration; other formats only
/// participate when `detect` is enabled.
pub(crate) fn empty_success_action(
    provider_format_is_gemini_generate_content: bool,
    policy: Option<&LocalEmptyResponsePolicy>,
    empty_attempts_before_this: u64,
) -> EmptySuccessAction {
    let detect = provider_format_is_gemini_generate_content
        || policy.is_some_and(|policy| policy.detect);
    if !detect {
        return EmptySuccessAction::None;
    }
    let budget = policy
        .map(|policy| policy.max_attempts)
        .unwrap_or(u64::MAX);
    if empty_attempts_before_this < budget {
        return EmptySuccessAction::RewriteRetryable;
    }
    match policy.map(|policy| policy.on_exhausted) {
        Some(LocalEmptyResponseExhaustion::Passthrough) => EmptySuccessAction::Passthrough,
        Some(LocalEmptyResponseExhaustion::Error) | None => EmptySuccessAction::RewriteRetryable,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        empty_success_action, EmptyResponseBudgetTracker, EmptySuccessAction,
    };
    use crate::orchestration::{LocalEmptyResponseExhaustion, LocalEmptyResponsePolicy};

    fn policy(
        detect: bool,
        max_attempts: u64,
        on_exhausted: LocalEmptyResponseExhaustion,
    ) -> LocalEmptyResponsePolicy {
        LocalEmptyResponsePolicy {
            detect,
            max_attempts,
            on_exhausted,
        }
    }

    #[test]
    fn budget_tracker_counts_per_request_and_forgets() {
        let tracker = EmptyResponseBudgetTracker::new();
        assert_eq!(tracker.record_empty_response("req-1"), 1);
        assert_eq!(tracker.record_empty_response("req-1"), 2);
        assert_eq!(tracker.record_empty_response("req-2"), 1);
        tracker.forget("req-1");
        assert_eq!(tracker.record_empty_response("req-1"), 1);
    }

    #[test]
    fn budget_tracker_prunes_stale_entries() {
        let tracker = EmptyResponseBudgetTracker::new();
        tracker.record_empty_response("req-old");
        tracker.prune_entries_older_than(Duration::ZERO);
        assert_eq!(tracker.record_empty_response("req-old"), 1);
    }

    #[test]
    fn gemini_format_without_policy_keeps_legacy_retry() {
        assert_eq!(
            empty_success_action(true, None, 0),
            EmptySuccessAction::RewriteRetryable
        );
        // Legacy behavior retries without a configured budget.
        assert_eq!(
            empty_success_action(true, None, 50),
            EmptySuccessAction::RewriteRetryable
        );
    }

    #[test]
    fn non_gemini_format_without_policy_is_untouched() {
        assert_eq!(
            empty_success_action(false, None, 0),
            EmptySuccessAction::None
        );
    }

    #[test]
    fn detect_policy_retries_within_budget_then_passthrough() {
        let policy = policy(true, 1, LocalEmptyResponseExhaustion::Passthrough);
        assert_eq!(
            empty_success_action(false, Some(&policy), 0),
            EmptySuccessAction::RewriteRetryable
        );
        assert_eq!(
            empty_success_action(false, Some(&policy), 1),
            EmptySuccessAction::Passthrough
        );
    }

    #[test]
    fn detect_policy_with_error_exhaustion_keeps_retrying() {
        let policy = policy(true, 1, LocalEmptyResponseExhaustion::Error);
        assert_eq!(
            empty_success_action(false, Some(&policy), 1),
            EmptySuccessAction::RewriteRetryable
        );
    }

    #[test]
    fn detect_disabled_skips_non_gemini_formats() {
        let policy = policy(false, 1, LocalEmptyResponseExhaustion::Passthrough);
        assert_eq!(
            empty_success_action(false, Some(&policy), 0),
            EmptySuccessAction::None
        );
        // Gemini format stays detected even with detect=false.
        assert_eq!(
            empty_success_action(true, Some(&policy), 0),
            EmptySuccessAction::RewriteRetryable
        );
    }

    #[test]
    fn gemini_format_honors_configured_passthrough_budget() {
        let policy = policy(true, 2, LocalEmptyResponseExhaustion::Passthrough);
        assert_eq!(
            empty_success_action(true, Some(&policy), 1),
            EmptySuccessAction::RewriteRetryable
        );
        assert_eq!(
            empty_success_action(true, Some(&policy), 2),
            EmptySuccessAction::Passthrough
        );
    }

    #[test]
    fn zero_budget_passthroughs_immediately() {
        let policy = policy(true, 0, LocalEmptyResponseExhaustion::Passthrough);
        assert_eq!(
            empty_success_action(false, Some(&policy), 0),
            EmptySuccessAction::Passthrough
        );
    }
}
