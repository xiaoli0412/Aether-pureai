CREATE TABLE relay_event_outbox (
    sequence           BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    instance_id        VARCHAR(255) NOT NULL,
    event_id           VARCHAR(255) NOT NULL,
    event_type         VARCHAR(128) NOT NULL,
    payload_json       TEXT NOT NULL,
    created_at_unix_ms BIGINT NOT NULL,
    UNIQUE KEY uq_relay_event_outbox_instance_event (instance_id, event_id),
    KEY idx_relay_event_outbox_instance_sequence (instance_id, sequence)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
