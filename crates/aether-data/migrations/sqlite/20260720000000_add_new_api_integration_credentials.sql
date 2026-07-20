CREATE TABLE new_api_integration_credentials (
    instance_id                         TEXT PRIMARY KEY,
    current_control_secret_ciphertext   TEXT NOT NULL,
    previous_control_secret_ciphertext  TEXT,
    current_relay_secret_ciphertext     TEXT NOT NULL,
    previous_relay_secret_ciphertext    TEXT,
    transition_expires_at_unix_ms       INTEGER,
    rotation_id                         TEXT NOT NULL,
    last_rotation_payload_sha256        TEXT NOT NULL CHECK (length(last_rotation_payload_sha256) = 64),
    credential_revision                 INTEGER NOT NULL CHECK (credential_revision >= 0),
    updated_at_unix_ms                  INTEGER NOT NULL,
    CHECK (
        (previous_control_secret_ciphertext IS NULL
         AND previous_relay_secret_ciphertext IS NULL
         AND transition_expires_at_unix_ms IS NULL)
        OR
        (previous_control_secret_ciphertext IS NOT NULL
         AND previous_relay_secret_ciphertext IS NOT NULL
         AND transition_expires_at_unix_ms IS NOT NULL)
    )
);
