use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bark::Wallet;
use bip39::Mnemonic;
use nostr_sdk::prelude::{Keys, PublicKey as NostrPublicKey, ToBech32};
use nwc_mobile::{
    HostError, HostErrorKind, HostFuture, Nip98SigningKey, NwcEncryption, NwcMethod,
    NwcNotificationType, NwcSecretKey, OperationContext, ProtectedSecretStore, RejectionCode,
    StoredNwcSecrets, WakeDiagnosticCollector, WakeDiagnosticSink, WakeDisposition,
};
pub(crate) use nwc_mobile_http::ApnsWakeRegistrationConfig as NwcPushConfig;
use nwc_mobile_http::{InvoiceSettlementCompletion, InvoiceSettlementMonitorConfig};
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
use nwc_mobile_tokio::{
    LightningNodeProvider, LightningNodeRequest, NwcMobile, NwcMobileConfig,
    NwcMobileSettlementStatus, NwcMobileWakeKind, OpenedLightningNode, ReadyLightningNodeProvider,
};
use nwc_mobile_uniffi::{validate_wake_envelope, MobileCancellation, MobileWakeEnvelope};
pub use nwc_mobile_uniffi::{NwcExtensionWakeExecution, NwcSettlementNotificationStatus};
use zeroize::Zeroizing;

use crate::core::{derive_nostr_keys_from_mnemonic, NOSTR_SECRET_KEY, WALLET_SEED_KEY};
use crate::persistence::{PersistedAppData, ServerConfig};
use crate::wallet::{open_bark_wallet, WalletOpenMode};
use crate::{NwcPermission, SecretStore, WalletNetwork};

use super::bark_lightning::{bark_wallet_info, BarkNode};

const APP_DATA_FILE: &str = "rebel-app-data.json";
const INFO_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EXTENSION_EXECUTION_MILLISECONDS: u64 = 30_000;
pub(crate) const SETTLEMENT_MONITOR_RESERVE: Duration = Duration::from_secs(5);

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
    nwc_mobile_nostr::publish_nwc_info_event_with_notifications(
        &relay,
        &secret,
        client_pubkey.as_ref(),
        methods,
        vec![
            NwcNotificationType::PaymentReceived,
            NwcNotificationType::PaymentSent,
        ],
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

pub(crate) type RebelSecretProvider = StoredNwcSecrets<dyn SecretStore>;

pub(crate) fn rebel_secret_provider(secrets: Arc<dyn SecretStore>) -> RebelSecretProvider {
    StoredNwcSecrets::new(secrets, NOSTR_SECRET_KEY)
}

impl ProtectedSecretStore for dyn SecretStore {
    fn load_secret(&self, key: &str) -> Result<Option<String>, HostError> {
        Ok(SecretStore::get_secret(self, key.to_owned()))
    }

    fn store_secret(&self, key: &str, value: &str) -> Result<(), HostError> {
        SecretStore::set_secret(self, key.to_owned(), value.to_owned())
            .then_some(())
            .ok_or_else(unavailable)
    }

    fn delete_secret(&self, key: &str) -> Result<(), HostError> {
        SecretStore::delete_secret(self, key.to_owned())
            .then_some(())
            .ok_or_else(unavailable)
    }
}

/// Opens Rebel's existing Bark wallet when a native worker starts cold.
struct NwcBarkNodeProvider {
    data_dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
}

impl NwcBarkNodeProvider {
    fn new(data_dir: PathBuf, secrets: Arc<dyn SecretStore>) -> Self {
        Self { data_dir, secrets }
    }
}

impl LightningNodeProvider for NwcBarkNodeProvider {
    fn open_node<'a>(
        &'a self,
        request: LightningNodeRequest,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<OpenedLightningNode, HostError>> {
        Box::pin(async move {
            if context.cancellation().is_cancelled() {
                return Err(HostError::new(HostErrorKind::Cancelled));
            }
            let server_config = extension_server_config(&self.data_dir)
                .ok_or_else(|| HostError::new(HostErrorKind::Unavailable))?;
            let mnemonic = self
                .secrets
                .get_secret(WALLET_SEED_KEY.to_string())
                .map(Zeroizing::new)
                .and_then(|seed| Mnemonic::from_str(seed.as_str()).ok())
                .ok_or_else(unavailable)?;
            ensure_nostr_secret(self.secrets.as_ref(), &mnemonic)?;
            if context.cancellation().is_cancelled() {
                return Err(HostError::new(HostErrorKind::Cancelled));
            }
            let wallet = open_bark_wallet(
                self.data_dir.clone(),
                &mnemonic,
                WalletOpenMode::OpenExisting,
                server_config,
            )
            .await
            .map_err(|_| unavailable())?
            .wallet;
            if context.cancellation().is_cancelled() {
                return Err(HostError::new(HostErrorKind::Cancelled));
            }
            Ok(opened_bark_node(wallet, request))
        })
    }
}

pub(crate) fn opened_bark_provider(wallet: Wallet) -> impl LightningNodeProvider {
    ReadyLightningNodeProvider::new(move |request| Ok(opened_bark_node(wallet.clone(), request)))
}

fn opened_bark_node(wallet: Wallet, request: LightningNodeRequest) -> OpenedLightningNode {
    let wallet_info = bark_wallet_info(request.wallet_service_pubkey().clone());
    let node = match request.diagnostics() {
        Some(diagnostics) => BarkNode::with_diagnostics(wallet, diagnostics),
        None => BarkNode::new(wallet),
    };
    OpenedLightningNode::new(node, wallet_info)
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

fn ensure_nostr_secret(secrets: &dyn SecretStore, mnemonic: &Mnemonic) -> Result<(), HostError> {
    if secrets.get_secret(NOSTR_SECRET_KEY.to_string()).is_some() {
        return Ok(());
    }
    let mnemonic = Zeroizing::new(mnemonic.to_string());
    let keys = derive_nostr_keys_from_mnemonic(mnemonic.as_str())
        .map_err(|_| HostError::new(HostErrorKind::Internal))?;
    let encoded = Zeroizing::new(
        keys.secret_key()
            .to_bech32()
            .expect("secret-key bech32 encoding is infallible"),
    );
    secrets
        .set_secret(NOSTR_SECRET_KEY.to_string(), encoded.to_string())
        .then_some(())
        .ok_or_else(unavailable)
}

const fn unavailable() -> HostError {
    HostError::new(HostErrorKind::Unavailable)
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
    settlement_monitor_config: Option<InvoiceSettlementMonitorConfig>,
}

#[uniffi::export]
impl NwcExtensionEngine {
    #[uniffi::constructor]
    pub fn new(
        data_dir: String,
        secret_store: Box<dyn SecretStore>,
        wake_server_url: Option<String>,
        install_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            data_dir: PathBuf::from(data_dir),
            secrets: Arc::from(secret_store),
            settlement_monitor_config: InvoiceSettlementMonitorConfig::new(
                wake_server_url,
                install_id,
            )
            .ok(),
        })
    }

    pub async fn execute_wake(
        &self,
        request: MobileWakeEnvelope,
        execution_milliseconds: u64,
        cancellation: Arc<MobileCancellation>,
    ) -> NwcExtensionWakeExecution {
        if execution_milliseconds == 0
            || execution_milliseconds > MAX_EXTENSION_EXECUTION_MILLISECONDS
        {
            return wake_execution(
                rejected_disposition(),
                &WakeDiagnosticCollector::default(),
                NwcSettlementNotificationStatus::NotTracked,
            );
        }
        let wake_kind = NwcMobileWakeKind::from_settlement_check(request.settlement_check);
        let input = match validate_wake_envelope(request) {
            Ok(input) => input.core_input(),
            Err(_) => {
                return wake_execution(
                    rejected_disposition(),
                    &WakeDiagnosticCollector::default(),
                    NwcSettlementNotificationStatus::NotTracked,
                )
            }
        };
        let data_dir = self.data_dir.clone();
        let secrets = self.secrets.clone();
        let diagnostics = Arc::new(WakeDiagnosticCollector::default());
        let execution_diagnostics: Arc<dyn WakeDiagnosticSink> = diagnostics.clone();
        let secret_provider = rebel_secret_provider(secrets);
        let mut config = NwcMobileConfig::new(
            &data_dir,
            NwcBarkNodeProvider::new(data_dir.clone(), self.secrets.clone()),
            NostrRelayTransport,
            secret_provider.clone(),
        )
        .with_diagnostics(execution_diagnostics);
        if let Some(monitor_config) = self.settlement_monitor_config.clone() {
            config = config.with_completion_handler(
                InvoiceSettlementCompletion::new(monitor_config, secret_provider),
                SETTLEMENT_MONITOR_RESERVE,
            );
        }
        let result = NwcMobile::execute_native_wake(
            config,
            input,
            wake_kind,
            Duration::from_millis(execution_milliseconds),
            cancellation,
        )
        .await;
        wake_execution(
            result.disposition(),
            &diagnostics,
            settlement_notification_status(result.settlement_status()),
        )
    }
}

fn wake_execution(
    disposition: WakeDisposition,
    diagnostics: &WakeDiagnosticCollector,
    settlement_notification_status: NwcSettlementNotificationStatus,
) -> NwcExtensionWakeExecution {
    NwcExtensionWakeExecution {
        disposition: disposition.into(),
        diagnostic_codes: diagnostics
            .codes()
            .into_iter()
            .map(|code| code.as_str().to_owned())
            .collect(),
        settlement_notification_status,
    }
}

fn settlement_notification_status(
    value: NwcMobileSettlementStatus,
) -> NwcSettlementNotificationStatus {
    match value {
        NwcMobileSettlementStatus::Pending => NwcSettlementNotificationStatus::Pending,
        NwcMobileSettlementStatus::Delivered => NwcSettlementNotificationStatus::Delivered,
        _ => NwcSettlementNotificationStatus::NotTracked,
    }
}

fn rejected_disposition() -> WakeDisposition {
    WakeDisposition::rejected(RejectionCode::InvalidWakePayload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_execution_exposes_only_stable_diagnostic_codes() {
        let diagnostics = WakeDiagnosticCollector::default();
        diagnostics.record(nwc_mobile::WakeDiagnosticCode::PaymentFeeLimitExceeded);

        let execution = wake_execution(
            WakeDisposition::queued(nwc_mobile::QueueReason::WalletUnavailable),
            &diagnostics,
            NwcSettlementNotificationStatus::Pending,
        );

        assert_eq!(
            execution.diagnostic_codes,
            vec!["payment_fee_limit_exceeded"]
        );
        assert_eq!(
            execution.settlement_notification_status,
            NwcSettlementNotificationStatus::Pending
        );
    }
}
