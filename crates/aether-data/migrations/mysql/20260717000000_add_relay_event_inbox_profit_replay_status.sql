ALTER TABLE relay_event_inbox
ADD COLUMN profit_status VARCHAR(16) NOT NULL DEFAULT 'pending'
CHECK (profit_status IN ('pending', 'recorded'));

UPDATE relay_event_inbox
SET profit_status = CASE
    WHEN event_type = 'usage_settled' THEN 'pending'
    ELSE 'recorded'
END;

CREATE INDEX idx_relay_event_inbox_pending_profit
ON relay_event_inbox(instance_id, profit_status, event_type, occurred_at);
