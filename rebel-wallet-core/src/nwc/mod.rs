mod nwc_bark_node;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use nostr_sdk::prelude::{Keys, PublicKey as NostrPublicKey, SecretKey};
pub(crate) use nwc_bark_node::{
    bark_wallet_info, BarkNode, NwcBarkNodeProvider, OpenedNwcBarkNodeProvider,
};
use nwc_mobile::{
    ClientSecretStore, ClientSecretStoreError, ConnectionId, HostError, HostErrorKind, HostFuture,
    Nip98SigningKey, NwcEncryption, NwcMethod, NwcNotificationType, NwcSecretKey, QueueReason,
    RejectionCode, SecretProvider, WakeDiagnosticCollector, WakeDiagnosticSink, WakeDisposition,
};
pub(crate) use nwc_mobile_http::ApnsWakeRegistrationConfig as NwcPushConfig;
use nwc_mobile_http::InvoiceSettlementMonitorConfig;
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
use nwc_mobile_tokio::{
    run_on_native_runtime, NwcMobile, NwcMobileCompletionContext, NwcMobileCompletionHandler,
    NwcMobileConfig, NwcMobileSettlementStatus, NwcMobileWakeKind,
};
use nwc_mobile_uniffi::{
    validate_wake_envelope, MobileCancellation, MobileWakeDisposition, MobileWakeEnvelope,
};
use zeroize::Zeroizing;

use crate::core::NOSTR_SECRET_KEY;
use crate::{NwcPermission, SecretStore};

const INFO_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EXTENSION_EXECUTION_MILLISECONDS: u64 = 30_000;
const SETTLEMENT_MONITOR_RESERVE: Duration = Duration::from_secs(5);

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
        let monitor_config = self.settlement_monitor_config.clone();
        let result = run_on_native_runtime(async move {
            let provider = NwcBarkNodeProvider::new(data_dir.clone(), secrets.clone());
            let secret_provider = RebelSecretProvider::new(secrets.clone());
            let mut config =
                NwcMobileConfig::new(&data_dir, provider, NostrRelayTransport, secret_provider)
                    .with_diagnostics(execution_diagnostics);
            if let Some(configured_monitor) = monitor_config {
                config = config.with_completion_handler(
                    RebelSettlementMonitorCompletion::new(configured_monitor, secrets),
                    SETTLEMENT_MONITOR_RESERVE,
                );
            }
            let mobile = match NwcMobile::open(config) {
                Ok(mobile) => mobile,
                Err(_) => return (queued_disposition(), NwcMobileSettlementStatus::NotTracked),
            };
            let kind = if settlement_check {
                NwcMobileWakeKind::InvoiceSettlement
            } else {
                NwcMobileWakeKind::Request
            };
            let result = mobile
                .execute_wake(
                    input,
                    kind,
                    Duration::from_millis(execution_milliseconds),
                    cancellation.as_ref(),
                )
                .await;
            (result.disposition(), result.settlement_status())
        })
        .await;
        let (disposition, settlement_status) = result
            .unwrap_or_else(|_| (queued_disposition(), NwcMobileSettlementStatus::NotTracked));
        wake_execution(disposition, &diagnostics, settlement_status.into())
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

impl From<NwcMobileSettlementStatus> for NwcSettlementNotificationStatus {
    fn from(value: NwcMobileSettlementStatus) -> Self {
        match value {
            NwcMobileSettlementStatus::Pending => Self::Pending,
            NwcMobileSettlementStatus::Delivered => Self::Delivered,
            _ => Self::NotTracked,
        }
    }
}

pub(crate) struct RebelSettlementMonitorCompletion {
    config: InvoiceSettlementMonitorConfig,
    secrets: Arc<dyn SecretStore>,
}

impl RebelSettlementMonitorCompletion {
    pub(crate) fn new(
        config: InvoiceSettlementMonitorConfig,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self { config, secrets }
    }
}

impl NwcMobileCompletionHandler for RebelSettlementMonitorCompletion {
    fn complete<'a>(
        &'a self,
        context: NwcMobileCompletionContext<'a>,
    ) -> HostFuture<'a, Result<(), HostError>> {
        Box::pin(async move {
            if context.operation().cancellation().is_cancelled() {
                return Err(HostError::new(HostErrorKind::Cancelled));
            }
            let signing_key = extension_nip98_signing_key(self.secrets.as_ref())
                .ok_or_else(|| HostError::new(HostErrorKind::Unavailable))?;
            nwc_mobile_http::update_invoice_settlement_monitor(
                context.ledger(),
                self.config.clone(),
                context.request_event_id(),
                signing_key,
            )
            .await
            .map(|_| ())
            .map_err(|_| HostError::new(HostErrorKind::Unavailable))
        })
    }
}

fn extension_nip98_signing_key(secrets: &dyn SecretStore) -> Option<Nip98SigningKey> {
    let encoded = secrets.get_secret(NOSTR_SECRET_KEY.to_string())?;
    let secret = SecretKey::parse(&encoded).ok()?;
    Nip98SigningKey::from_bytes(secret.to_secret_bytes()).ok()
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
