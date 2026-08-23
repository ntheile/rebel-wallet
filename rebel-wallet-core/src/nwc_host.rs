use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bip39::Mnemonic;
use nostr_sdk::prelude::{Keys, PublicKey as NostrPublicKey, SecretKey, ToBech32};
use nwc_mobile::{
    ClientSecretStore, ClientSecretStoreError, ConnectionId, HostError, HostErrorKind,
    Nip98SigningKey, NwcEncryption, NwcMethod, NwcSecretKey, QueueReason, RejectionCode,
    SecretProvider, WakeDisposition,
};
use nwc_mobile_bark::execute_bark_wake;
pub(crate) use nwc_mobile_http::ApnsWakeRegistrationConfig as NwcPushConfig;
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
use nwc_mobile_tokio::{run_bounded_background_wake, BackgroundWakeWindow};
use nwc_mobile_uniffi::{
    validate_wake_envelope, MobileCancellation, MobileWakeDisposition, MobileWakeEnvelope,
};
use zeroize::Zeroizing;

use crate::core::{derive_nostr_keys_from_mnemonic, NOSTR_SECRET_KEY, WALLET_SEED_KEY};
use crate::persistence::{PersistedAppData, ServerConfig};
use crate::wallet::{open_bark_wallet, WalletOpenMode};
use crate::{NwcPermission, SecretStore, WalletNetwork};

const APP_DATA_FILE: &str = "rebel-app-data.json";
const INFO_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EXTENSION_EXECUTION_MILLISECONDS: u64 = 30_000;

// Existing Rebel clients advertise NIP-04. This must change atomically with
// connection migration and info-event advertisement.
pub(crate) const NWC_ENCRYPTION: NwcEncryption = NwcEncryption::LegacyNip04;

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
        .collect();
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

const fn implemented_permissions() -> [NwcPermission; 6] {
    [
        NwcPermission::GetInfo,
        NwcPermission::GetBalance,
        NwcPermission::PayInvoice,
        NwcPermission::MakeInvoice,
        NwcPermission::LookupInvoice,
        NwcPermission::ListTransactions,
    ]
}

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

pub(crate) async fn run_registration_worker(
    ledger: &nwc_mobile::WakeLedger,
    config: nwc_mobile_http::ReadyApnsWakeRegistrationConfig,
    keys: Keys,
) -> anyhow::Result<nwc_mobile_http::RegistrationPass> {
    let signing_key = Nip98SigningKey::from_bytes(keys.secret_key().to_secret_bytes())
        .context("invalid wake registration signing key")?;
    nwc_mobile_http::run_registration_worker(ledger, config, signing_key)
        .await
        .context("wake registration outbox pass failed")
}

/// Rebel-specific wallet bootstrap around the shared native wake contract.
#[derive(uniffi::Object)]
pub struct NwcExtensionEngine {
    data_dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
}

#[uniffi::export]
impl NwcExtensionEngine {
    #[uniffi::constructor]
    pub fn new(data_dir: String, secret_store: Box<dyn SecretStore>) -> Arc<Self> {
        Arc::new(Self {
            data_dir: PathBuf::from(data_dir),
            secrets: Arc::from(secret_store),
        })
    }

    pub async fn execute_wake(
        &self,
        request: MobileWakeEnvelope,
        execution_milliseconds: u64,
        cancellation: Arc<MobileCancellation>,
    ) -> MobileWakeDisposition {
        if execution_milliseconds == 0
            || execution_milliseconds > MAX_EXTENSION_EXECUTION_MILLISECONDS
        {
            return rejected_disposition().into();
        }
        let execution_cancellation = cancellation.clone();
        run_bounded_background_wake(
            Duration::from_millis(execution_milliseconds),
            cancellation.as_ref(),
            |window| self.execute_wake_inner(request, window, execution_cancellation),
        )
        .await
        .into()
    }
}

impl NwcExtensionEngine {
    async fn execute_wake_inner(
        &self,
        request: MobileWakeEnvelope,
        window: BackgroundWakeWindow,
        cancellation: Arc<MobileCancellation>,
    ) -> WakeDisposition {
        let input = match validate_wake_envelope(request) {
            Ok(input) => input.core_input(),
            Err(_) => return rejected_disposition(),
        };
        let Some(server_config) = extension_server_config(&self.data_dir) else {
            return queued_disposition();
        };
        let Some(mnemonic) = self
            .secrets
            .get_secret(WALLET_SEED_KEY.to_string())
            .map(Zeroizing::new)
            .and_then(|seed| Mnemonic::from_str(seed.as_str()).ok())
        else {
            return queued_disposition();
        };
        if !ensure_nostr_secret(self.secrets.as_ref(), &mnemonic) || cancellation.is_cancelled() {
            return queued_disposition();
        }
        let wallet = match open_bark_wallet(
            self.data_dir.clone(),
            &mnemonic,
            WalletOpenMode::OpenExisting,
            server_config,
        )
        .await
        {
            Ok(opened) => opened.wallet,
            Err(_) => return queued_disposition(),
        };
        let Some(budget) = window.operation_budget() else {
            return queued_disposition();
        };
        if cancellation.is_cancelled() {
            return queued_disposition();
        }
        let manager = match nwc_mobile::NwcApplicationManager::open(&self.data_dir) {
            Ok(manager) => manager,
            Err(_) => return queued_disposition(),
        };
        execute_bark_wake(
            manager.service().ledger(),
            wallet,
            &NostrRelayTransport,
            &RebelSecretProvider::new(self.secrets.clone()),
            input,
            budget,
            cancellation.as_ref(),
        )
        .await
    }
}

fn extension_server_config(data_dir: &std::path::Path) -> Option<ServerConfig> {
    let raw = std::fs::read_to_string(data_dir.join(APP_DATA_FILE)).ok()?;
    let data: PersistedAppData = serde_json::from_str(&raw).ok()?;
    Some(
        if data.network == WalletNetwork::Regtest && data.servers.network == WalletNetwork::Regtest
        {
            data.servers
        } else {
            ServerConfig::for_network(data.network)
        },
    )
}

fn ensure_nostr_secret(secrets: &dyn SecretStore, mnemonic: &Mnemonic) -> bool {
    if secrets.get_secret(NOSTR_SECRET_KEY.to_string()).is_some() {
        return true;
    }
    let mnemonic = Zeroizing::new(mnemonic.to_string());
    let Ok(keys) = derive_nostr_keys_from_mnemonic(mnemonic.as_str()) else {
        return false;
    };
    let encoded = Zeroizing::new(
        keys.secret_key()
            .to_bech32()
            .expect("secret-key bech32 encoding is infallible"),
    );
    secrets.set_secret(NOSTR_SECRET_KEY.to_string(), encoded.to_string())
}

fn queued_disposition() -> WakeDisposition {
    WakeDisposition::queued(QueueReason::WalletUnavailable)
}

fn rejected_disposition() -> WakeDisposition {
    WakeDisposition::rejected(RejectionCode::InvalidWakePayload)
}
