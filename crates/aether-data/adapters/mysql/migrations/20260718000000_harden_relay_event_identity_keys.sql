-- Relay event identifiers are opaque protocol keys. Keep every identity used
-- by inbox/cursor/outbox persistence binary so case variants stay isolated.
ALTER TABLE relay_event_inbox
    MODIFY instance_id VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
    MODIFY event_id VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL;

ALTER TABLE relay_event_cursors
    MODIFY instance_id VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL;

ALTER TABLE relay_event_outbox
    MODIFY instance_id VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
    MODIFY event_id VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL;
