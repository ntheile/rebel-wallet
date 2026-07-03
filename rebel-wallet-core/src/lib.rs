use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;

use flume::{Receiver, Sender};
use nostr_sdk::prelude::ToBech32;

mod actions;
mod activity;
mod core;
mod custom_address;
mod nostr_support;
mod nwc;
mod payments;
mod persistence;
mod price;
mod profile_cache;
mod state;
mod time;
mod updates;
mod wallet;
mod zaps;

pub use actions::AppAction;
use profile_cache::normalize_profile_picture_to_jpeg;
pub use state::{
    ActivityIconKind, ActivityItem, AppState, BusyState, CapabilityRequest, CapabilityRequestKind,
    Contact, CurrencyOption, LightningAddressRegistrationPhase, LightningAddressState, MainTab,
    NetworkOption, NostrMessage, NostrState, NwcBudgetInterval, NwcConnection, NwcPermission,
    NwcProcessedWakeRequest, NwcState, NwcWakeRequest, PriceCurrency, PushNotificationState,
    ReceiveMethod, ReceivePhase, ReceiveState, Router, Screen, SendDestinationKind, SendPhase,
    SendState, SetupState, WalletNetwork, WalletState,
};
pub use updates::{AppUpdate, HapticFeedback};
pub(crate) use updates::{AsyncMsg, CoreMsg};

uniffi::setup_scaffolding!();

pub(crate) const SIGNET_SERVER: &str = "https://ark.signet.2nd.dev";
pub(crate) const SIGNET_ESPLORA: &str = "https://esplora.signet.2nd.dev";
pub(crate) const MAINNET_SERVER: &str = "https://ark.second.tech";
pub(crate) const MAINNET_ESPLORA: &str = "https://mempool.second.tech/api";

#[uniffi::export(callback_interface)]
pub trait AppReconciler: Send + Sync + 'static {
    fn reconcile(&self, update: AppUpdate);
}

#[uniffi::export(callback_interface)]
pub trait SecretStore: Send + Sync + 'static {
    fn get_secret(&self, key: String) -> Option<String>;
    fn set_secret(&self, key: String, value: String) -> bool;
    fn delete_secret(&self, key: String) -> bool;
}

#[derive(uniffi::Object)]
pub struct FfiApp {
    core_tx: Sender<CoreMsg>,
    update_rx: Receiver<AppUpdate>,
    listening: AtomicBool,
    shared_state: Arc<RwLock<AppState>>,
    secrets: Arc<dyn SecretStore>,
    data_dir: PathBuf,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct NwcExtensionWakeResult {
    pub success: bool,
    pub message: String,
    pub updated_snapshot_json: Option<String>,
}

#[uniffi::export]
impl FfiApp {
    #[uniffi::constructor]
    pub fn new(
        data_dir: String,
        cache_dir: String,
        secret_store: Box<dyn SecretStore>,
    ) -> Arc<Self> {
        let (update_tx, update_rx) = flume::unbounded();
        let (core_tx, core_rx) = flume::unbounded::<CoreMsg>();
        let shared_state = Arc::new(RwLock::new(AppState::initial()));
        let shared_for_core = shared_state.clone();
        let data_dir = PathBuf::from(data_dir);
        let cache_dir = PathBuf::from(cache_dir);
        let secrets: Arc<dyn SecretStore> = Arc::from(secret_store);
        let tx_for_bootstrap = core_tx.clone();

        core::spawn_actor(
            data_dir.clone(),
            cache_dir,
            secrets.clone(),
            tx_for_bootstrap,
            core_rx,
            shared_for_core,
            update_tx,
        );

        Arc::new(Self {
            core_tx,
            update_rx,
            listening: AtomicBool::new(false),
            shared_state,
            secrets,
            data_dir,
        })
    }

    pub fn state(&self) -> AppState {
        match self.shared_state.read() {
            Ok(g) => g.clone(),
            Err(poison) => poison.into_inner().clone(),
        }
    }

    pub fn dispatch(&self, action: AppAction) {
        let _ = self.core_tx.send(CoreMsg::Action(action));
    }

    pub fn listen_for_updates(&self, reconciler: Box<dyn AppReconciler>) {
        if self
            .listening
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let rx = self.update_rx.clone();
        thread::spawn(move || {
            while let Ok(update) = rx.recv() {
                reconciler.reconcile(update);
            }
        });
    }

    pub fn normalize_profile_image_to_jpeg(&self, image_bytes: Vec<u8>) -> Option<Vec<u8>> {
        normalize_profile_picture_to_jpeg(&image_bytes).ok()
    }

    pub fn nwc_wake_snapshot_json(&self) -> Option<String> {
        let state = self.state();
        if state.nwc.connections.is_empty() {
            return None;
        }

        let nostr_secret = self
            .secrets
            .get_secret(core::NOSTR_SECRET_KEY.to_string())
            .or_else(|| {
                let mnemonic = self.secrets.get_secret(core::WALLET_SEED_KEY.to_string())?;
                let keys = core::derive_nostr_keys_from_mnemonic(&mnemonic).ok()?;
                let nsec = keys.secret_key().to_bech32().ok()?;
                let _ = self
                    .secrets
                    .set_secret(core::NOSTR_SECRET_KEY.to_string(), nsec.clone());
                Some(nsec)
            })?;
        let wallet_seed = self.secrets.get_secret(core::WALLET_SEED_KEY.to_string());

        nwc::build_nwc_wake_snapshot(
            nostr_secret,
            wallet_seed,
            self.data_dir.to_string_lossy().to_string(),
            state.wallet.balance_sat,
            state.wallet.network,
            state.nwc.connections,
        )
        .ok()
    }
}

#[uniffi::export]
pub fn process_nwc_wake_from_snapshot(
    snapshot_json: String,
    relay: String,
    event_id: String,
    wallet_service_pubkey: String,
) -> NwcExtensionWakeResult {
    let wake = NwcWakeRequest {
        relay,
        event_id,
        wallet_service_pubkey,
        received_at: time::now_unix(),
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            return NwcExtensionWakeResult {
                success: false,
                message: format!("failed to start NWC wake runtime: {e:#}"),
                updated_snapshot_json: None,
            }
        }
    };

    match runtime.block_on(nwc::process_nwc_wake_from_snapshot(snapshot_json, wake)) {
        Ok(processed) => NwcExtensionWakeResult {
            success: true,
            message: format!(
                "NSE responded to {} request {}",
                processed.method, processed.wake.event_id
            ),
            updated_snapshot_json: processed.updated_snapshot_json,
        },
        Err(e) => NwcExtensionWakeResult {
            success: false,
            message: format!("{e:#}"),
            updated_snapshot_json: None,
        },
    }
}

#[uniffi::export]
pub fn process_nwc_event_from_snapshot(
    snapshot_json: String,
    relay: String,
    event_id: String,
    wallet_service_pubkey: String,
    event_json: String,
) -> NwcExtensionWakeResult {
    let wake = NwcWakeRequest {
        relay,
        event_id,
        wallet_service_pubkey,
        received_at: time::now_unix(),
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            return NwcExtensionWakeResult {
                success: false,
                message: format!("failed to start NWC wake runtime: {e:#}"),
                updated_snapshot_json: None,
            }
        }
    };

    match runtime.block_on(nwc::process_nwc_event_from_snapshot(
        snapshot_json,
        wake,
        event_json,
    )) {
        Ok(processed) => NwcExtensionWakeResult {
            success: true,
            message: format!(
                "NSE responded to {} request {}",
                processed.method, processed.wake.event_id
            ),
            updated_snapshot_json: processed.updated_snapshot_json,
        },
        Err(e) => NwcExtensionWakeResult {
            success: false,
            message: format!("{e:#}"),
            updated_snapshot_json: None,
        },
    }
}
