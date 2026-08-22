use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use nostr_sdk::prelude::{Keys, PublicKey as NostrPublicKey, SecretKey};
use nwc_mobile::{
    maximum_mobile_fee_sat, ClientSecretStore, ClientSecretStoreError, ConnectionId, FeePolicy,
    HostConnectionAuthorization, HostError, HostErrorKind, LegacyHostConnection,
    NwaRequestPresentation, NwcEncryption, NwcMethod, NwcMobileService, NwcSecretKey,
    SecretProvider, UnixTimestamp,
};
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
use zeroize::Zeroizing;

use crate::core::NOSTR_SECRET_KEY;
use crate::{NwaRequestState, NwcConnection, NwcPermission, SecretStore};

const NWC_LEDGER_FILE: &str = "nwc-mobile.sqlite3";
const INFO_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

// Existing Rebel clients advertise NIP-04. This must change atomically with
// connection migration and info-event advertisement.
pub(crate) const NWC_ENCRYPTION: NwcEncryption = NwcEncryption::LegacyNip04;

/// Opens the high-level service over the ledger shared by the app and its NSE.
pub(crate) fn open_nwc_service(
    data_dir: &Path,
) -> Result<NwcMobileService, nwc_mobile::MobileServiceError> {
    NwcMobileService::open(nwc_ledger_path(data_dir))
}

pub(crate) fn nwc_ledger_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(NWC_LEDGER_FILE)
}

/// Maps Rebel's persisted display model into the shared authorization boundary.
pub(crate) fn connection_authorization(connection: &NwcConnection) -> HostConnectionAuthorization {
    HostConnectionAuthorization::new(
        connection.id.clone(),
        connection.client_pubkey.clone(),
        connection.service_pubkey.clone(),
        connection
            .relay
            .split(|character: char| character.is_whitespace() || character == ',')
            .map(str::trim)
            .filter(|relay| !relay.is_empty())
            .map(str::to_owned)
            .collect(),
        connection
            .enabled_permissions()
            .into_iter()
            .map(NwcMethod::from)
            .collect(),
        connection.budget_sat,
        connection.budget_interval.into(),
        FeePolicy::CountTowardBudget {
            maximum_fee_sat: maximum_mobile_fee_sat(connection.budget_sat),
        },
        NWC_ENCRYPTION,
        connection.expires_at.map(UnixTimestamp::from_secs),
    )
}

/// Preserves trusted accounting state only during one-time legacy migration.
pub(crate) fn legacy_connection(connection: &NwcConnection) -> LegacyHostConnection {
    LegacyHostConnection::new(
        connection_authorization(connection),
        UnixTimestamp::from_secs(connection.created_at),
        connection.spent_sat,
    )
}

/// Maps a validated, non-sensitive presentation into Rebel's view state.
pub(crate) fn nwa_request_state(
    request: NwaRequestPresentation,
) -> anyhow::Result<NwaRequestState> {
    request
        .try_into()
        .context("shared NWA presentation is not representable by the native contract")
}

/// Publishes a bounded public capability event through the shared transport.
pub(crate) async fn publish_nwc_info_event(
    relay: String,
    keys: Keys,
    client_pubkey: Option<NostrPublicKey>,
    permissions: Vec<NwcPermission>,
) -> anyhow::Result<()> {
    let client_pubkey = client_pubkey
        .map(|key| nwc_mobile::PublicKey::from_hex(&key.to_hex()))
        .transpose()
        .context("invalid NWC client public key")?;
    let secret = NwcSecretKey::from_bytes(keys.secret_key().to_secret_bytes())
        .context("invalid NWC wallet service key")?;
    let methods = implemented_permissions()
        .into_iter()
        .filter(|permission| client_pubkey.is_none() || permissions.contains(permission))
        .map(NwcMethod::from)
        .collect::<Vec<_>>();
    nwc_mobile_nostr::publish_nwc_info_event(
        &relay,
        &secret,
        client_pubkey.as_ref(),
        methods,
        NWC_ENCRYPTION,
        INFO_PUBLISH_TIMEOUT,
    )
    .await
    .context("failed to publish NWC info event")
}

pub(crate) const fn implemented_permissions() -> [NwcPermission; 6] {
    [
        NwcPermission::GetInfo,
        NwcPermission::GetBalance,
        NwcPermission::PayInvoice,
        NwcPermission::MakeInvoice,
        NwcPermission::LookupInvoice,
        NwcPermission::ListTransactions,
    ]
}

/// Loads the wallet-service secret from the platform secret store on demand.
pub(crate) struct RebelSecretProvider {
    secrets: Arc<dyn SecretStore>,
}

impl RebelSecretProvider {
    pub(crate) fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self { secrets }
    }
}

impl SecretProvider for RebelSecretProvider {
    fn load_nwc_secret(&self, _connection_id: &ConnectionId) -> Result<NwcSecretKey, HostError> {
        let encoded = self
            .secrets
            .get_secret(NOSTR_SECRET_KEY.to_string())
            .map(Zeroizing::new)
            .ok_or_else(|| HostError::new(HostErrorKind::Unavailable))?;
        let secret =
            SecretKey::parse(&encoded).map_err(|_| HostError::new(HostErrorKind::Internal))?;
        NwcSecretKey::from_bytes(secret.to_secret_bytes())
            .map_err(|_| HostError::new(HostErrorKind::Internal))
    }
}

impl ClientSecretStore for RebelSecretProvider {
    fn load_client_secret(
        &self,
        storage_key: &str,
    ) -> Result<Option<String>, ClientSecretStoreError> {
        Ok(self.secrets.get_secret(storage_key.to_owned()))
    }

    fn store_client_secret(
        &self,
        storage_key: &str,
        secret: &str,
    ) -> Result<(), ClientSecretStoreError> {
        self.secrets
            .set_secret(storage_key.to_owned(), secret.to_owned())
            .then_some(())
            .ok_or(ClientSecretStoreError)
    }

    fn delete_client_secret(&self, storage_key: &str) -> Result<(), ClientSecretStoreError> {
        self.secrets
            .delete_secret(storage_key.to_owned())
            .then_some(())
            .ok_or(ClientSecretStoreError)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const SECRET_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct TestSecrets(Mutex<Option<String>>);

    impl SecretStore for TestSecrets {
        fn get_secret(&self, _key: String) -> Option<String> {
            self.0.lock().expect("secret lock").clone()
        }

        fn set_secret(&self, _key: String, _value: String) -> bool {
            false
        }

        fn delete_secret(&self, _key: String) -> bool {
            false
        }
    }

    #[test]
    fn secret_provider_loads_without_caching_or_exposing_material() {
        let store = Arc::new(TestSecrets(Mutex::new(Some(SECRET_HEX.to_string()))));
        let provider = RebelSecretProvider::new(store);
        let connection = ConnectionId::parse("connection:test").expect("connection");
        let secret = provider.load_nwc_secret(&connection).expect("secret");
        assert_eq!(format!("{secret:?}"), "NwcSecretKey([redacted])");
    }

    #[test]
    fn secret_provider_fails_closed_for_missing_or_invalid_material() {
        let connection = ConnectionId::parse("connection:test").expect("connection");
        for value in [None, Some("not-a-secret".to_string())] {
            let provider = RebelSecretProvider::new(Arc::new(TestSecrets(Mutex::new(value))));
            assert!(provider.load_nwc_secret(&connection).is_err());
        }
    }

    #[test]
    fn ledger_path_stays_inside_supplied_app_group_directory() {
        let directory = tempfile::tempdir().expect("directory");
        let service = open_nwc_service(directory.path()).expect("service");
        drop(service);
        assert!(directory.path().join(NWC_LEDGER_FILE).is_file());
    }
}
