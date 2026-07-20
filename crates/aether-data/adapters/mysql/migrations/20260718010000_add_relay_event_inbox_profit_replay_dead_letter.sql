-- Preserve malformed usage_settled facts for investigation without allowing
-- them to block durable profit replay for later valid events.
ALTER TABLE relay_event_inbox
    ADD COLUMN profit_replay_state VARCHAR(16) NOT NULL DEFAULT 'eligible'
        CHECK (profit_replay_state IN ('eligible', 'invalid')),
    ADD COLUMN profit_replay_error TEXT NULL;

DROP INDEX idx_relay_event_inbox_pending_profit ON relay_event_inbox;
CREATE INDEX idx_relay_event_inbox_pending_profit
ON relay_event_inbox(
    instance_id,
    event_type,
    profit_status,
    profit_replay_state,
    occurred_at,
    event_id
);
