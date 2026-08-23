use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bip39::Mnemonic;
use nostr_sdk::prelude::{Keys, PublicKey as NostrPublicKey, SecretKey, ToBech32};
use nwc_mobile::{
    ClientSecretStore, ClientSecretStoreError, ConnectionId, HostError, HostErrorKind,
    Nip98SigningKey, NotificationHint, NwcEncryption, NwcMethod, NwcNotificationType, NwcSecretKey,
    QueueReason, RejectionCode, SecretProvider, WakeDiagnosticCollector, WakeDiagnosticSink,
    WakeDisposition,
};
use nwc_mobile_bark::{execute_bark_wake_with_diagnostics, run_bark_invoice_notification_worker};
pub(crate) use nwc_mobile_http::ApnsWakeRegistrationConfig as NwcPushConfig;
use nwc_mobile_http::InvoiceSettlementMonitorConfig;
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
use nwc_mobile_tokio::{run_bounded_background_wake, run_on_native_runtime, BackgroundWakeWindow};
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
const SETTLEMENT_MONITOR_RESERVE: Duration = Duration::from_secs(5);

const SETTLEMENT_NOT_TRACKED: u8 = 0;
const SETTLEMENT_PENDING: u8 = 1;
const SETTLEMENT_DELIVERED: u8 = 2;

/// Safe settlement-notification state for native notification presentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, uniffi::Enum)]
pub enum NwcSettlementNotificationStatus {
    #[default]
    NotTracked,
    Pending,
    Delivered,
}

/// Safe, non-secret result metadata from one NSE wake execution.
#[derive(Clone, Debug, uniffi::Record)]
pub struct NwcExtensionWakeExecution {
    pub disposition: MobileWakeDisposition,
    pub diagnostic_codes: Vec<String>,
    pub settlement_notification_status: NwcSettlementNotificationStatus,
}

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
        settlement_check: bool,
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
        let data_dir = self.data_dir.clone();
        let secrets = self.secrets.clone();
        let runtime_cancellation = cancellation.clone();
        let execution_cancellation = cancellation.clone();
        let diagnostics = Arc::new(WakeDiagnosticCollector::default());
        let execution_diagnostics: Arc<dyn WakeDiagnosticSink> = diagnostics.clone();
        let settlement_status = Arc::new(AtomicU8::new(SETTLEMENT_NOT_TRACKED));
        let execution_settlement_status = settlement_status.clone();
        let monitor_config = self.settlement_monitor_config.clone();
        let result = run_on_native_runtime(async move {
            run_bounded_background_wake(
                Duration::from_millis(execution_milliseconds),
                runtime_cancellation.as_ref(),
                |window| {
                    Self::execute_wake_inner(
                        data_dir,
                        secrets,
                        request,
                        settlement_check,
                        window,
                        execution_cancellation,
                        execution_diagnostics,
                        monitor_config,
                        execution_settlement_status,
                    )
                },
            )
            .await
        })
        .await;
        wake_execution(
            result.unwrap_or_else(|_| queued_disposition()),
            &diagnostics,
            settlement_status_from_byte(settlement_status.load(Ordering::Acquire)),
        )
    }
}

impl NwcExtensionEngine {
    #[allow(clippy::too_many_arguments)]
    async fn execute_wake_inner(
        data_dir: PathBuf,
        secrets: Arc<dyn SecretStore>,
        request: MobileWakeEnvelope,
        settlement_check: bool,
        window: BackgroundWakeWindow,
        cancellation: Arc<MobileCancellation>,
        diagnostics: Arc<dyn WakeDiagnosticSink>,
        monitor_config: Option<InvoiceSettlementMonitorConfig>,
        settlement_status: Arc<AtomicU8>,
    ) -> WakeDisposition {
        let input = match validate_wake_envelope(request) {
            Ok(input) => input.core_input(),
            Err(_) => return rejected_disposition(),
        };
        let Some(server_config) = extension_server_config(&data_dir) else {
            return queued_disposition();
        };
        let Some(mnemonic) = secrets
            .get_secret(WALLET_SEED_KEY.to_string())
            .map(Zeroizing::new)
            .and_then(|seed| Mnemonic::from_str(seed.as_str()).ok())
        else {
            return queued_disposition();
        };
        if !ensure_nostr_secret(secrets.as_ref(), &mnemonic) || cancellation.is_cancelled() {
            return queued_disposition();
        }
        let manager = match nwc_mobile::NwcApplicationManager::open(&data_dir) {
            Ok(manager) => manager,
            Err(_) => return queued_disposition(),
        };
        let event_id = input.event_id().clone();
        let initial_monitor = manager
            .service()
            .ledger()
            .nwc_invoice_monitor(&event_id)
            .ok()
            .flatten();
        if settlement_check
            && !initial_monitor.as_ref().is_some_and(|monitor| {
                monitor.wallet_service_pubkey() == input.wallet_service_pubkey()
                    && monitor
                        .relays()
                        .iter()
                        .any(|relay| relay.as_str() == input.relay())
            })
        {
            return rejected_disposition();
        }
        let wallet = match open_bark_wallet(
            data_dir.clone(),
            &mnemonic,
            WalletOpenMode::OpenExisting,
            server_config,
        )
        .await
        {
            Ok(opened) => opened.wallet,
            Err(_) => return queued_disposition(),
        };
        let execution_time = if monitor_config.is_some() {
            window
                .remaining()
                .saturating_sub(SETTLEMENT_MONITOR_RESERVE)
        } else {
            window.remaining()
        };
        let Ok(budget) = nwc_mobile::OperationBudget::new(execution_time) else {
            return queued_disposition();
        };
        if cancellation.is_cancelled() {
            return queued_disposition();
        }
        let mut disposition = if settlement_check {
            let _ = run_bark_invoice_notification_worker(
                manager.service().ledger(),
                wallet,
                input.wallet_service_pubkey().clone(),
                &event_id,
                &NostrRelayTransport,
                &RebelSecretProvider::new(secrets.clone()),
                budget,
                cancellation.as_ref(),
            )
            .await;
            WakeDisposition::Completed {
                notification: NotificationHint::Processing,
            }
        } else {
            execute_bark_wake_with_diagnostics(
                manager.service().ledger(),
                wallet,
                &NostrRelayTransport,
                &RebelSecretProvider::new(secrets.clone()),
                input,
                budget,
                cancellation.as_ref(),
                diagnostics,
            )
            .await
        };
        let monitor = manager
            .service()
            .ledger()
            .nwc_invoice_monitor(&event_id)
            .ok()
            .flatten();
        if let Some(monitor) = monitor.as_ref() {
            let completed = monitor.completed();
            settlement_status.store(
                if completed {
                    SETTLEMENT_DELIVERED
                } else {
                    SETTLEMENT_PENDING
                },
                Ordering::Release,
            );
            if settlement_check && completed {
                disposition = WakeDisposition::Completed {
                    notification: NotificationHint::Completed,
                };
            }
        }
        if let (Some(config), Some(_)) = (monitor_config, monitor) {
            if let Some(signing_key) = extension_nip98_signing_key(secrets.as_ref()) {
                let remaining = window.remaining();
                if !remaining.is_zero() {
                    let _ = tokio::time::timeout(
                        remaining,
                        nwc_mobile_http::update_invoice_settlement_monitor(
                            manager.service().ledger(),
                            config,
                            &event_id,
                            signing_key,
                        ),
                    )
                    .await;
                }
            }
        }
        disposition
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

fn settlement_status_from_byte(value: u8) -> NwcSettlementNotificationStatus {
    match value {
        SETTLEMENT_PENDING => NwcSettlementNotificationStatus::Pending,
        SETTLEMENT_DELIVERED => NwcSettlementNotificationStatus::Delivered,
        _ => NwcSettlementNotificationStatus::NotTracked,
    }
}

fn extension_nip98_signing_key(secrets: &dyn SecretStore) -> Option<Nip98SigningKey> {
    let encoded = secrets.get_secret(NOSTR_SECRET_KEY.to_string())?;
    let secret = SecretKey::parse(&encoded).ok()?;
    Nip98SigningKey::from_bytes(secret.to_secret_bytes()).ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_execution_exposes_only_stable_diagnostic_codes() {
        let diagnostics = WakeDiagnosticCollector::default();
        diagnostics.record(nwc_mobile::WakeDiagnosticCode::PaymentFeeLimitExceeded);

        let execution = wake_execution(
            queued_disposition(),
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
