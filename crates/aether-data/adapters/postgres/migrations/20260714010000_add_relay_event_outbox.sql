CREATE TABLE relay_event_outbox (
    sequence           BIGSERIAL PRIMARY KEY,
    instance_id        TEXT NOT NULL,
    event_id           TEXT NOT NULL,
    event_type         TEXT NOT NULL,
    payload_json       TEXT NOT NULL,
    created_at_unix_ms BIGINT NOT NULL,
    UNIQUE (instance_id, event_id)
);

CREATE INDEX idx_relay_event_outbox_instance_sequence
    ON relay_event_outbox(instance_id, sequence);
