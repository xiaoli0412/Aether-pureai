-- Preserve malformed usage_settled facts for investigation without allowing
-- them to block durable profit replay for later valid events.
ALTER TABLE relay_event_inbox
    ADD COLUMN profit_replay_state TEXT NOT NULL DEFAULT 'eligible'
        CHECK (profit_replay_state IN ('eligible', 'invalid')),
    ADD COLUMN profit_replay_error TEXT;

DROP INDEX idx_relay_event_inbox_pending_profit;
CREATE INDEX idx_relay_event_inbox_pending_profit
ON relay_event_inbox(instance_id, occurred_at, event_id)
WHERE event_type = 'usage_settled'
  AND profit_status = 'pending'
  AND profit_replay_state = 'eligible';
