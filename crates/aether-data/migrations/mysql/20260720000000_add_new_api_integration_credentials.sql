CREATE TABLE new_api_integration_credentials (
    instance_id                         VARCHAR(255) PRIMARY KEY,
    current_control_secret_ciphertext   TEXT NOT NULL,
    previous_control_secret_ciphertext  TEXT,
    current_relay_secret_ciphertext     TEXT NOT NULL,
    previous_relay_secret_ciphertext    TEXT,
    transition_expires_at_unix_ms       BIGINT,
    rotation_id                         VARCHAR(255) NOT NULL,
    last_rotation_payload_sha256        CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    credential_revision                 BIGINT NOT NULL,
    updated_at_unix_ms                  BIGINT NOT NULL,
    CHECK (
        (previous_control_secret_ciphertext IS NULL
         AND previous_relay_secret_ciphertext IS NULL
         AND transition_expires_at_unix_ms IS NULL)
        OR
        (previous_control_secret_ciphertext IS NOT NULL
         AND previous_relay_secret_ciphertext IS NOT NULL
         AND transition_expires_at_unix_ms IS NOT NULL)
    ),
    CHECK (credential_revision >= 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
