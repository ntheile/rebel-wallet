use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use nostr_sdk::prelude::{Keys, PublicKey as NostrPublicKey, SecretKey};
use nwc_mobile::{
    BudgetInterval, ConnectionId, FeePolicy, HostConnectionAuthorization, HostError, HostErrorKind,
    LegacyHostConnection, NwaRequestPresentation, NwcEncryption, NwcMethod, NwcMobileService,
    NwcSecretKey, SecretProvider, UnixTimestamp,
};
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
use zeroize::Zeroizing;

use crate::core::NOSTR_SECRET_KEY;
use crate::{NwaRequestState, NwcBudgetInterval, NwcConnection, NwcPermission, SecretStore};

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
            .filter_map(permission_method)
            .collect(),
        connection.budget_sat,
        budget_interval(connection.budget_interval),
        FeePolicy::CountTowardBudget {
            maximum_fee_sat: maximum_nwc_fee_sat(connection.budget_sat),
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
pub(crate) fn nwa_request_state(request: &NwaRequestPresentation) -> NwaRequestState {
    NwaRequestState {
        id: request.id_hex().to_owned(),
        client_pubkey: request.client_pubkey_hex().to_owned(),
        display_name: request.display_name().to_owned(),
        icon_url: request.icon_url().map(str::to_owned),
        icon_display_url: None,
        requesting_app_description: request.requesting_app_description().map(str::to_owned),
        callback_target_description: request.callback_target_description().to_owned(),
        relay: request.relay_urls().join("\n"),
        budget_sat: request.budget_limit_sat(),
        budget_interval: rebel_budget_interval(request.budget_interval()),
        permissions: request
            .methods()
            .iter()
            .copied()
            .filter_map(permission)
            .collect(),
        expires_at: request.expires_at().map(UnixTimestamp::as_secs),
    }
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
    let methods = NwcPermission::IMPLEMENTED
        .into_iter()
        .filter(|permission| client_pubkey.is_none() || permissions.contains(permission))
        .filter_map(permission_method)
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

/// Maps implemented Rebel permissions into shared NIP-47 methods.
pub(crate) const fn permission_method(permission: NwcPermission) -> Option<NwcMethod> {
    match permission {
        NwcPermission::GetInfo => Some(NwcMethod::GetInfo),
        NwcPermission::GetBalance => Some(NwcMethod::GetBalance),
        NwcPermission::MakeInvoice => Some(NwcMethod::MakeInvoice),
        NwcPermission::PayInvoice => Some(NwcMethod::PayInvoice),
        NwcPermission::LookupInvoice => Some(NwcMethod::LookupInvoice),
        NwcPermission::ListTransactions => Some(NwcMethod::ListTransactions),
        NwcPermission::PayKeysend
        | NwcPermission::MakeHoldInvoice
        | NwcPermission::CancelHoldInvoice
        | NwcPermission::SettleHoldInvoice => None,
    }
}

const fn permission(method: NwcMethod) -> Option<NwcPermission> {
    match method {
        NwcMethod::GetInfo => Some(NwcPermission::GetInfo),
        NwcMethod::GetBalance => Some(NwcPermission::GetBalance),
        NwcMethod::MakeInvoice => Some(NwcPermission::MakeInvoice),
        NwcMethod::PayInvoice => Some(NwcPermission::PayInvoice),
        NwcMethod::LookupInvoice => Some(NwcPermission::LookupInvoice),
        NwcMethod::ListTransactions => Some(NwcPermission::ListTransactions),
        _ => None,
    }
}

const fn budget_interval(interval: NwcBudgetInterval) -> BudgetInterval {
    match interval {
        NwcBudgetInterval::Never => BudgetInterval::Never,
        NwcBudgetInterval::Hourly => BudgetInterval::Hourly,
        NwcBudgetInterval::Daily => BudgetInterval::Daily,
        NwcBudgetInterval::Weekly => BudgetInterval::Weekly,
        NwcBudgetInterval::Monthly => BudgetInterval::Monthly,
        NwcBudgetInterval::Yearly => BudgetInterval::Yearly,
    }
}

const fn rebel_budget_interval(interval: BudgetInterval) -> NwcBudgetInterval {
    match interval {
        BudgetInterval::Never => NwcBudgetInterval::Never,
        BudgetInterval::Hourly => NwcBudgetInterval::Hourly,
        BudgetInterval::Daily => NwcBudgetInterval::Daily,
        BudgetInterval::Weekly => NwcBudgetInterval::Weekly,
        BudgetInterval::Monthly => NwcBudgetInterval::Monthly,
        BudgetInterval::Yearly => NwcBudgetInterval::Yearly,
        _ => NwcBudgetInterval::Never,
    }
}

fn maximum_nwc_fee_sat(budget_sat: u64) -> u64 {
    if budget_sat == 0 {
        return 0;
    }
    (budget_sat / 20).clamp(10, 1_000).min(budget_sat)
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
