use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bip39::Mnemonic;
use nostr_sdk::prelude::ToBech32;
use nwc_mobile::{
    CancellationSignal, EventId, NotificationHint, OperationBudget, PublicKey, QueueReason,
    RejectionCode, UnixTimestamp, WakeDisposition, WakeEngine, WakeInput, WakePolicy,
};
use zeroize::Zeroizing;

use crate::core::{derive_nostr_keys_from_mnemonic, NOSTR_SECRET_KEY, WALLET_SEED_KEY};
use crate::nwc_mobile_adapter::{
    open_nwc_ledger, NostrRelayTransport, RebelSecretProvider, RebelWalletBackend,
};
use crate::persistence::{PersistedAppData, ServerConfig};
use crate::wallet::{open_bark_wallet, WalletOpenMode};
use crate::{SecretStore, WalletNetwork};

const APP_DATA_FILE: &str = "rebel-app-data.json";
const MAX_EXTENSION_EXECUTION_MILLISECONDS: u64 = 30_000;

#[derive(Clone, uniffi::Record)]
pub struct NwcExtensionWakeRequest {
    pub relay_url: String,
    pub event_id_hex: String,
    pub wallet_service_public_key_hex: String,
    pub embedded_event_json: Option<String>,
    pub received_at_seconds: u64,
}

impl fmt::Debug for NwcExtensionWakeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NwcExtensionWakeRequest")
            .field("relay_url", &"[redacted]")
            .field("event_id_hex", &"[redacted]")
            .field("wallet_service_public_key_hex", &"[redacted]")
            .field("has_embedded_event", &self.embedded_event_json.is_some())
            .field("received_at_seconds", &self.received_at_seconds)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum NwcExtensionDisposition {
    Completed,
    AlreadyProcessed,
    QueuedForApplication,
    RetryAfter,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum NwcExtensionNotification {
    Processing,
    Completed,
    OpenApplication,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, uniffi::Record)]
pub struct NwcExtensionWakeResult {
    pub disposition: NwcExtensionDisposition,
    pub notification: NwcExtensionNotification,
}

#[derive(Debug, uniffi::Object)]
pub struct NwcExtensionCancellation {
    cancelled: AtomicBool,
}

#[uniffi::export]
impl NwcExtensionCancellation {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl CancellationSignal for NwcExtensionCancellation {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

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
        request: NwcExtensionWakeRequest,
        execution_milliseconds: u64,
        cancellation: Arc<NwcExtensionCancellation>,
    ) -> NwcExtensionWakeResult {
        if execution_milliseconds == 0
            || execution_milliseconds > MAX_EXTENSION_EXECUTION_MILLISECONDS
        {
            return rejected_result();
        }
        if cancellation.is_cancelled() {
            return queued_result();
        }

        let total_budget = Duration::from_millis(execution_milliseconds);
        match tokio::time::timeout(
            total_budget,
            self.execute_wake_inner(request, total_budget, cancellation),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => queued_result(),
        }
    }
}

impl NwcExtensionEngine {
    async fn execute_wake_inner(
        &self,
        request: NwcExtensionWakeRequest,
        total_budget: Duration,
        cancellation: Arc<NwcExtensionCancellation>,
    ) -> NwcExtensionWakeResult {
        let started_at = Instant::now();
        let input = match validated_input(request) {
            Ok(input) => input,
            Err(()) => return rejected_result(),
        };
        let server_config = match extension_server_config(&self.data_dir) {
            Some(config) => config,
            None => return queued_result(),
        };
        let mnemonic = match self
            .secrets
            .get_secret(WALLET_SEED_KEY.to_string())
            .map(Zeroizing::new)
            .and_then(|seed| Mnemonic::from_str(seed.as_str()).ok())
        {
            Some(mnemonic) => mnemonic,
            None => return queued_result(),
        };
        if !ensure_nostr_secret(self.secrets.as_ref(), &mnemonic) {
            return queued_result();
        }
        if cancellation.is_cancelled() {
            return queued_result();
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
            Err(_) => return queued_result(),
        };
        if cancellation.is_cancelled() {
            return queued_result();
        }

        let remaining = total_budget.saturating_sub(started_at.elapsed());
        let budget = match OperationBudget::new(remaining) {
            Ok(budget) => budget,
            Err(_) => return queued_result(),
        };
        let service_pubkey = input.wallet_service_pubkey().clone();
        let ledger = match open_nwc_ledger(&self.data_dir) {
            Ok(ledger) => ledger,
            Err(_) => return queued_result(),
        };
        let wallet = RebelWalletBackend::new(wallet, service_pubkey);
        let relays = NostrRelayTransport;
        let secrets = RebelSecretProvider::new(self.secrets.clone());
        let clock = nwc_mobile::SystemClock;
        let engine = WakeEngine::new(
            &ledger,
            &wallet,
            &relays,
            &secrets,
            &clock,
            WakePolicy::default(),
        );
        disposition_result(engine.execute(input, budget, cancellation.as_ref()).await)
    }
}

fn validated_input(request: NwcExtensionWakeRequest) -> Result<WakeInput, ()> {
    let relay = nwc_mobile::SecureRelayUrl::parse(&request.relay_url).map_err(|_| ())?;
    let event_id = EventId::from_hex(&request.event_id_hex).map_err(|_| ())?;
    let wallet_service_pubkey =
        PublicKey::from_hex(&request.wallet_service_public_key_hex).map_err(|_| ())?;
    if request
        .embedded_event_json
        .as_ref()
        .is_some_and(|event| event.len() > 64 * 1024)
    {
        return Err(());
    }
    Ok(WakeInput::new(
        relay.as_str().to_string(),
        event_id,
        wallet_service_pubkey,
        request.embedded_event_json,
        UnixTimestamp::from_secs(request.received_at_seconds),
    ))
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

fn disposition_result(disposition: WakeDisposition) -> NwcExtensionWakeResult {
    match disposition {
        WakeDisposition::Completed { notification } => NwcExtensionWakeResult {
            disposition: NwcExtensionDisposition::Completed,
            notification: notification.into(),
        },
        WakeDisposition::AlreadyProcessed { notification } => NwcExtensionWakeResult {
            disposition: NwcExtensionDisposition::AlreadyProcessed,
            notification: notification.into(),
        },
        WakeDisposition::QueuedForApplication { notification, .. } => NwcExtensionWakeResult {
            disposition: NwcExtensionDisposition::QueuedForApplication,
            notification: notification.into(),
        },
        WakeDisposition::RetryAfter { notification, .. } => NwcExtensionWakeResult {
            disposition: NwcExtensionDisposition::RetryAfter,
            notification: notification.into(),
        },
        WakeDisposition::Rejected { notification, .. } => NwcExtensionWakeResult {
            disposition: NwcExtensionDisposition::Rejected,
            notification: notification.into(),
        },
        _ => queued_result(),
    }
}

impl From<NotificationHint> for NwcExtensionNotification {
    fn from(notification: NotificationHint) -> Self {
        match notification {
            NotificationHint::Processing => Self::Processing,
            NotificationHint::Completed => Self::Completed,
            NotificationHint::OpenApplication => Self::OpenApplication,
            _ => Self::OpenApplication,
        }
    }
}

fn queued_result() -> NwcExtensionWakeResult {
    disposition_result(WakeDisposition::QueuedForApplication {
        reason: QueueReason::WalletUnavailable,
        notification: NotificationHint::OpenApplication,
    })
}

fn rejected_result() -> NwcExtensionWakeResult {
    disposition_result(WakeDisposition::Rejected {
        code: RejectionCode::InvalidWakePayload,
        notification: NotificationHint::OpenApplication,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn request() -> NwcExtensionWakeRequest {
        NwcExtensionWakeRequest {
            relay_url: "wss://relay.example/path".to_string(),
            event_id_hex: HEX.to_string(),
            wallet_service_public_key_hex: HEX.to_string(),
            embedded_event_json: Some("{}".to_string()),
            received_at_seconds: 1_750_000_000,
        }
    }

    #[test]
    fn validates_native_wake_before_wallet_or_secret_access() {
        let input = validated_input(request()).expect("valid request");
        assert_eq!(input.relay(), "wss://relay.example/path");
        assert_eq!(input.event_id().to_hex(), HEX);
    }

    #[test]
    fn rejects_insecure_relays_and_oversized_embedded_events() {
        let mut insecure = request();
        insecure.relay_url = "ws://relay.example".to_string();
        assert!(validated_input(insecure).is_err());

        let mut oversized = request();
        oversized.embedded_event_json = Some("x".repeat(64 * 1024 + 1));
        assert!(validated_input(oversized).is_err());
    }

    #[test]
    fn maps_retry_without_exposing_remote_error_text() {
        let result = disposition_result(WakeDisposition::RetryAfter {
            delay: Duration::from_secs(1),
            reason: nwc_mobile::RetryReason::RelayUnavailable,
            notification: NotificationHint::Processing,
        });
        assert_eq!(result.disposition, NwcExtensionDisposition::RetryAfter);
        assert_eq!(result.notification, NwcExtensionNotification::Processing);
    }

    #[test]
    fn native_wake_debug_output_redacts_transport_content() {
        let debug = format!("{:?}", request());
        assert!(!debug.contains("relay.example"));
        assert!(!debug.contains(HEX));
        assert!(!debug.contains("{}"));
        assert!(debug.contains("has_embedded_event: true"));
    }
}
