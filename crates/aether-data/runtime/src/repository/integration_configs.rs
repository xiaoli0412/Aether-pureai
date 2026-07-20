use std::fmt;

use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, SqlitePool};

use crate::{DataBackends, DataLayerError};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedIntegrationConfig {
    pub instance_id: String,
    pub route_profile: String,
    pub execution_mode: String,
    pub enabled: bool,
    pub capability_version: String,
    pub revision: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationConfigUpdate {
    pub route_profile: String,
    pub execution_mode: String,
    pub enabled: bool,
    pub capability_version: String,
    pub updated_at_unix_ms: i64,
}

const REDACTED_CREDENTIAL_VALUE: &str = "[REDACTED]";
const CREDENTIAL_CIPHERTEXT_MAX_BYTES: usize = 65_535;
const ROTATION_ID_MAX_BYTES: usize = 255;

/// Ciphertext supplied by the cryptographic boundary for durable storage.
///
/// This wrapper deliberately has no `Display` implementation and redacts its
/// value from debug and serde output. Callers can only expose the ciphertext
/// explicitly when handing it to the configured decryptor.
#[derive(Clone, PartialEq, Eq)]
pub struct OpaqueCredentialCiphertext(String);

impl OpaqueCredentialCiphertext {
    pub fn new(ciphertext: impl Into<String>) -> Result<Self, DataLayerError> {
        let ciphertext = ciphertext.into();
        if ciphertext.trim().is_empty() {
            return Err(DataLayerError::InvalidInput(
                "credential ciphertext must be non-empty".to_string(),
            ));
        }
        if ciphertext.contains('\0') {
            return Err(DataLayerError::InvalidInput(
                "credential ciphertext must not contain NUL bytes".to_string(),
            ));
        }
        if ciphertext.len() > CREDENTIAL_CIPHERTEXT_MAX_BYTES {
            return Err(DataLayerError::InvalidInput(
                "credential ciphertext is too large".to_string(),
            ));
        }
        Ok(Self(ciphertext))
    }

    /// Exposes the opaque value only at an explicit storage or decryptor boundary.
    pub fn as_ciphertext(&self) -> &str {
        &self.0
    }

    fn from_storage(ciphertext: String) -> Self {
        Self(ciphertext)
    }
}

impl fmt::Debug for OpaqueCredentialCiphertext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpaqueCredentialCiphertext([REDACTED])")
    }
}

impl Serialize for OpaqueCredentialCiphertext {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(REDACTED_CREDENTIAL_VALUE)
    }
}

/// New encrypted credential material for one idempotent rotation request.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationCredentialRotation {
    #[serde(skip_serializing)]
    control_secret_ciphertext: OpaqueCredentialCiphertext,
    #[serde(skip_serializing)]
    relay_secret_ciphertext: OpaqueCredentialCiphertext,
    pub transition_expires_at_unix_ms: Option<i64>,
    pub revoke_previous: bool,
    pub rotation_id: String,
    #[serde(skip_serializing)]
    stable_payload_sha256: String,
}

impl IntegrationCredentialRotation {
    pub fn new(
        control_secret_ciphertext: OpaqueCredentialCiphertext,
        relay_secret_ciphertext: OpaqueCredentialCiphertext,
        transition_expires_at_unix_ms: Option<i64>,
        revoke_previous: bool,
        rotation_id: impl Into<String>,
        stable_payload_sha256: impl Into<String>,
    ) -> Result<Self, DataLayerError> {
        let rotation_id = rotation_id.into();
        if rotation_id.trim().is_empty() {
            return Err(DataLayerError::InvalidInput(
                "credential rotation id must be non-empty".to_string(),
            ));
        }
        if rotation_id.contains('\0') {
            return Err(DataLayerError::InvalidInput(
                "credential rotation id must not contain NUL bytes".to_string(),
            ));
        }
        if rotation_id.len() > ROTATION_ID_MAX_BYTES {
            return Err(DataLayerError::InvalidInput(
                "credential rotation id is too large".to_string(),
            ));
        }
        let stable_payload_sha256 = stable_payload_sha256.into();
        if stable_payload_sha256.len() != 64
            || !stable_payload_sha256
                .bytes()
                .all(|value| value.is_ascii_digit() || matches!(value, b'a'..=b'f'))
        {
            return Err(DataLayerError::InvalidInput(
                "credential rotation payload digest must be a 64-character lowercase SHA-256 hex string"
                    .to_string(),
            ));
        }
        Ok(Self {
            control_secret_ciphertext,
            relay_secret_ciphertext,
            transition_expires_at_unix_ms,
            revoke_previous,
            rotation_id,
            stable_payload_sha256,
        })
    }

    /// Returns the gateway-computed digest without exposing it through serde
    /// or debug output. It must be calculated before randomized encryption.
    fn stable_payload_sha256(&self) -> &str {
        &self.stable_payload_sha256
    }
}

impl fmt::Debug for IntegrationCredentialRotation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IntegrationCredentialRotation")
            .field("control_secret_ciphertext", &REDACTED_CREDENTIAL_VALUE)
            .field("relay_secret_ciphertext", &REDACTED_CREDENTIAL_VALUE)
            .field(
                "transition_expires_at_unix_ms",
                &self.transition_expires_at_unix_ms,
            )
            .field("revoke_previous", &self.revoke_previous)
            .field("rotation_id", &self.rotation_id)
            .field("stable_payload_sha256", &REDACTED_CREDENTIAL_VALUE)
            .finish()
    }
}

/// Credential material that may establish the first durable credential row
/// after the gateway has authenticated the request with its instance-bound
/// bootstrap credentials. The previous ciphertext is intentionally not part
/// of the ordinary rotation type, so callers cannot inject it into later
/// rotations.
#[derive(Clone, PartialEq, Eq)]
pub struct BootstrapIntegrationCredentialRotation {
    bootstrap_control_secret_ciphertext: OpaqueCredentialCiphertext,
    bootstrap_relay_secret_ciphertext: OpaqueCredentialCiphertext,
    rotation: IntegrationCredentialRotation,
}

impl BootstrapIntegrationCredentialRotation {
    pub fn new(
        bootstrap_control_secret_ciphertext: OpaqueCredentialCiphertext,
        bootstrap_relay_secret_ciphertext: OpaqueCredentialCiphertext,
        rotation: IntegrationCredentialRotation,
    ) -> Result<Self, DataLayerError> {
        if rotation.revoke_previous {
            return Err(DataLayerError::InvalidInput(
                "bootstrap credential rotation cannot revoke bootstrap credentials".to_string(),
            ));
        }
        if rotation.transition_expires_at_unix_ms.is_none() {
            return Err(DataLayerError::InvalidInput(
                "bootstrap credential rotation requires a transition expiry".to_string(),
            ));
        }
        Ok(Self {
            bootstrap_control_secret_ciphertext,
            bootstrap_relay_secret_ciphertext,
            rotation,
        })
    }

    fn rotation(&self) -> &IntegrationCredentialRotation {
        &self.rotation
    }
}

impl fmt::Debug for BootstrapIntegrationCredentialRotation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapIntegrationCredentialRotation")
            .field(
                "bootstrap_control_secret_ciphertext",
                &REDACTED_CREDENTIAL_VALUE,
            )
            .field(
                "bootstrap_relay_secret_ciphertext",
                &REDACTED_CREDENTIAL_VALUE,
            )
            .field("rotation", &self.rotation)
            .finish()
    }
}

/// Persisted credential state. Secret ciphertext is never emitted by `Debug`
/// or serde serialization.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct PersistedIntegrationCredentials {
    pub instance_id: String,
    #[serde(skip_serializing)]
    pub current_control_secret_ciphertext: OpaqueCredentialCiphertext,
    #[serde(skip_serializing)]
    pub previous_control_secret_ciphertext: Option<OpaqueCredentialCiphertext>,
    #[serde(skip_serializing)]
    pub current_relay_secret_ciphertext: OpaqueCredentialCiphertext,
    #[serde(skip_serializing)]
    pub previous_relay_secret_ciphertext: Option<OpaqueCredentialCiphertext>,
    pub transition_expires_at_unix_ms: Option<i64>,
    pub rotation_id: String,
    #[serde(skip_serializing)]
    last_rotation_payload_sha256: String,
    pub credential_revision: i64,
    pub updated_at_unix_ms: i64,
}

impl fmt::Debug for PersistedIntegrationCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistedIntegrationCredentials")
            .field("instance_id", &self.instance_id)
            .field(
                "current_control_secret_ciphertext",
                &REDACTED_CREDENTIAL_VALUE,
            )
            .field(
                "previous_control_secret_ciphertext",
                &self
                    .previous_control_secret_ciphertext
                    .as_ref()
                    .map(|_| REDACTED_CREDENTIAL_VALUE),
            )
            .field(
                "current_relay_secret_ciphertext",
                &REDACTED_CREDENTIAL_VALUE,
            )
            .field(
                "previous_relay_secret_ciphertext",
                &self
                    .previous_relay_secret_ciphertext
                    .as_ref()
                    .map(|_| REDACTED_CREDENTIAL_VALUE),
            )
            .field(
                "transition_expires_at_unix_ms",
                &self.transition_expires_at_unix_ms,
            )
            .field("rotation_id", &self.rotation_id)
            .field("last_rotation_payload_sha256", &REDACTED_CREDENTIAL_VALUE)
            .field("credential_revision", &self.credential_revision)
            .field("updated_at_unix_ms", &self.updated_at_unix_ms)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationConfigCasResult {
    Applied(PersistedIntegrationConfig),
    Conflict(Option<PersistedIntegrationConfig>),
}

#[derive(Clone)]
enum IntegrationConfigBackend {
    Postgres(sqlx::PgPool),
    Mysql(MySqlPool),
    Sqlite(SqlitePool),
}

#[derive(Debug, PartialEq, Eq)]
enum MysqlInitialInsertOutcome {
    Applied,
    Conflict,
}

fn mysql_initial_insert_outcome<T>(
    result: Result<T, sqlx::Error>,
) -> Result<MysqlInitialInsertOutcome, sqlx::Error> {
    match result {
        Ok(_) => Ok(MysqlInitialInsertOutcome::Applied),
        Err(error) if is_mysql_duplicate_key_error(&error) => {
            Ok(MysqlInitialInsertOutcome::Conflict)
        }
        Err(error) => Err(error),
    }
}

fn is_mysql_duplicate_key_error(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error) if database_error.is_unique_violation()
    )
}

const ROUTE_PROFILE_OUTBOX_EVENT_TYPE: &str = "route_decision_changed";
const DEFAULT_ROUTE_PROFILE: &str = "default";

struct RouteProfileOutboxEvent {
    event_id: String,
    payload_json: String,
    created_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionalCasOutcome {
    Applied,
    Conflict,
}

type IntegrationCredentialRow = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    String,
    String,
    i64,
    i64,
);

struct ExistingCredentialState {
    current_control_secret_ciphertext: String,
    current_relay_secret_ciphertext: String,
    rotation_id: String,
    last_rotation_payload_sha256: String,
    credential_revision: i64,
}

struct CredentialRotationWriteValues<'a> {
    previous_control_secret_ciphertext: Option<&'a str>,
    previous_relay_secret_ciphertext: Option<&'a str>,
    transition_expires_at_unix_ms: Option<i64>,
}

fn persisted_credentials_from_row(
    instance_id: &str,
    (
        current_control_secret_ciphertext,
        previous_control_secret_ciphertext,
        current_relay_secret_ciphertext,
        previous_relay_secret_ciphertext,
        transition_expires_at_unix_ms,
        rotation_id,
        last_rotation_payload_sha256,
        credential_revision,
        updated_at_unix_ms,
    ): IntegrationCredentialRow,
) -> PersistedIntegrationCredentials {
    PersistedIntegrationCredentials {
        instance_id: instance_id.to_string(),
        current_control_secret_ciphertext: OpaqueCredentialCiphertext::from_storage(
            current_control_secret_ciphertext,
        ),
        previous_control_secret_ciphertext: previous_control_secret_ciphertext
            .map(OpaqueCredentialCiphertext::from_storage),
        current_relay_secret_ciphertext: OpaqueCredentialCiphertext::from_storage(
            current_relay_secret_ciphertext,
        ),
        previous_relay_secret_ciphertext: previous_relay_secret_ciphertext
            .map(OpaqueCredentialCiphertext::from_storage),
        transition_expires_at_unix_ms,
        rotation_id,
        last_rotation_payload_sha256,
        credential_revision,
        updated_at_unix_ms,
    }
}

fn credential_transition_is_valid(
    existing: Option<&ExistingCredentialState>,
    rotation: &IntegrationCredentialRotation,
    updated_at_unix_ms: i64,
) -> Result<(), DataLayerError> {
    match (
        existing,
        rotation.revoke_previous,
        rotation.transition_expires_at_unix_ms,
    ) {
        (None, false, None) => Ok(()),
        (None, true, _) => Err(DataLayerError::InvalidInput(
            "initial credentials cannot revoke a previous credential".to_string(),
        )),
        (None, false, Some(_)) => Err(DataLayerError::InvalidInput(
            "initial credentials must not include a transition expiry".to_string(),
        )),
        (Some(_), true, None) => Ok(()),
        (Some(_), true, Some(_)) => Err(DataLayerError::InvalidInput(
            "revoked credentials must not include a transition expiry".to_string(),
        )),
        (Some(_), false, None) => Err(DataLayerError::InvalidInput(
            "credential rotation requires a transition expiry".to_string(),
        )),
        (Some(_), false, Some(expires_at_unix_ms)) if expires_at_unix_ms <= updated_at_unix_ms => {
            Err(DataLayerError::InvalidInput(
                "credential transition expiry must be later than the update time".to_string(),
            ))
        }
        (Some(_), false, Some(_)) => Ok(()),
    }
}

fn credential_rotation_write_values<'a>(
    existing: Option<&'a ExistingCredentialState>,
    rotation: &'a IntegrationCredentialRotation,
    bootstrap: Option<&'a BootstrapIntegrationCredentialRotation>,
    updated_at_unix_ms: i64,
) -> Result<CredentialRotationWriteValues<'a>, DataLayerError> {
    if let Some(bootstrap) = bootstrap {
        if existing.is_some() {
            return Err(DataLayerError::InvalidInput(
                "bootstrap credential rotation requires credentials to be absent".to_string(),
            ));
        }
        if rotation.revoke_previous {
            return Err(DataLayerError::InvalidInput(
                "bootstrap credential rotation cannot revoke bootstrap credentials".to_string(),
            ));
        }
        let transition_expires_at_unix_ms =
            rotation.transition_expires_at_unix_ms.ok_or_else(|| {
                DataLayerError::InvalidInput(
                    "bootstrap credential rotation requires a transition expiry".to_string(),
                )
            })?;
        if transition_expires_at_unix_ms <= updated_at_unix_ms {
            return Err(DataLayerError::InvalidInput(
                "bootstrap credential transition expiry must be later than the update time"
                    .to_string(),
            ));
        }
        return Ok(CredentialRotationWriteValues {
            previous_control_secret_ciphertext: Some(
                bootstrap
                    .bootstrap_control_secret_ciphertext
                    .as_ciphertext(),
            ),
            previous_relay_secret_ciphertext: Some(
                bootstrap.bootstrap_relay_secret_ciphertext.as_ciphertext(),
            ),
            transition_expires_at_unix_ms: Some(transition_expires_at_unix_ms),
        });
    }

    credential_transition_is_valid(existing, rotation, updated_at_unix_ms)?;
    let retain_previous = existing.is_some() && !rotation.revoke_previous;
    Ok(CredentialRotationWriteValues {
        previous_control_secret_ciphertext: if retain_previous {
            existing.map(|credentials| credentials.current_control_secret_ciphertext.as_str())
        } else {
            None
        },
        previous_relay_secret_ciphertext: if retain_previous {
            existing.map(|credentials| credentials.current_relay_secret_ciphertext.as_str())
        } else {
            None
        },
        transition_expires_at_unix_ms: if retain_previous {
            rotation.transition_expires_at_unix_ms
        } else {
            None
        },
    })
}

fn rotation_retry_outcome(
    existing: Option<&ExistingCredentialState>,
    current_config_revision: Option<i64>,
    expected_revision: i64,
    next_revision: i64,
    rotation: &IntegrationCredentialRotation,
    payload_sha256: &str,
) -> Result<TransactionalCasOutcome, DataLayerError> {
    let Some(existing) = existing else {
        return Ok(TransactionalCasOutcome::Conflict);
    };
    if existing.rotation_id != rotation.rotation_id {
        return Ok(TransactionalCasOutcome::Conflict);
    }
    if existing.last_rotation_payload_sha256 != payload_sha256 {
        return Err(DataLayerError::InvalidInput(
            "credential rotation id is already bound to a different payload".to_string(),
        ));
    }
    if current_config_revision == Some(next_revision)
        && existing.credential_revision == next_revision
        && next_revision == expected_revision.saturating_add(1)
    {
        return Ok(TransactionalCasOutcome::Applied);
    }
    Ok(TransactionalCasOutcome::Conflict)
}

#[derive(Clone)]
pub struct IntegrationConfigStore {
    backend: IntegrationConfigBackend,
}

impl IntegrationConfigStore {
    pub fn from_backends(backends: &DataBackends) -> Option<Self> {
        backends
            .postgres()
            .map(|backend| Self {
                backend: IntegrationConfigBackend::Postgres(backend.pool_clone()),
            })
            .or_else(|| {
                backends.mysql().map(|backend| Self {
                    backend: IntegrationConfigBackend::Mysql(backend.pool_clone()),
                })
            })
            .or_else(|| {
                backends.sqlite().map(|backend| Self {
                    backend: IntegrationConfigBackend::Sqlite(backend.pool_clone()),
                })
            })
    }

    pub fn sqlite(pool: SqlitePool) -> Self {
        Self {
            backend: IntegrationConfigBackend::Sqlite(pool),
        }
    }

    pub async fn get(
        &self,
        instance_id: &str,
    ) -> Result<Option<PersistedIntegrationConfig>, DataLayerError> {
        let row: Option<(String, String, bool, String, i64, i64)> = match &self.backend {
            IntegrationConfigBackend::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT route_profile, execution_mode, enabled, capability_version,
                        revision, updated_at_unix_ms
                 FROM new_api_integration_configs WHERE instance_id = $1",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
            IntegrationConfigBackend::Mysql(pool) => {
                sqlx::query_as(
                    "SELECT route_profile, execution_mode, enabled, capability_version,
                        revision, updated_at_unix_ms
                 FROM new_api_integration_configs WHERE instance_id = ?",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
            IntegrationConfigBackend::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT route_profile, execution_mode, enabled, capability_version,
                        revision, updated_at_unix_ms
                 FROM new_api_integration_configs WHERE instance_id = ?",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        Ok(row.map(
            |(
                route_profile,
                execution_mode,
                enabled,
                capability_version,
                revision,
                updated_at_unix_ms,
            )| PersistedIntegrationConfig {
                instance_id: instance_id.to_string(),
                route_profile,
                execution_mode,
                enabled,
                capability_version,
                revision,
                updated_at_unix_ms,
            },
        ))
    }

    pub async fn get_credentials(
        &self,
        instance_id: &str,
    ) -> Result<Option<PersistedIntegrationCredentials>, DataLayerError> {
        let row: Option<IntegrationCredentialRow> = match &self.backend {
            IntegrationConfigBackend::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT current_control_secret_ciphertext,
                        previous_control_secret_ciphertext,
                        current_relay_secret_ciphertext,
                        previous_relay_secret_ciphertext,
                        transition_expires_at_unix_ms,
                        rotation_id,
                        last_rotation_payload_sha256,
                        credential_revision,
                        updated_at_unix_ms
                 FROM new_api_integration_credentials
                 WHERE instance_id = $1",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
            IntegrationConfigBackend::Mysql(pool) => {
                sqlx::query_as(
                    "SELECT current_control_secret_ciphertext,
                        previous_control_secret_ciphertext,
                        current_relay_secret_ciphertext,
                        previous_relay_secret_ciphertext,
                        transition_expires_at_unix_ms,
                        rotation_id,
                        last_rotation_payload_sha256,
                        credential_revision,
                        updated_at_unix_ms
                 FROM new_api_integration_credentials
                 WHERE instance_id = ?",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
            IntegrationConfigBackend::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT current_control_secret_ciphertext,
                        previous_control_secret_ciphertext,
                        current_relay_secret_ciphertext,
                        previous_relay_secret_ciphertext,
                        transition_expires_at_unix_ms,
                        rotation_id,
                        last_rotation_payload_sha256,
                        credential_revision,
                        updated_at_unix_ms
                 FROM new_api_integration_credentials
                 WHERE instance_id = ?",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        Ok(row.map(|row| persisted_credentials_from_row(instance_id, row)))
    }

    pub async fn compare_and_set(
        &self,
        instance_id: &str,
        expected_revision: i64,
        update: &IntegrationConfigUpdate,
    ) -> Result<IntegrationConfigCasResult, DataLayerError> {
        if expected_revision < 0 {
            return Err(DataLayerError::InvalidInput(
                "integration config revision must be non-negative".to_string(),
            ));
        }
        let next_revision = expected_revision.checked_add(1).ok_or_else(|| {
            DataLayerError::InvalidInput("integration config revision overflow".to_string())
        })?;

        let rows_affected = if expected_revision == 0 {
            match &self.backend {
                IntegrationConfigBackend::Postgres(pool) => sqlx::query(
                    "INSERT INTO new_api_integration_configs
                     (instance_id, route_profile, execution_mode, enabled, capability_version,
                      revision, updated_at_unix_ms)
                     VALUES ($1, $2, $3, $4, $5, 1, $6)
                     ON CONFLICT (instance_id) DO NOTHING",
                )
                .bind(instance_id)
                .bind(&update.route_profile)
                .bind(&update.execution_mode)
                .bind(update.enabled)
                .bind(&update.capability_version)
                .bind(update.updated_at_unix_ms)
                .execute(pool)
                .await
                .map(|result| result.rows_affected()),
                IntegrationConfigBackend::Mysql(pool) => {
                    // The primary key arbitrates concurrent initial writes atomically.
                    let result = sqlx::query(
                        "INSERT INTO new_api_integration_configs
                         (instance_id, route_profile, execution_mode, enabled, capability_version,
                          revision, updated_at_unix_ms)
                         VALUES (?, ?, ?, ?, ?, 1, ?)",
                    )
                    .bind(instance_id)
                    .bind(&update.route_profile)
                    .bind(&update.execution_mode)
                    .bind(update.enabled)
                    .bind(&update.capability_version)
                    .bind(update.updated_at_unix_ms)
                    .execute(pool)
                    .await;

                    match mysql_initial_insert_outcome(result) {
                        Ok(MysqlInitialInsertOutcome::Applied) => Ok(1),
                        Ok(MysqlInitialInsertOutcome::Conflict) => {
                            return Ok(IntegrationConfigCasResult::Conflict(
                                self.get(instance_id).await?,
                            ));
                        }
                        Err(error) => Err(error),
                    }
                }
                IntegrationConfigBackend::Sqlite(pool) => sqlx::query(
                    "INSERT INTO new_api_integration_configs
                     (instance_id, route_profile, execution_mode, enabled, capability_version,
                      revision, updated_at_unix_ms)
                     VALUES (?, ?, ?, ?, ?, 1, ?)
                     ON CONFLICT(instance_id) DO NOTHING",
                )
                .bind(instance_id)
                .bind(&update.route_profile)
                .bind(&update.execution_mode)
                .bind(update.enabled)
                .bind(&update.capability_version)
                .bind(update.updated_at_unix_ms)
                .execute(pool)
                .await
                .map(|result| result.rows_affected()),
            }
        } else {
            match &self.backend {
                IntegrationConfigBackend::Postgres(pool) => sqlx::query(
                    "UPDATE new_api_integration_configs
                     SET route_profile = $1, execution_mode = $2, enabled = $3,
                         capability_version = $4, revision = $5, updated_at_unix_ms = $6
                     WHERE instance_id = $7 AND revision = $8",
                )
                .bind(&update.route_profile)
                .bind(&update.execution_mode)
                .bind(update.enabled)
                .bind(&update.capability_version)
                .bind(next_revision)
                .bind(update.updated_at_unix_ms)
                .bind(instance_id)
                .bind(expected_revision)
                .execute(pool)
                .await
                .map(|result| result.rows_affected()),
                IntegrationConfigBackend::Mysql(pool) => sqlx::query(
                    "UPDATE new_api_integration_configs
                     SET route_profile = ?, execution_mode = ?, enabled = ?,
                         capability_version = ?, revision = ?, updated_at_unix_ms = ?
                     WHERE instance_id = ? AND revision = ?",
                )
                .bind(&update.route_profile)
                .bind(&update.execution_mode)
                .bind(update.enabled)
                .bind(&update.capability_version)
                .bind(next_revision)
                .bind(update.updated_at_unix_ms)
                .bind(instance_id)
                .bind(expected_revision)
                .execute(pool)
                .await
                .map(|result| result.rows_affected()),
                IntegrationConfigBackend::Sqlite(pool) => sqlx::query(
                    "UPDATE new_api_integration_configs
                     SET route_profile = ?, execution_mode = ?, enabled = ?,
                         capability_version = ?, revision = ?, updated_at_unix_ms = ?
                     WHERE instance_id = ? AND revision = ?",
                )
                .bind(&update.route_profile)
                .bind(&update.execution_mode)
                .bind(update.enabled)
                .bind(&update.capability_version)
                .bind(next_revision)
                .bind(update.updated_at_unix_ms)
                .bind(instance_id)
                .bind(expected_revision)
                .execute(pool)
                .await
                .map(|result| result.rows_affected()),
            }
        }
        .map_err(DataLayerError::sql)?;

        if rows_affected == 0 {
            return Ok(IntegrationConfigCasResult::Conflict(
                self.get(instance_id).await?,
            ));
        }

        Ok(IntegrationConfigCasResult::Applied(
            PersistedIntegrationConfig {
                instance_id: instance_id.to_string(),
                route_profile: update.route_profile.clone(),
                execution_mode: update.execution_mode.clone(),
                enabled: update.enabled,
                capability_version: update.capability_version.clone(),
                revision: next_revision,
                updated_at_unix_ms: update.updated_at_unix_ms,
            },
        ))
    }

    /// Applies a configuration revision and a credential rotation in the same
    /// database transaction. The credential revision is the committed config
    /// revision, so a remote retry can acknowledge one durable state.
    pub async fn compare_and_set_with_credential_rotation(
        &self,
        instance_id: &str,
        expected_revision: i64,
        update: &IntegrationConfigUpdate,
        rotation: &IntegrationCredentialRotation,
    ) -> Result<IntegrationConfigCasResult, DataLayerError> {
        if expected_revision < 0 {
            return Err(DataLayerError::InvalidInput(
                "integration config revision must be non-negative".to_string(),
            ));
        }
        let next_revision = expected_revision.checked_add(1).ok_or_else(|| {
            DataLayerError::InvalidInput("integration config revision overflow".to_string())
        })?;
        let payload_sha256 = rotation.stable_payload_sha256();

        let outcome = match &self.backend {
            IntegrationConfigBackend::Postgres(pool) => {
                compare_and_set_postgres_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    None,
                    None,
                )
                .await?
            }
            IntegrationConfigBackend::Mysql(pool) => {
                compare_and_set_mysql_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    None,
                    None,
                )
                .await?
            }
            IntegrationConfigBackend::Sqlite(pool) => {
                compare_and_set_sqlite_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    None,
                    None,
                )
                .await?
            }
        };

        if outcome == TransactionalCasOutcome::Conflict {
            return Ok(IntegrationConfigCasResult::Conflict(
                self.get(instance_id).await?,
            ));
        }

        let persisted = self.get(instance_id).await?.ok_or_else(|| {
            DataLayerError::InvalidInput(
                "credential rotation completed without a persisted config".to_string(),
            )
        })?;
        Ok(IntegrationConfigCasResult::Applied(persisted))
    }

    /// Applies a configuration revision, credential rotation, and any required
    /// route-profile transition event in one database transaction.
    pub async fn compare_and_set_with_route_profile_outbox_and_credential_rotation(
        &self,
        instance_id: &str,
        expected_revision: i64,
        update: &IntegrationConfigUpdate,
        rotation: &IntegrationCredentialRotation,
    ) -> Result<IntegrationConfigCasResult, DataLayerError> {
        if expected_revision < 0 {
            return Err(DataLayerError::InvalidInput(
                "integration config revision must be non-negative".to_string(),
            ));
        }
        let next_revision = expected_revision.checked_add(1).ok_or_else(|| {
            DataLayerError::InvalidInput("integration config revision overflow".to_string())
        })?;
        let current = self.get(instance_id).await?;
        let event = route_profile_outbox_event(
            current.as_ref(),
            instance_id,
            expected_revision,
            next_revision,
            update,
        )?;
        let payload_sha256 = rotation.stable_payload_sha256();

        let outcome = match &self.backend {
            IntegrationConfigBackend::Postgres(pool) => {
                compare_and_set_postgres_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    event.as_ref(),
                    None,
                )
                .await?
            }
            IntegrationConfigBackend::Mysql(pool) => {
                compare_and_set_mysql_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    event.as_ref(),
                    None,
                )
                .await?
            }
            IntegrationConfigBackend::Sqlite(pool) => {
                compare_and_set_sqlite_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    event.as_ref(),
                    None,
                )
                .await?
            }
        };

        if outcome == TransactionalCasOutcome::Conflict {
            return Ok(IntegrationConfigCasResult::Conflict(
                self.get(instance_id).await?,
            ));
        }

        let persisted = self.get(instance_id).await?.ok_or_else(|| {
            DataLayerError::InvalidInput(
                "credential rotation completed without a persisted config".to_string(),
            )
        })?;
        Ok(IntegrationConfigCasResult::Applied(persisted))
    }

    /// Establishes the first durable credentials after the gateway has
    /// authenticated an instance-bound bootstrap control request. This is
    /// deliberately distinct from the ordinary rotation API: it is valid only
    /// while no credential row exists, and preserves the verified bootstrap
    /// credentials as the expiring previous pair.
    pub async fn compare_and_set_with_route_profile_outbox_and_bootstrap_credential_rotation(
        &self,
        instance_id: &str,
        expected_revision: i64,
        update: &IntegrationConfigUpdate,
        bootstrap: &BootstrapIntegrationCredentialRotation,
    ) -> Result<IntegrationConfigCasResult, DataLayerError> {
        if expected_revision < 0 {
            return Err(DataLayerError::InvalidInput(
                "integration config revision must be non-negative".to_string(),
            ));
        }
        let next_revision = expected_revision.checked_add(1).ok_or_else(|| {
            DataLayerError::InvalidInput("integration config revision overflow".to_string())
        })?;
        let current = self.get(instance_id).await?;
        let event = route_profile_outbox_event(
            current.as_ref(),
            instance_id,
            expected_revision,
            next_revision,
            update,
        )?;
        let rotation = bootstrap.rotation();
        let payload_sha256 = rotation.stable_payload_sha256();

        let outcome = match &self.backend {
            IntegrationConfigBackend::Postgres(pool) => {
                compare_and_set_postgres_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    event.as_ref(),
                    Some(bootstrap),
                )
                .await?
            }
            IntegrationConfigBackend::Mysql(pool) => {
                compare_and_set_mysql_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    event.as_ref(),
                    Some(bootstrap),
                )
                .await?
            }
            IntegrationConfigBackend::Sqlite(pool) => {
                compare_and_set_sqlite_with_credential_rotation(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    rotation,
                    payload_sha256,
                    event.as_ref(),
                    Some(bootstrap),
                )
                .await?
            }
        };

        if outcome == TransactionalCasOutcome::Conflict {
            return Ok(IntegrationConfigCasResult::Conflict(
                self.get(instance_id).await?,
            ));
        }

        let persisted = self.get(instance_id).await?.ok_or_else(|| {
            DataLayerError::InvalidInput(
                "bootstrap credential rotation completed without a persisted config".to_string(),
            )
        })?;
        Ok(IntegrationConfigCasResult::Applied(persisted))
    }

    /// Atomically applies a route-profile revision and emits the corresponding
    /// durable routing-change event when the profile actually changes.
    pub async fn compare_and_set_with_route_profile_outbox(
        &self,
        instance_id: &str,
        expected_revision: i64,
        update: &IntegrationConfigUpdate,
    ) -> Result<IntegrationConfigCasResult, DataLayerError> {
        if expected_revision < 0 {
            return Err(DataLayerError::InvalidInput(
                "integration config revision must be non-negative".to_string(),
            ));
        }
        let next_revision = expected_revision.checked_add(1).ok_or_else(|| {
            DataLayerError::InvalidInput("integration config revision overflow".to_string())
        })?;
        let current = self.get(instance_id).await?;
        let event = route_profile_outbox_event(
            current.as_ref(),
            instance_id,
            expected_revision,
            next_revision,
            update,
        )?;

        let outcome = match &self.backend {
            IntegrationConfigBackend::Postgres(pool) => {
                compare_and_set_postgres_with_route_profile_outbox(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    event.as_ref(),
                )
                .await?
            }
            IntegrationConfigBackend::Mysql(pool) => {
                compare_and_set_mysql_with_route_profile_outbox(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    event.as_ref(),
                )
                .await?
            }
            IntegrationConfigBackend::Sqlite(pool) => {
                compare_and_set_sqlite_with_route_profile_outbox(
                    pool,
                    instance_id,
                    expected_revision,
                    next_revision,
                    update,
                    event.as_ref(),
                )
                .await?
            }
        };

        if outcome == TransactionalCasOutcome::Conflict {
            return Ok(IntegrationConfigCasResult::Conflict(
                self.get(instance_id).await?,
            ));
        }

        Ok(IntegrationConfigCasResult::Applied(
            PersistedIntegrationConfig {
                instance_id: instance_id.to_string(),
                route_profile: update.route_profile.clone(),
                execution_mode: update.execution_mode.clone(),
                enabled: update.enabled,
                capability_version: update.capability_version.clone(),
                revision: next_revision,
                updated_at_unix_ms: update.updated_at_unix_ms,
            },
        ))
    }
}

fn route_profile_outbox_event(
    current: Option<&PersistedIntegrationConfig>,
    instance_id: &str,
    expected_revision: i64,
    next_revision: i64,
    update: &IntegrationConfigUpdate,
) -> Result<Option<RouteProfileOutboxEvent>, DataLayerError> {
    let previous_route_profile = match current {
        Some(current)
            if current.revision == expected_revision
                && current.route_profile != update.route_profile =>
        {
            current.route_profile.as_str()
        }
        None if expected_revision == 0 && update.route_profile != DEFAULT_ROUTE_PROFILE => {
            DEFAULT_ROUTE_PROFILE
        }
        _ => return Ok(None),
    };

    let payload_json = serde_json::to_string(&serde_json::json!({
        "instance_id": instance_id,
        "previous_route_profile": previous_route_profile,
        "route_profile": update.route_profile.as_str(),
        "revision": next_revision,
    }))
    .map_err(|error| {
        DataLayerError::InvalidInput(format!("serialize route profile outbox event: {error}"))
    })?;
    Ok(Some(RouteProfileOutboxEvent {
        event_id: format!("route-profile:{instance_id}:{next_revision}"),
        payload_json,
        created_at_unix_ms: update.updated_at_unix_ms,
    }))
}

async fn compare_and_set_postgres_with_route_profile_outbox(
    pool: &sqlx::PgPool,
    instance_id: &str,
    expected_revision: i64,
    next_revision: i64,
    update: &IntegrationConfigUpdate,
    event: Option<&RouteProfileOutboxEvent>,
) -> Result<TransactionalCasOutcome, DataLayerError> {
    let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
    let rows_affected = if expected_revision == 0 {
        sqlx::query(
            "INSERT INTO new_api_integration_configs
             (instance_id, route_profile, execution_mode, enabled, capability_version,
              revision, updated_at_unix_ms)
             VALUES ($1, $2, $3, $4, $5, 1, $6)
             ON CONFLICT (instance_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(update.updated_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    } else {
        sqlx::query(
            "UPDATE new_api_integration_configs
             SET route_profile = $1, execution_mode = $2, enabled = $3,
                 capability_version = $4, revision = $5, updated_at_unix_ms = $6
             WHERE instance_id = $7 AND revision = $8",
        )
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(next_revision)
        .bind(update.updated_at_unix_ms)
        .bind(instance_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    };
    if rows_affected == 0 {
        tx.rollback().await.map_err(DataLayerError::sql)?;
        return Ok(TransactionalCasOutcome::Conflict);
    }
    if let Some(event) = event {
        sqlx::query(
            "INSERT INTO relay_event_outbox
             (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (instance_id, event_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&event.event_id)
        .bind(ROUTE_PROFILE_OUTBOX_EVENT_TYPE)
        .bind(&event.payload_json)
        .bind(event.created_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?;
    }
    tx.commit().await.map_err(DataLayerError::sql)?;
    Ok(TransactionalCasOutcome::Applied)
}

async fn compare_and_set_mysql_with_route_profile_outbox(
    pool: &MySqlPool,
    instance_id: &str,
    expected_revision: i64,
    next_revision: i64,
    update: &IntegrationConfigUpdate,
    event: Option<&RouteProfileOutboxEvent>,
) -> Result<TransactionalCasOutcome, DataLayerError> {
    let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
    let rows_affected = if expected_revision == 0 {
        let result = sqlx::query(
            "INSERT INTO new_api_integration_configs
             (instance_id, route_profile, execution_mode, enabled, capability_version,
              revision, updated_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, 1, ?)",
        )
        .bind(instance_id)
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(update.updated_at_unix_ms)
        .execute(&mut *tx)
        .await;
        match mysql_initial_insert_outcome(result) {
            Ok(MysqlInitialInsertOutcome::Applied) => 1,
            Ok(MysqlInitialInsertOutcome::Conflict) => {
                tx.rollback().await.map_err(DataLayerError::sql)?;
                return Ok(TransactionalCasOutcome::Conflict);
            }
            Err(error) => return Err(DataLayerError::sql(error)),
        }
    } else {
        sqlx::query(
            "UPDATE new_api_integration_configs
             SET route_profile = ?, execution_mode = ?, enabled = ?,
                 capability_version = ?, revision = ?, updated_at_unix_ms = ?
             WHERE instance_id = ? AND revision = ?",
        )
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(next_revision)
        .bind(update.updated_at_unix_ms)
        .bind(instance_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    };
    if rows_affected == 0 {
        tx.rollback().await.map_err(DataLayerError::sql)?;
        return Ok(TransactionalCasOutcome::Conflict);
    }
    if let Some(event) = event {
        sqlx::query(
            "INSERT INTO relay_event_outbox
             (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
             VALUES (?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE event_id = VALUES(event_id)",
        )
        .bind(instance_id)
        .bind(&event.event_id)
        .bind(ROUTE_PROFILE_OUTBOX_EVENT_TYPE)
        .bind(&event.payload_json)
        .bind(event.created_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?;
    }
    tx.commit().await.map_err(DataLayerError::sql)?;
    Ok(TransactionalCasOutcome::Applied)
}

async fn compare_and_set_sqlite_with_route_profile_outbox(
    pool: &SqlitePool,
    instance_id: &str,
    expected_revision: i64,
    next_revision: i64,
    update: &IntegrationConfigUpdate,
    event: Option<&RouteProfileOutboxEvent>,
) -> Result<TransactionalCasOutcome, DataLayerError> {
    let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
    let rows_affected = if expected_revision == 0 {
        sqlx::query(
            "INSERT INTO new_api_integration_configs
             (instance_id, route_profile, execution_mode, enabled, capability_version,
              revision, updated_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, 1, ?)
             ON CONFLICT(instance_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(update.updated_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    } else {
        sqlx::query(
            "UPDATE new_api_integration_configs
             SET route_profile = ?, execution_mode = ?, enabled = ?,
                 capability_version = ?, revision = ?, updated_at_unix_ms = ?
             WHERE instance_id = ? AND revision = ?",
        )
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(next_revision)
        .bind(update.updated_at_unix_ms)
        .bind(instance_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    };
    if rows_affected == 0 {
        tx.rollback().await.map_err(DataLayerError::sql)?;
        return Ok(TransactionalCasOutcome::Conflict);
    }
    if let Some(event) = event {
        sqlx::query(
            "INSERT INTO relay_event_outbox
             (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(instance_id, event_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&event.event_id)
        .bind(ROUTE_PROFILE_OUTBOX_EVENT_TYPE)
        .bind(&event.payload_json)
        .bind(event.created_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?;
    }
    tx.commit().await.map_err(DataLayerError::sql)?;
    Ok(TransactionalCasOutcome::Applied)
}

async fn compare_and_set_postgres_with_credential_rotation(
    pool: &sqlx::PgPool,
    instance_id: &str,
    expected_revision: i64,
    next_revision: i64,
    update: &IntegrationConfigUpdate,
    rotation: &IntegrationCredentialRotation,
    payload_sha256: &str,
    event: Option<&RouteProfileOutboxEvent>,
    bootstrap: Option<&BootstrapIntegrationCredentialRotation>,
) -> Result<TransactionalCasOutcome, DataLayerError> {
    let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
    let rows_affected = if expected_revision == 0 {
        sqlx::query(
            "INSERT INTO new_api_integration_configs
             (instance_id, route_profile, execution_mode, enabled, capability_version,
              revision, updated_at_unix_ms)
             VALUES ($1, $2, $3, $4, $5, 1, $6)
             ON CONFLICT (instance_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(update.updated_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    } else {
        sqlx::query(
            "UPDATE new_api_integration_configs
             SET route_profile = $1, execution_mode = $2, enabled = $3,
                 capability_version = $4, revision = $5, updated_at_unix_ms = $6
             WHERE instance_id = $7 AND revision = $8",
        )
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(next_revision)
        .bind(update.updated_at_unix_ms)
        .bind(instance_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    };

    if rows_affected == 0 {
        let existing = postgres_existing_credentials(&mut tx, instance_id).await?;
        let current_config_revision: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM new_api_integration_configs WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?;
        let outcome = rotation_retry_outcome(
            existing.as_ref(),
            current_config_revision,
            expected_revision,
            next_revision,
            rotation,
            payload_sha256,
        )?;
        match outcome {
            TransactionalCasOutcome::Applied => {
                tx.commit().await.map_err(DataLayerError::sql)?;
                return Ok(TransactionalCasOutcome::Applied);
            }
            TransactionalCasOutcome::Conflict => {
                tx.rollback().await.map_err(DataLayerError::sql)?;
                return Ok(TransactionalCasOutcome::Conflict);
            }
        }
    }

    let existing = postgres_existing_credentials(&mut tx, instance_id).await?;
    let write_values = match credential_rotation_write_values(
        existing.as_ref(),
        rotation,
        bootstrap,
        update.updated_at_unix_ms,
    ) {
        Ok(values) => values,
        Err(error) => {
            tx.rollback().await.map_err(DataLayerError::sql)?;
            return Err(error);
        }
    };

    let credential_write = sqlx::query(
        "INSERT INTO new_api_integration_credentials
         (instance_id,
          current_control_secret_ciphertext,
          previous_control_secret_ciphertext,
          current_relay_secret_ciphertext,
          previous_relay_secret_ciphertext,
          transition_expires_at_unix_ms,
          rotation_id,
          last_rotation_payload_sha256,
          credential_revision,
          updated_at_unix_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (instance_id) DO UPDATE
         SET current_control_secret_ciphertext = EXCLUDED.current_control_secret_ciphertext,
             previous_control_secret_ciphertext = EXCLUDED.previous_control_secret_ciphertext,
             current_relay_secret_ciphertext = EXCLUDED.current_relay_secret_ciphertext,
             previous_relay_secret_ciphertext = EXCLUDED.previous_relay_secret_ciphertext,
             transition_expires_at_unix_ms = EXCLUDED.transition_expires_at_unix_ms,
             rotation_id = EXCLUDED.rotation_id,
             last_rotation_payload_sha256 = EXCLUDED.last_rotation_payload_sha256,
             credential_revision = EXCLUDED.credential_revision,
             updated_at_unix_ms = EXCLUDED.updated_at_unix_ms",
    )
    .bind(instance_id)
    .bind(rotation.control_secret_ciphertext.as_ciphertext())
    .bind(write_values.previous_control_secret_ciphertext)
    .bind(rotation.relay_secret_ciphertext.as_ciphertext())
    .bind(write_values.previous_relay_secret_ciphertext)
    .bind(write_values.transition_expires_at_unix_ms)
    .bind(&rotation.rotation_id)
    .bind(payload_sha256)
    .bind(next_revision)
    .bind(update.updated_at_unix_ms)
    .execute(&mut *tx)
    .await;
    if let Err(error) = credential_write {
        tx.rollback().await.map_err(DataLayerError::sql)?;
        return Err(DataLayerError::sql(error));
    }
    if let Some(event) = event {
        let outbox_write = sqlx::query(
            "INSERT INTO relay_event_outbox
             (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (instance_id, event_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&event.event_id)
        .bind(ROUTE_PROFILE_OUTBOX_EVENT_TYPE)
        .bind(&event.payload_json)
        .bind(event.created_at_unix_ms)
        .execute(&mut *tx)
        .await;
        if let Err(error) = outbox_write {
            tx.rollback().await.map_err(DataLayerError::sql)?;
            return Err(DataLayerError::sql(error));
        }
    }
    tx.commit().await.map_err(DataLayerError::sql)?;
    Ok(TransactionalCasOutcome::Applied)
}

async fn postgres_existing_credentials(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance_id: &str,
) -> Result<Option<ExistingCredentialState>, DataLayerError> {
    let row: Option<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision
         FROM new_api_integration_credentials
         WHERE instance_id = $1",
    )
    .bind(instance_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(DataLayerError::sql)?;
    Ok(row.map(
        |(
            current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision,
        )| ExistingCredentialState {
            current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision,
        },
    ))
}

async fn compare_and_set_mysql_with_credential_rotation(
    pool: &MySqlPool,
    instance_id: &str,
    expected_revision: i64,
    next_revision: i64,
    update: &IntegrationConfigUpdate,
    rotation: &IntegrationCredentialRotation,
    payload_sha256: &str,
    event: Option<&RouteProfileOutboxEvent>,
    bootstrap: Option<&BootstrapIntegrationCredentialRotation>,
) -> Result<TransactionalCasOutcome, DataLayerError> {
    let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
    let rows_affected = if expected_revision == 0 {
        let result = sqlx::query(
            "INSERT INTO new_api_integration_configs
             (instance_id, route_profile, execution_mode, enabled, capability_version,
              revision, updated_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, 1, ?)",
        )
        .bind(instance_id)
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(update.updated_at_unix_ms)
        .execute(&mut *tx)
        .await;
        match mysql_initial_insert_outcome(result) {
            Ok(MysqlInitialInsertOutcome::Applied) => 1,
            Ok(MysqlInitialInsertOutcome::Conflict) => 0,
            Err(error) => return Err(DataLayerError::sql(error)),
        }
    } else {
        sqlx::query(
            "UPDATE new_api_integration_configs
             SET route_profile = ?, execution_mode = ?, enabled = ?,
                 capability_version = ?, revision = ?, updated_at_unix_ms = ?
             WHERE instance_id = ? AND revision = ?",
        )
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(next_revision)
        .bind(update.updated_at_unix_ms)
        .bind(instance_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    };

    if rows_affected == 0 {
        let existing = mysql_existing_credentials(&mut tx, instance_id).await?;
        let current_config_revision: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM new_api_integration_configs WHERE instance_id = ?",
        )
        .bind(instance_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?;
        let outcome = rotation_retry_outcome(
            existing.as_ref(),
            current_config_revision,
            expected_revision,
            next_revision,
            rotation,
            payload_sha256,
        )?;
        match outcome {
            TransactionalCasOutcome::Applied => {
                tx.commit().await.map_err(DataLayerError::sql)?;
                return Ok(TransactionalCasOutcome::Applied);
            }
            TransactionalCasOutcome::Conflict => {
                tx.rollback().await.map_err(DataLayerError::sql)?;
                return Ok(TransactionalCasOutcome::Conflict);
            }
        }
    }

    let existing = mysql_existing_credentials(&mut tx, instance_id).await?;
    let write_values = match credential_rotation_write_values(
        existing.as_ref(),
        rotation,
        bootstrap,
        update.updated_at_unix_ms,
    ) {
        Ok(values) => values,
        Err(error) => {
            tx.rollback().await.map_err(DataLayerError::sql)?;
            return Err(error);
        }
    };

    let credential_write = sqlx::query(
        "INSERT INTO new_api_integration_credentials
         (instance_id,
          current_control_secret_ciphertext,
          previous_control_secret_ciphertext,
          current_relay_secret_ciphertext,
          previous_relay_secret_ciphertext,
          transition_expires_at_unix_ms,
          rotation_id,
          last_rotation_payload_sha256,
          credential_revision,
          updated_at_unix_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE
             current_control_secret_ciphertext = VALUES(current_control_secret_ciphertext),
             previous_control_secret_ciphertext = VALUES(previous_control_secret_ciphertext),
             current_relay_secret_ciphertext = VALUES(current_relay_secret_ciphertext),
             previous_relay_secret_ciphertext = VALUES(previous_relay_secret_ciphertext),
             transition_expires_at_unix_ms = VALUES(transition_expires_at_unix_ms),
             rotation_id = VALUES(rotation_id),
             last_rotation_payload_sha256 = VALUES(last_rotation_payload_sha256),
             credential_revision = VALUES(credential_revision),
             updated_at_unix_ms = VALUES(updated_at_unix_ms)",
    )
    .bind(instance_id)
    .bind(rotation.control_secret_ciphertext.as_ciphertext())
    .bind(write_values.previous_control_secret_ciphertext)
    .bind(rotation.relay_secret_ciphertext.as_ciphertext())
    .bind(write_values.previous_relay_secret_ciphertext)
    .bind(write_values.transition_expires_at_unix_ms)
    .bind(&rotation.rotation_id)
    .bind(payload_sha256)
    .bind(next_revision)
    .bind(update.updated_at_unix_ms)
    .execute(&mut *tx)
    .await;
    if let Err(error) = credential_write {
        tx.rollback().await.map_err(DataLayerError::sql)?;
        return Err(DataLayerError::sql(error));
    }
    if let Some(event) = event {
        let outbox_write = sqlx::query(
            "INSERT INTO relay_event_outbox
             (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
             VALUES (?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE event_id = VALUES(event_id)",
        )
        .bind(instance_id)
        .bind(&event.event_id)
        .bind(ROUTE_PROFILE_OUTBOX_EVENT_TYPE)
        .bind(&event.payload_json)
        .bind(event.created_at_unix_ms)
        .execute(&mut *tx)
        .await;
        if let Err(error) = outbox_write {
            tx.rollback().await.map_err(DataLayerError::sql)?;
            return Err(DataLayerError::sql(error));
        }
    }
    tx.commit().await.map_err(DataLayerError::sql)?;
    Ok(TransactionalCasOutcome::Applied)
}

async fn mysql_existing_credentials(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    instance_id: &str,
) -> Result<Option<ExistingCredentialState>, DataLayerError> {
    let row: Option<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision
         FROM new_api_integration_credentials
         WHERE instance_id = ?",
    )
    .bind(instance_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(DataLayerError::sql)?;
    Ok(row.map(
        |(
            current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision,
        )| ExistingCredentialState {
            current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision,
        },
    ))
}

async fn compare_and_set_sqlite_with_credential_rotation(
    pool: &SqlitePool,
    instance_id: &str,
    expected_revision: i64,
    next_revision: i64,
    update: &IntegrationConfigUpdate,
    rotation: &IntegrationCredentialRotation,
    payload_sha256: &str,
    event: Option<&RouteProfileOutboxEvent>,
    bootstrap: Option<&BootstrapIntegrationCredentialRotation>,
) -> Result<TransactionalCasOutcome, DataLayerError> {
    let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
    let rows_affected = if expected_revision == 0 {
        sqlx::query(
            "INSERT INTO new_api_integration_configs
             (instance_id, route_profile, execution_mode, enabled, capability_version,
              revision, updated_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, 1, ?)
             ON CONFLICT(instance_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(update.updated_at_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    } else {
        sqlx::query(
            "UPDATE new_api_integration_configs
             SET route_profile = ?, execution_mode = ?, enabled = ?,
                 capability_version = ?, revision = ?, updated_at_unix_ms = ?
             WHERE instance_id = ? AND revision = ?",
        )
        .bind(&update.route_profile)
        .bind(&update.execution_mode)
        .bind(update.enabled)
        .bind(&update.capability_version)
        .bind(next_revision)
        .bind(update.updated_at_unix_ms)
        .bind(instance_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected()
    };

    if rows_affected == 0 {
        let existing = sqlite_existing_credentials(&mut tx, instance_id).await?;
        let current_config_revision: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM new_api_integration_configs WHERE instance_id = ?",
        )
        .bind(instance_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DataLayerError::sql)?;
        let outcome = rotation_retry_outcome(
            existing.as_ref(),
            current_config_revision,
            expected_revision,
            next_revision,
            rotation,
            payload_sha256,
        )?;
        match outcome {
            TransactionalCasOutcome::Applied => {
                tx.commit().await.map_err(DataLayerError::sql)?;
                return Ok(TransactionalCasOutcome::Applied);
            }
            TransactionalCasOutcome::Conflict => {
                tx.rollback().await.map_err(DataLayerError::sql)?;
                return Ok(TransactionalCasOutcome::Conflict);
            }
        }
    }

    let existing = sqlite_existing_credentials(&mut tx, instance_id).await?;
    let write_values = match credential_rotation_write_values(
        existing.as_ref(),
        rotation,
        bootstrap,
        update.updated_at_unix_ms,
    ) {
        Ok(values) => values,
        Err(error) => {
            tx.rollback().await.map_err(DataLayerError::sql)?;
            return Err(error);
        }
    };

    let credential_write = sqlx::query(
        "INSERT INTO new_api_integration_credentials
         (instance_id,
          current_control_secret_ciphertext,
          previous_control_secret_ciphertext,
          current_relay_secret_ciphertext,
          previous_relay_secret_ciphertext,
          transition_expires_at_unix_ms,
          rotation_id,
          last_rotation_payload_sha256,
          credential_revision,
          updated_at_unix_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(instance_id) DO UPDATE
         SET current_control_secret_ciphertext = excluded.current_control_secret_ciphertext,
             previous_control_secret_ciphertext = excluded.previous_control_secret_ciphertext,
             current_relay_secret_ciphertext = excluded.current_relay_secret_ciphertext,
             previous_relay_secret_ciphertext = excluded.previous_relay_secret_ciphertext,
             transition_expires_at_unix_ms = excluded.transition_expires_at_unix_ms,
             rotation_id = excluded.rotation_id,
             last_rotation_payload_sha256 = excluded.last_rotation_payload_sha256,
             credential_revision = excluded.credential_revision,
             updated_at_unix_ms = excluded.updated_at_unix_ms",
    )
    .bind(instance_id)
    .bind(rotation.control_secret_ciphertext.as_ciphertext())
    .bind(write_values.previous_control_secret_ciphertext)
    .bind(rotation.relay_secret_ciphertext.as_ciphertext())
    .bind(write_values.previous_relay_secret_ciphertext)
    .bind(write_values.transition_expires_at_unix_ms)
    .bind(&rotation.rotation_id)
    .bind(payload_sha256)
    .bind(next_revision)
    .bind(update.updated_at_unix_ms)
    .execute(&mut *tx)
    .await;
    if let Err(error) = credential_write {
        tx.rollback().await.map_err(DataLayerError::sql)?;
        return Err(DataLayerError::sql(error));
    }
    if let Some(event) = event {
        let outbox_write = sqlx::query(
            "INSERT INTO relay_event_outbox
             (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(instance_id, event_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&event.event_id)
        .bind(ROUTE_PROFILE_OUTBOX_EVENT_TYPE)
        .bind(&event.payload_json)
        .bind(event.created_at_unix_ms)
        .execute(&mut *tx)
        .await;
        if let Err(error) = outbox_write {
            tx.rollback().await.map_err(DataLayerError::sql)?;
            return Err(DataLayerError::sql(error));
        }
    }
    tx.commit().await.map_err(DataLayerError::sql)?;
    Ok(TransactionalCasOutcome::Applied)
}

async fn sqlite_existing_credentials(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    instance_id: &str,
) -> Result<Option<ExistingCredentialState>, DataLayerError> {
    let row: Option<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision
         FROM new_api_integration_credentials
         WHERE instance_id = ?",
    )
    .bind(instance_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(DataLayerError::sql)?;
    Ok(row.map(
        |(
            current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision,
        )| ExistingCredentialState {
            current_control_secret_ciphertext,
            current_relay_secret_ciphertext,
            rotation_id,
            last_rotation_payload_sha256,
            credential_revision,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error as StdError,
        fmt::{self, Display, Formatter},
    };

    use sqlx::{
        error::{DatabaseError, ErrorKind},
        sqlite::SqlitePoolOptions,
    };

    use super::{
        mysql_initial_insert_outcome, BootstrapIntegrationCredentialRotation,
        IntegrationConfigCasResult, IntegrationConfigStore, IntegrationConfigUpdate,
        IntegrationCredentialRotation, MysqlInitialInsertOutcome, OpaqueCredentialCiphertext,
        PersistedIntegrationConfig,
    };

    #[derive(Debug)]
    struct TestDatabaseError {
        unique_violation: bool,
    }

    impl Display for TestDatabaseError {
        fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
            formatter.write_str("synthetic database error")
        }
    }

    impl StdError for TestDatabaseError {}

    impl DatabaseError for TestDatabaseError {
        fn message(&self) -> &str {
            "synthetic database error"
        }

        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            if self.unique_violation {
                ErrorKind::UniqueViolation
            } else {
                ErrorKind::Other
            }
        }
    }

    #[test]
    fn mysql_initial_insert_only_treats_duplicate_keys_as_cas_conflicts() {
        let duplicate_key = sqlx::Error::Database(Box::new(TestDatabaseError {
            unique_violation: true,
        }));
        let other_database_error = sqlx::Error::Database(Box::new(TestDatabaseError {
            unique_violation: false,
        }));

        assert_eq!(
            mysql_initial_insert_outcome::<()>(Err(duplicate_key))
                .expect("duplicate key should be a CAS conflict"),
            MysqlInitialInsertOutcome::Conflict,
        );
        assert!(mysql_initial_insert_outcome::<()>(Err(other_database_error)).is_err());
    }

    async fn test_store() -> IntegrationConfigStore {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        sqlx::query(
            "CREATE TABLE new_api_integration_configs (
                instance_id TEXT PRIMARY KEY,
                route_profile TEXT NOT NULL,
                execution_mode TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                capability_version TEXT NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("integration config table should be created");
        IntegrationConfigStore::sqlite(pool)
    }

    async fn test_store_with_outbox() -> (IntegrationConfigStore, sqlx::SqlitePool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        sqlx::query(
            "CREATE TABLE new_api_integration_configs (
                instance_id TEXT PRIMARY KEY,
                route_profile TEXT NOT NULL,
                execution_mode TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                capability_version TEXT NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("integration config table should be created");
        sqlx::query(
            "CREATE TABLE relay_event_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                UNIQUE (instance_id, event_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("relay event outbox table should be created");
        (IntegrationConfigStore::sqlite(pool.clone()), pool)
    }

    async fn test_store_with_credentials() -> (IntegrationConfigStore, sqlx::SqlitePool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        sqlx::query(
            "CREATE TABLE new_api_integration_configs (
                instance_id TEXT PRIMARY KEY,
                route_profile TEXT NOT NULL,
                execution_mode TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                capability_version TEXT NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("integration config table should be created");
        sqlx::query(
            "CREATE TABLE new_api_integration_credentials (
                instance_id TEXT PRIMARY KEY,
                current_control_secret_ciphertext TEXT NOT NULL,
                previous_control_secret_ciphertext TEXT,
                current_relay_secret_ciphertext TEXT NOT NULL,
                previous_relay_secret_ciphertext TEXT,
                transition_expires_at_unix_ms INTEGER,
                rotation_id TEXT NOT NULL,
                last_rotation_payload_sha256 TEXT NOT NULL,
                credential_revision INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("integration credential table should be created");
        (IntegrationConfigStore::sqlite(pool.clone()), pool)
    }

    async fn test_store_with_credentials_and_outbox() -> (IntegrationConfigStore, sqlx::SqlitePool)
    {
        let (store, pool) = test_store_with_credentials().await;
        sqlx::query(
            "CREATE TABLE relay_event_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                UNIQUE (instance_id, event_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("relay event outbox table should be created");
        (store, pool)
    }

    fn update(route_profile: &str) -> IntegrationConfigUpdate {
        IntegrationConfigUpdate {
            route_profile: route_profile.to_string(),
            execution_mode: "direct_channel".to_string(),
            enabled: true,
            capability_version: "0.1.0".to_string(),
            updated_at_unix_ms: 1_784_073_600_000,
        }
    }

    fn rotation(
        control_ciphertext: &str,
        relay_ciphertext: &str,
        rotation_id: &str,
        transition_expires_at_unix_ms: Option<i64>,
        payload_marker: u8,
    ) -> IntegrationCredentialRotation {
        IntegrationCredentialRotation::new(
            OpaqueCredentialCiphertext::new(control_ciphertext)
                .expect("control ciphertext should be accepted"),
            OpaqueCredentialCiphertext::new(relay_ciphertext)
                .expect("relay ciphertext should be accepted"),
            transition_expires_at_unix_ms,
            false,
            rotation_id,
            format!("{payload_marker:064x}"),
        )
        .expect("credential rotation should be accepted")
    }

    fn bootstrap_rotation(
        bootstrap_control_ciphertext: &str,
        bootstrap_relay_ciphertext: &str,
        control_ciphertext: &str,
        relay_ciphertext: &str,
        rotation_id: &str,
        transition_expires_at_unix_ms: i64,
        payload_marker: u8,
    ) -> BootstrapIntegrationCredentialRotation {
        BootstrapIntegrationCredentialRotation::new(
            OpaqueCredentialCiphertext::new(bootstrap_control_ciphertext)
                .expect("bootstrap control ciphertext should be accepted"),
            OpaqueCredentialCiphertext::new(bootstrap_relay_ciphertext)
                .expect("bootstrap relay ciphertext should be accepted"),
            rotation(
                control_ciphertext,
                relay_ciphertext,
                rotation_id,
                Some(transition_expires_at_unix_ms),
                payload_marker,
            ),
        )
        .expect("bootstrap credential rotation should be accepted")
    }

    #[tokio::test]
    async fn bootstrap_rotation_retains_verified_bootstrap_credentials_and_never_replaces_a_persisted_row(
    ) {
        let (store, pool) = test_store_with_credentials_and_outbox().await;
        let first = bootstrap_rotation(
            "gateway-ciphertext:bootstrap-control:nonce-a",
            "gateway-ciphertext:bootstrap-relay:nonce-a",
            "gateway-ciphertext:control:nonce-a",
            "gateway-ciphertext:relay:nonce-a",
            "bootstrap-rotation-1",
            1_784_073_600_001,
            7,
        );
        let applied = store
            .compare_and_set_with_route_profile_outbox_and_bootstrap_credential_rotation(
                "aether-primary",
                0,
                &update("balanced"),
                &first,
            )
            .await
            .expect("bootstrap config, credentials, and outbox should persist together");
        assert!(matches!(applied, IntegrationConfigCasResult::Applied(_)));

        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("bootstrap credentials should exist");
        assert_eq!(
            credentials
                .current_control_secret_ciphertext
                .as_ciphertext(),
            "gateway-ciphertext:control:nonce-a"
        );
        assert_eq!(
            credentials
                .previous_control_secret_ciphertext
                .as_ref()
                .expect("bootstrap control credential should be retained")
                .as_ciphertext(),
            "gateway-ciphertext:bootstrap-control:nonce-a"
        );
        assert_eq!(
            credentials.current_relay_secret_ciphertext.as_ciphertext(),
            "gateway-ciphertext:relay:nonce-a"
        );
        assert_eq!(
            credentials
                .previous_relay_secret_ciphertext
                .as_ref()
                .expect("bootstrap relay credential should be retained")
                .as_ciphertext(),
            "gateway-ciphertext:bootstrap-relay:nonce-a"
        );
        assert_eq!(
            credentials.transition_expires_at_unix_ms,
            Some(1_784_073_600_001)
        );

        let retry = bootstrap_rotation(
            "gateway-ciphertext:bootstrap-control:nonce-b",
            "gateway-ciphertext:bootstrap-relay:nonce-b",
            "gateway-ciphertext:control:nonce-b",
            "gateway-ciphertext:relay:nonce-b",
            "bootstrap-rotation-1",
            1_784_073_600_001,
            7,
        );
        let retry_result = store
            .compare_and_set_with_route_profile_outbox_and_bootstrap_credential_rotation(
                "aether-primary",
                0,
                &update("low-cost"),
                &retry,
            )
            .await
            .expect("same bootstrap rotation should be idempotent after re-encryption");
        assert!(matches!(
            retry_result,
            IntegrationConfigCasResult::Applied(_)
        ));

        let config = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        assert_eq!(config.route_profile, "balanced");
        assert_eq!(config.revision, 1);
        let retry_credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(
            retry_credentials
                .current_control_secret_ciphertext
                .as_ciphertext(),
            "gateway-ciphertext:control:nonce-a"
        );

        let second_bootstrap = bootstrap_rotation(
            "gateway-ciphertext:bootstrap-control:replacement",
            "gateway-ciphertext:bootstrap-relay:replacement",
            "gateway-ciphertext:control:replacement",
            "gateway-ciphertext:relay:replacement",
            "bootstrap-rotation-2",
            1_784_073_600_001,
            8,
        );
        let error = store
            .compare_and_set_with_route_profile_outbox_and_bootstrap_credential_rotation(
                "aether-primary",
                1,
                &update("latency-first"),
                &second_bootstrap,
            )
            .await
            .expect_err("bootstrap must never replace a durable credential row");
        assert!(error
            .to_string()
            .contains("bootstrap credential rotation requires credentials to be absent"));

        let config = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        assert_eq!(config.route_profile, "balanced");
        assert_eq!(config.revision, 1);
        let event_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM relay_event_outbox WHERE instance_id = ?")
                .bind("aether-primary")
                .fetch_one(&pool)
                .await
                .expect("outbox rows should load");
        assert_eq!(event_count, 1);
    }

    #[tokio::test]
    async fn bootstrap_rotation_rejects_invalid_transition_without_creating_config_credentials_or_outbox(
    ) {
        let missing_expiry = BootstrapIntegrationCredentialRotation::new(
            OpaqueCredentialCiphertext::new("gateway-ciphertext:bootstrap-control")
                .expect("bootstrap control ciphertext should be accepted"),
            OpaqueCredentialCiphertext::new("gateway-ciphertext:bootstrap-relay")
                .expect("bootstrap relay ciphertext should be accepted"),
            rotation(
                "gateway-ciphertext:control",
                "gateway-ciphertext:relay",
                "bootstrap-missing-expiry",
                None,
                9,
            ),
        );
        assert!(missing_expiry.is_err());

        let revoked = IntegrationCredentialRotation::new(
            OpaqueCredentialCiphertext::new("gateway-ciphertext:control")
                .expect("control ciphertext should be accepted"),
            OpaqueCredentialCiphertext::new("gateway-ciphertext:relay")
                .expect("relay ciphertext should be accepted"),
            Some(1_784_073_600_001),
            true,
            "bootstrap-revoked",
            format!("{:064x}", 10),
        )
        .expect("credential rotation should be accepted");
        let revoked_bootstrap = BootstrapIntegrationCredentialRotation::new(
            OpaqueCredentialCiphertext::new("gateway-ciphertext:bootstrap-control")
                .expect("bootstrap control ciphertext should be accepted"),
            OpaqueCredentialCiphertext::new("gateway-ciphertext:bootstrap-relay")
                .expect("bootstrap relay ciphertext should be accepted"),
            revoked,
        );
        assert!(revoked_bootstrap.is_err());

        let (store, pool) = test_store_with_credentials_and_outbox().await;
        let expired = bootstrap_rotation(
            "gateway-ciphertext:bootstrap-control",
            "gateway-ciphertext:bootstrap-relay",
            "gateway-ciphertext:control",
            "gateway-ciphertext:relay",
            "bootstrap-expired",
            1_784_073_600_000,
            11,
        );
        let error = store
            .compare_and_set_with_route_profile_outbox_and_bootstrap_credential_rotation(
                "aether-primary",
                0,
                &update("balanced"),
                &expired,
            )
            .await
            .expect_err("expired bootstrap transition must fail atomically");
        assert!(error
            .to_string()
            .contains("bootstrap credential transition expiry must be later than the update time"));
        assert!(store
            .get("aether-primary")
            .await
            .expect("config should load")
            .is_none());
        assert!(store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .is_none());
        let event_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM relay_event_outbox WHERE instance_id = ?")
                .bind("aether-primary")
                .fetch_one(&pool)
                .await
                .expect("outbox rows should load");
        assert_eq!(event_count, 0);
    }

    #[tokio::test]
    async fn config_and_credential_rotation_commit_under_one_revision() {
        let (store, _) = test_store_with_credentials().await;
        let initial = rotation(
            "ciphertext:control:v1",
            "ciphertext:relay:v1",
            "rotation-1",
            None,
            1,
        );

        let applied = store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("default"),
                &initial,
            )
            .await
            .expect("initial config and credentials should persist");
        assert!(matches!(applied, IntegrationConfigCasResult::Applied(_)));
        let initial_credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(initial_credentials.credential_revision, 1);
        assert_eq!(
            initial_credentials
                .current_control_secret_ciphertext
                .as_ciphertext(),
            "ciphertext:control:v1"
        );
        assert_eq!(
            initial_credentials
                .current_relay_secret_ciphertext
                .as_ciphertext(),
            "ciphertext:relay:v1"
        );
        assert!(initial_credentials
            .previous_control_secret_ciphertext
            .is_none());
        assert!(initial_credentials
            .previous_relay_secret_ciphertext
            .is_none());
        assert_eq!(initial_credentials.transition_expires_at_unix_ms, None);
        assert_eq!(
            initial_credentials.last_rotation_payload_sha256,
            initial.stable_payload_sha256()
        );

        let rotated = rotation(
            "ciphertext:control:v2",
            "ciphertext:relay:v2",
            "rotation-2",
            Some(1_784_160_000_000),
            2,
        );
        store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                1,
                &update("latency-first"),
                &rotated,
            )
            .await
            .expect("config and credential rotation should persist");
        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(current.revision, 2);
        assert_eq!(credentials.credential_revision, current.revision);
        assert_eq!(
            credentials
                .current_control_secret_ciphertext
                .as_ciphertext(),
            "ciphertext:control:v2"
        );
        assert_eq!(
            credentials
                .previous_control_secret_ciphertext
                .as_ref()
                .expect("previous control ciphertext should be retained")
                .as_ciphertext(),
            "ciphertext:control:v1"
        );
        assert_eq!(
            credentials.current_relay_secret_ciphertext.as_ciphertext(),
            "ciphertext:relay:v2"
        );
        assert_eq!(
            credentials
                .previous_relay_secret_ciphertext
                .as_ref()
                .expect("previous relay ciphertext should be retained")
                .as_ciphertext(),
            "ciphertext:relay:v1"
        );
        assert_eq!(credentials.rotation_id, "rotation-2");
        assert_eq!(
            credentials.transition_expires_at_unix_ms,
            Some(1_784_160_000_000)
        );
    }

    #[tokio::test]
    async fn stale_config_cas_does_not_write_credentials() {
        let (store, pool) = test_store_with_credentials().await;
        let initial = rotation(
            "ciphertext:control:v1",
            "ciphertext:relay:v1",
            "rotation-1",
            None,
            1,
        );
        store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("default"),
                &initial,
            )
            .await
            .expect("initial config and credentials should persist");

        let stale = store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("low-cost"),
                &rotation(
                    "ciphertext:control:stale",
                    "ciphertext:relay:stale",
                    "rotation-stale",
                    Some(1_784_160_000_000),
                    3,
                ),
            )
            .await
            .expect("stale CAS should return a conflict");
        assert!(matches!(
            stale,
            IntegrationConfigCasResult::Conflict(Some(_))
        ));

        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("initial credentials should remain");
        assert_eq!(credentials.credential_revision, 1);
        assert_eq!(
            credentials
                .current_control_secret_ciphertext
                .as_ciphertext(),
            "ciphertext:control:v1"
        );
        let row_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM new_api_integration_credentials")
                .fetch_one(&pool)
                .await
                .expect("credential row count should load");
        assert_eq!(row_count, 1);
    }

    #[tokio::test]
    async fn credential_write_failure_rolls_back_config_cas() {
        let (store, pool) = test_store_with_credentials().await;
        sqlx::query("DROP TABLE new_api_integration_credentials")
            .execute(&pool)
            .await
            .expect("credential table should drop");
        sqlx::query(
            "CREATE TABLE new_api_integration_credentials (
                instance_id TEXT PRIMARY KEY,
                current_control_secret_ciphertext TEXT NOT NULL CHECK (current_control_secret_ciphertext = 'accepted'),
                previous_control_secret_ciphertext TEXT,
                current_relay_secret_ciphertext TEXT NOT NULL,
                previous_relay_secret_ciphertext TEXT,
                transition_expires_at_unix_ms INTEGER,
                rotation_id TEXT NOT NULL,
                last_rotation_payload_sha256 TEXT NOT NULL,
                credential_revision INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("rejecting credential table should be created");
        store
            .compare_and_set("aether-primary", 0, &update("default"))
            .await
            .expect("initial config should persist");

        let error = store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                1,
                &update("latency-first"),
                &rotation(
                    "ciphertext:control:rejected",
                    "ciphertext:relay:v2",
                    "rotation-2",
                    None,
                    2,
                ),
            )
            .await
            .expect_err("credential constraint violation should abort config CAS");
        assert!(error.to_string().contains("CHECK constraint failed"));

        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("initial config should remain");
        assert_eq!(current.route_profile, "default");
        assert_eq!(current.revision, 1);
        let row_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM new_api_integration_credentials")
                .fetch_one(&pool)
                .await
                .expect("credential row count should load");
        assert_eq!(row_count, 0);
    }

    #[tokio::test]
    async fn credential_values_are_redacted_from_debug_and_json() {
        let (store, _) = test_store_with_credentials().await;
        let sentinel = "control-plaintext-must-never-leak";
        let opaque = OpaqueCredentialCiphertext::new(sentinel)
            .expect("opaque ciphertext should be accepted");
        let opaque_debug = format!("{opaque:?}");
        let opaque_json = serde_json::to_string(&opaque)
            .expect("opaque ciphertext should serialize without its contents");
        assert!(!opaque_debug.contains(sentinel));
        assert!(!opaque_json.contains(sentinel));
        let rotation = rotation(
            sentinel,
            "relay-plaintext-must-never-leak",
            "rotation-secret-safe",
            None,
            4,
        );
        let digest = rotation.stable_payload_sha256().to_string();
        let rotation_debug = format!("{rotation:?}");
        let rotation_json = serde_json::to_string(&rotation)
            .expect("credential rotation should serialize without secret contents");
        assert!(!rotation_debug.contains(sentinel));
        assert!(!rotation_json.contains(sentinel));
        assert!(!rotation_debug.contains(&digest));
        assert!(!rotation_json.contains(&digest));

        store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("default"),
                &rotation,
            )
            .await
            .expect("config and credentials should persist");
        let persisted = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        let persisted_debug = format!("{persisted:?}");
        let persisted_json = serde_json::to_string(&persisted)
            .expect("persisted credentials should serialize without secret contents");
        assert!(!persisted_debug.contains(sentinel));
        assert!(!persisted_json.contains(sentinel));
        assert!(!persisted_json.contains("relay-plaintext-must-never-leak"));
        assert!(!persisted_debug.contains(&digest));
        assert!(!persisted_json.contains(&digest));
    }

    #[tokio::test]
    async fn identical_rotation_retry_is_idempotent_but_payload_mismatch_is_rejected() {
        let (store, pool) = test_store_with_credentials().await;
        let initial = rotation(
            "ciphertext:control:v1",
            "ciphertext:relay:v1",
            "rotation-1",
            None,
            1,
        );
        store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("default"),
                &initial,
            )
            .await
            .expect("initial rotation should persist");

        let retry = store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("low-cost"),
                &initial,
            )
            .await
            .expect("identical rotation retry should be idempotent");
        assert!(matches!(retry, IntegrationConfigCasResult::Applied(_)));
        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        assert_eq!(current.route_profile, "default");
        assert_eq!(current.revision, 1);
        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(credentials.rotation_id, "rotation-1");
        assert_eq!(credentials.credential_revision, 1);

        let error = store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("low-cost"),
                &rotation(
                    "ciphertext:control:different",
                    "ciphertext:relay:different",
                    "rotation-1",
                    None,
                    2,
                ),
            )
            .await
            .expect_err("same rotation id with another payload must fail");
        assert!(error
            .to_string()
            .contains("credential rotation id is already bound to a different payload"));
        let row_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM new_api_integration_credentials")
                .fetch_one(&pool)
                .await
                .expect("credential row count should load");
        assert_eq!(row_count, 1);
    }

    #[tokio::test]
    async fn stable_plaintext_payload_digest_makes_reencrypted_retry_idempotent() {
        let (store, _) = test_store_with_credentials().await;
        let first_encryption = rotation(
            "gateway-ciphertext:control:nonce-a",
            "gateway-ciphertext:relay:nonce-a",
            "rotation-1",
            None,
            7,
        );
        store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("default"),
                &first_encryption,
            )
            .await
            .expect("initial ciphertext should persist");

        let reencrypted_retry = rotation(
            "gateway-ciphertext:control:nonce-b",
            "gateway-ciphertext:relay:nonce-b",
            "rotation-1",
            None,
            7,
        );
        let retry = store
            .compare_and_set_with_credential_rotation(
                "aether-primary",
                0,
                &update("low-cost"),
                &reencrypted_retry,
            )
            .await
            .expect("same plaintext digest should make a re-encrypted retry idempotent");
        assert!(matches!(retry, IntegrationConfigCasResult::Applied(_)));

        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(current.route_profile, "default");
        assert_eq!(
            credentials
                .current_control_secret_ciphertext
                .as_ciphertext(),
            "gateway-ciphertext:control:nonce-a"
        );
        assert_eq!(
            credentials.current_relay_secret_ciphertext.as_ciphertext(),
            "gateway-ciphertext:relay:nonce-a"
        );
    }

    #[tokio::test]
    async fn config_credential_rotation_and_route_outbox_share_one_transaction() {
        let (store, pool) = test_store_with_credentials_and_outbox().await;
        let initial = rotation(
            "ciphertext:control:v1",
            "ciphertext:relay:v1",
            "rotation-1",
            None,
            1,
        );
        store
            .compare_and_set_with_route_profile_outbox_and_credential_rotation(
                "aether-primary",
                0,
                &update("balanced"),
                &initial,
            )
            .await
            .expect("initial combined update should persist");

        let events: Vec<(String, String)> = sqlx::query_as(
            "SELECT event_id, payload_json
             FROM relay_event_outbox
             WHERE instance_id = ?
             ORDER BY sequence ASC",
        )
        .bind("aether-primary")
        .fetch_all(&pool)
        .await
        .expect("combined outbox rows should load");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "route-profile:aether-primary:1");
        let first_payload: serde_json::Value =
            serde_json::from_str(&events[0].1).expect("outbox payload should be JSON");
        assert_eq!(first_payload["previous_route_profile"], "default");
        assert_eq!(first_payload["route_profile"], "balanced");
        assert_eq!(first_payload["revision"], 1);

        let mut non_route_update = update("balanced");
        non_route_update.enabled = false;
        store
            .compare_and_set_with_route_profile_outbox_and_credential_rotation(
                "aether-primary",
                1,
                &non_route_update,
                &rotation(
                    "ciphertext:control:v2",
                    "ciphertext:relay:v2",
                    "rotation-2",
                    Some(1_784_160_000_000),
                    2,
                ),
            )
            .await
            .expect("non-route combined update should persist");
        let event_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM relay_event_outbox WHERE instance_id = ?")
                .bind("aether-primary")
                .fetch_one(&pool)
                .await
                .expect("combined outbox row count should load");
        assert_eq!(event_count, 1);
        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(credentials.credential_revision, 2);
        assert!(
            !store
                .get("aether-primary")
                .await
                .expect("config should load")
                .expect("config should exist")
                .enabled
        );
    }

    #[tokio::test]
    async fn combined_outbox_failure_rolls_back_config_and_credentials() {
        let (store, pool) = test_store_with_credentials_and_outbox().await;
        store
            .compare_and_set_with_route_profile_outbox_and_credential_rotation(
                "aether-primary",
                0,
                &update("default"),
                &rotation(
                    "ciphertext:control:v1",
                    "ciphertext:relay:v1",
                    "rotation-1",
                    None,
                    1,
                ),
            )
            .await
            .expect("initial combined update should persist");
        sqlx::query("DROP TABLE relay_event_outbox")
            .execute(&pool)
            .await
            .expect("default outbox table should drop");
        sqlx::query(
            "CREATE TABLE relay_event_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                event_type TEXT NOT NULL CHECK (event_type = 'rejected'),
                payload_json TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                UNIQUE (instance_id, event_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("rejecting outbox table should be created");

        let error = store
            .compare_and_set_with_route_profile_outbox_and_credential_rotation(
                "aether-primary",
                1,
                &update("latency-first"),
                &rotation(
                    "ciphertext:control:v2",
                    "ciphertext:relay:v2",
                    "rotation-2",
                    Some(1_784_160_000_000),
                    2,
                ),
            )
            .await
            .expect_err("outbox failure must abort the combined transaction");
        assert!(error.to_string().contains("CHECK constraint failed"));
        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(current.route_profile, "default");
        assert_eq!(current.revision, 1);
        assert_eq!(credentials.credential_revision, 1);
        assert_eq!(
            credentials
                .current_control_secret_ciphertext
                .as_ciphertext(),
            "ciphertext:control:v1"
        );
    }

    #[tokio::test]
    async fn compare_and_set_allows_only_one_writer_for_each_revision() {
        let store = test_store().await;
        let first_store = store.clone();
        let second_store = store.clone();
        let first_update = update("balanced");
        let second_update = update("low-cost");
        let (first, second) = tokio::join!(
            first_store.compare_and_set("aether-primary", 0, &first_update),
            second_store.compare_and_set("aether-primary", 0, &second_update),
        );
        let outcomes = [
            first.expect("first CAS should complete"),
            second.expect("second CAS should complete"),
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, IntegrationConfigCasResult::Applied(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, IntegrationConfigCasResult::Conflict(Some(_))))
                .count(),
            1
        );

        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        assert_eq!(current.revision, 1);

        let applied = store
            .compare_and_set("aether-primary", 1, &update("latency-first"))
            .await
            .expect("second revision CAS should complete");
        assert_eq!(
            applied,
            IntegrationConfigCasResult::Applied(PersistedIntegrationConfig {
                instance_id: "aether-primary".to_string(),
                route_profile: "latency-first".to_string(),
                execution_mode: "direct_channel".to_string(),
                enabled: true,
                capability_version: "0.1.0".to_string(),
                revision: 2,
                updated_at_unix_ms: 1_784_073_600_000,
            })
        );
    }

    #[tokio::test]
    async fn route_profile_transition_outbox_is_atomic_for_same_revision_writers() {
        let (store, pool) = test_store_with_outbox().await;
        store
            .compare_and_set("aether-primary", 0, &update("default"))
            .await
            .expect("initial config should persist");

        let first_store = store.clone();
        let second_store = store.clone();
        let first_update = update("latency-first");
        let second_update = update("low-cost");
        let (first, second) = tokio::join!(
            first_store.compare_and_set_with_route_profile_outbox(
                "aether-primary",
                1,
                &first_update,
            ),
            second_store.compare_and_set_with_route_profile_outbox(
                "aether-primary",
                1,
                &second_update,
            ),
        );
        let outcomes = [
            first.expect("first CAS should complete"),
            second.expect("second CAS should complete"),
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, IntegrationConfigCasResult::Applied(_)))
                .count(),
            1,
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, IntegrationConfigCasResult::Conflict(Some(_))))
                .count(),
            1,
        );

        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        assert_eq!(current.revision, 2);
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT event_id, event_type, payload_json
             FROM relay_event_outbox
             WHERE instance_id = ?
             ORDER BY sequence ASC",
        )
        .bind("aether-primary")
        .fetch_all(&pool)
        .await
        .expect("outbox rows should load");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "route-profile:aether-primary:2");
        assert_eq!(rows[0].1, "route_decision_changed");
        let payload: serde_json::Value =
            serde_json::from_str(&rows[0].2).expect("outbox payload should be JSON");
        assert_eq!(payload["previous_route_profile"], "default");
        assert_eq!(payload["route_profile"], current.route_profile);
        assert_eq!(payload["revision"], 2);

        let conflict = store
            .compare_and_set_with_route_profile_outbox("aether-primary", 1, &update("cost-first"))
            .await
            .expect("stale CAS should return a conflict");
        assert!(matches!(
            conflict,
            IntegrationConfigCasResult::Conflict(Some(_))
        ));
        let row_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM relay_event_outbox WHERE instance_id = ?")
                .bind("aether-primary")
                .fetch_one(&pool)
                .await
                .expect("outbox row count should load");
        assert_eq!(row_count, 1);
    }

    #[tokio::test]
    async fn route_profile_transition_rolls_back_config_when_outbox_insert_fails() {
        let (store, pool) = test_store_with_outbox().await;
        sqlx::query("DROP TABLE relay_event_outbox")
            .execute(&pool)
            .await
            .expect("default outbox table should drop");
        sqlx::query(
            "CREATE TABLE relay_event_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                event_type TEXT NOT NULL CHECK (event_type = 'rejected'),
                payload_json TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                UNIQUE (instance_id, event_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("rejecting outbox table should be created");
        store
            .compare_and_set("aether-primary", 0, &update("default"))
            .await
            .expect("initial config should persist");

        let error = store
            .compare_and_set_with_route_profile_outbox(
                "aether-primary",
                1,
                &update("latency-first"),
            )
            .await
            .expect_err("outbox constraint violation should abort the config transition");
        assert!(error.to_string().contains("CHECK constraint failed"));

        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("initial config should remain");
        assert_eq!(current.route_profile, "default");
        assert_eq!(current.revision, 1);
        let row_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_event_outbox")
            .fetch_one(&pool)
            .await
            .expect("outbox row count should load");
        assert_eq!(row_count, 0);
    }

    #[tokio::test]
    async fn non_route_profile_transition_does_not_append_an_outbox_event() {
        let (store, pool) = test_store_with_outbox().await;
        store
            .compare_and_set("aether-primary", 0, &update("default"))
            .await
            .expect("initial config should persist");
        let mut non_route_update = update("default");
        non_route_update.enabled = false;

        let applied = store
            .compare_and_set_with_route_profile_outbox("aether-primary", 1, &non_route_update)
            .await
            .expect("non-route config update should persist");
        assert!(matches!(applied, IntegrationConfigCasResult::Applied(_)));

        let current = store
            .get("aether-primary")
            .await
            .expect("config should load")
            .expect("config should exist");
        assert_eq!(current.revision, 2);
        assert!(!current.enabled);
        let row_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_event_outbox")
            .fetch_one(&pool)
            .await
            .expect("outbox row count should load");
        assert_eq!(row_count, 0);
    }

    #[tokio::test]
    async fn initial_non_default_route_profile_publishes_the_virtual_default_transition() {
        let (store, pool) = test_store_with_outbox().await;

        let applied = store
            .compare_and_set_with_route_profile_outbox("aether-primary", 0, &update("balanced"))
            .await
            .expect("initial non-default config should persist");
        assert!(matches!(applied, IntegrationConfigCasResult::Applied(_)));

        let row: (String, String) = sqlx::query_as(
            "SELECT event_id, payload_json
             FROM relay_event_outbox
             WHERE instance_id = ?",
        )
        .bind("aether-primary")
        .fetch_one(&pool)
        .await
        .expect("initial route transition event should persist");
        assert_eq!(row.0, "route-profile:aether-primary:1");
        let payload: serde_json::Value =
            serde_json::from_str(&row.1).expect("outbox payload should be JSON");
        assert_eq!(payload["previous_route_profile"], "default");
        assert_eq!(payload["route_profile"], "balanced");
        assert_eq!(payload["revision"], 1);
    }
}
