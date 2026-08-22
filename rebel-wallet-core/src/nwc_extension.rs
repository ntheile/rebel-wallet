use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bip39::Mnemonic;
use nostr_sdk::prelude::ToBech32;
use nwc_mobile::{
    AtomicCancellation, CancellationSignal, NotificationHint, QueueReason, RejectionCode,
    WakeDisposition, WakeDispositionKind, WakeEnvelope,
};
use nwc_mobile_bark::execute_bark_wake;
use nwc_mobile_tokio::{run_bounded_background_wake, BackgroundWakeWindow};
use zeroize::Zeroizing;

use crate::core::{derive_nostr_keys_from_mnemonic, NOSTR_SECRET_KEY, WALLET_SEED_KEY};
use crate::nwc_mobile_adapter::{open_nwc_service, NostrRelayTransport, RebelSecretProvider};
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
        WakeEnvelope::from(self.clone()).fmt(formatter)
    }
}

impl From<NwcExtensionWakeRequest> for WakeEnvelope {
    fn from(request: NwcExtensionWakeRequest) -> Self {
        Self::new(
            request.relay_url,
            request.event_id_hex,
            request.wallet_service_public_key_hex,
            request.embedded_event_json,
            request.received_at_seconds,
        )
    }
}

impl From<WakeEnvelope> for NwcExtensionWakeRequest {
    fn from(envelope: WakeEnvelope) -> Self {
        Self {
            relay_url: envelope.relay_url().to_string(),
            event_id_hex: envelope.event_id_hex().to_string(),
            wallet_service_public_key_hex: envelope.wallet_service_public_key_hex().to_string(),
            embedded_event_json: envelope.embedded_event_json().map(str::to_string),
            received_at_seconds: envelope.received_at_seconds(),
        }
    }
}

#[uniffi::export]
pub fn parse_nwc_wake_payload_json(
    payload_json: String,
    received_at_seconds: u64,
) -> Option<NwcExtensionWakeRequest> {
    WakeEnvelope::parse_json(&payload_json, received_at_seconds)
        .ok()
        .map(Into::into)
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
    inner: AtomicCancellation,
}

#[uniffi::export]
impl NwcExtensionCancellation {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: AtomicCancellation::new(),
        })
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

impl CancellationSignal for NwcExtensionCancellation {
    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
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
        let total_budget = Duration::from_millis(execution_milliseconds);
        let execution_cancellation = cancellation.clone();
        disposition_result(
            run_bounded_background_wake(total_budget, cancellation.as_ref(), |window| {
                self.execute_wake_inner(request, window, execution_cancellation)
            })
            .await,
        )
    }
}

impl NwcExtensionEngine {
    async fn execute_wake_inner(
        &self,
        request: NwcExtensionWakeRequest,
        window: BackgroundWakeWindow,
        cancellation: Arc<NwcExtensionCancellation>,
    ) -> WakeDisposition {
        let input = match WakeEnvelope::from(request).validate() {
            Ok(input) => input,
            Err(_) => return rejected_disposition(),
        };
        let server_config = match extension_server_config(&self.data_dir) {
            Some(config) => config,
            None => return queued_disposition(),
        };
        let mnemonic = match self
            .secrets
            .get_secret(WALLET_SEED_KEY.to_string())
            .map(Zeroizing::new)
            .and_then(|seed| Mnemonic::from_str(seed.as_str()).ok())
        {
            Some(mnemonic) => mnemonic,
            None => return queued_disposition(),
        };
        if !ensure_nostr_secret(self.secrets.as_ref(), &mnemonic) {
            return queued_disposition();
        }
        if cancellation.is_cancelled() {
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
        if cancellation.is_cancelled() {
            return queued_disposition();
        }

        let budget = match window.operation_budget() {
            Some(budget) => budget,
            None => return queued_disposition(),
        };
        let service = match open_nwc_service(&self.data_dir) {
            Ok(service) => service,
            Err(_) => return queued_disposition(),
        };
        let relays = NostrRelayTransport;
        let secrets = RebelSecretProvider::new(self.secrets.clone());
        execute_bark_wake(
            service.ledger(),
            wallet,
            &relays,
            &secrets,
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

fn disposition_result(disposition: WakeDisposition) -> NwcExtensionWakeResult {
    let notification = disposition.notification().into();
    let disposition = match disposition.kind() {
        WakeDispositionKind::Completed => NwcExtensionDisposition::Completed,
        WakeDispositionKind::AlreadyProcessed => NwcExtensionDisposition::AlreadyProcessed,
        WakeDispositionKind::QueuedForApplication => NwcExtensionDisposition::QueuedForApplication,
        WakeDispositionKind::RetryAfter => NwcExtensionDisposition::RetryAfter,
        WakeDispositionKind::Rejected => NwcExtensionDisposition::Rejected,
        _ => NwcExtensionDisposition::QueuedForApplication,
    };
    NwcExtensionWakeResult {
        disposition,
        notification,
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

fn rejected_result() -> NwcExtensionWakeResult {
    disposition_result(rejected_disposition())
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
    fn shared_push_parser_is_exposed_through_uniffi_wrapper() {
        let canonical = serde_json::json!({
            "nwc_relay": "wss://relay.example/path",
            "nwc_event_id": HEX,
            "nwc_wallet_service_pubkey": HEX,
            "nwc_event_json": "{}"
        });
        let parsed =
            parse_nwc_wake_payload_json(canonical.to_string(), 42).expect("canonical payload");
        assert_eq!(parsed.received_at_seconds, 42);
        assert_eq!(parsed.relay_url, "wss://relay.example/path");

        assert!(parse_nwc_wake_payload_json("{}".to_string(), 42).is_none());
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
