use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bip39::Mnemonic;
use nostr_sdk::prelude::ToBech32;
use nwc_mobile::{QueueReason, RejectionCode, WakeDisposition};
use nwc_mobile_bark::execute_bark_wake;
use nwc_mobile_tokio::{run_bounded_background_wake, BackgroundWakeWindow};
use nwc_mobile_uniffi::{
    validate_wake_envelope, MobileCancellation, MobileWakeDisposition, MobileWakeEnvelope,
};
use zeroize::Zeroizing;

use crate::core::{derive_nostr_keys_from_mnemonic, NOSTR_SECRET_KEY, WALLET_SEED_KEY};
use crate::nwc_mobile_adapter::{open_nwc_service, NostrRelayTransport, RebelSecretProvider};
use crate::persistence::{PersistedAppData, ServerConfig};
use crate::wallet::{open_bark_wallet, WalletOpenMode};
use crate::{SecretStore, WalletNetwork};

const APP_DATA_FILE: &str = "rebel-app-data.json";
const MAX_EXTENSION_EXECUTION_MILLISECONDS: u64 = 30_000;

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
        if cancellation.is_cancelled() {
            return queued_disposition();
        }

        let Some(budget) = window.operation_budget() else {
            return queued_disposition();
        };
        let service = match open_nwc_service(&self.data_dir) {
            Ok(service) => service,
            Err(_) => return queued_disposition(),
        };
        execute_bark_wake(
            service.ledger(),
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
