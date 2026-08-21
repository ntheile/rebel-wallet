use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use bip39::Mnemonic;
use zeroize::Zeroizing;

use super::custom_address_flow::{
    lightning_address_local_part, pending_custom_lightning_address_matches_name,
};
use super::{nwc_client_secret_key, AppCore, NOSTR_SECRET_KEY, WALLET_SEED_KEY};
use crate::custom_address::amount_msats_to_sat;
use crate::nwc_mobile_adapter::{nwc_ledger_path, open_nwc_ledger};
use crate::persistence::{PersistedAppData, PersistedPriceCurrency, ServerConfig};
use crate::profile_cache::{
    hydrate_contact_picture, hydrate_own_profile_picture, sanitize_persisted_contact_pictures,
};
use crate::updates::{AsyncMsg, CoreMsg, HapticFeedback};
use crate::wallet::{open_bark_wallet, remove_wallet_database_files, WalletOpenMode};
use crate::{AppState, LightningAddressRegistrationPhase, PriceCurrency, WalletNetwork};

impl AppCore {
    pub(super) fn bootstrap(&mut self) {
        self.load_app_data();
        self.refresh_cached_contact_profiles_on_startup();
        self.load_nostr_key();
        self.sync_nwc_push_registrations();
        self.refresh_price();
        if let Some(mnemonic) = self.secrets.get_secret(WALLET_SEED_KEY.to_string()) {
            self.state.busy.bootstrapping = true;
            self.state.busy.opening_wallet = true;
            self.open_wallet(Zeroizing::new(mnemonic), WalletOpenMode::OpenOrCreate);
        }
    }

    pub(super) fn open_wallet(&mut self, mnemonic: Zeroizing<String>, mode: WalletOpenMode) {
        let generation = self.invalidate_wallet_session();
        let tx = self.tx.clone();
        let data_dir = self.data_dir.clone();
        let server_config = ServerConfig::from_wallet(&self.state.wallet);
        self.rt.spawn(async move {
            let result = async {
                let mnemonic =
                    Mnemonic::from_str(mnemonic.as_str()).context("invalid recovery phrase")?;
                let opened = open_bark_wallet(data_dir, &mnemonic, mode, server_config).await?;
                Ok::<_, anyhow::Error>((opened, Zeroizing::new(mnemonic.to_string())))
            }
            .await;
            let msg = match result {
                Ok((opened, mnemonic)) => AsyncMsg::WalletReady {
                    generation,
                    wallet: opened.wallet,
                    mnemonic,
                    recovery_notice: opened.recovery_notice,
                },
                Err(e) => AsyncMsg::WalletOpenFailed {
                    generation,
                    message: format!("Wallet setup failed: {e:#}"),
                },
            };
            let _ = tx.send(CoreMsg::Async(msg));
        });
    }

    pub(super) fn delete_wallet(&mut self) {
        self.invalidate_wallet_session();

        // Remove local data first. Only delete the secrets once the data they
        // unlock is confirmed gone, so a failed cleanup cannot orphan the
        // databases behind an already-deleted seed.
        let mut errors = Vec::new();
        let nwc_secrets = self
            .state
            .nwc
            .connections
            .iter()
            .map(|connection| (connection.client_pubkey.clone(), connection.name.clone()))
            .collect::<Vec<_>>();
        for network in [
            WalletNetwork::Mainnet,
            WalletNetwork::Signet,
            WalletNetwork::Regtest,
        ] {
            let db_path = self.data_dir.join(network.db_file_name());
            if let Err(e) = remove_wallet_database_files(&db_path) {
                errors.push(format!("{e:#}"));
            }
        }
        self.nwc_ledger = None;
        let nwc_database_path = nwc_ledger_path(&self.data_dir);
        if let Err(error) = remove_wallet_database_files(&nwc_database_path) {
            errors.push(format!("{error:#}"));
        }

        match std::fs::remove_file(&self.app_data_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => errors.push(format!(
                "failed to remove {}: {e}",
                self.app_data_path.display()
            )),
        }

        self.payment_annotations.clear();
        self.zap_receipts.clear();
        self.profile_picture_downloads.clear();
        self.profile_info_requests.clear();
        self.nwc_in_flight_wake_requests.clear();

        let mut state = AppState::initial();
        state.show_launch_splash = false;
        self.state = state;

        self.nwc_ledger = open_nwc_ledger(&self.data_dir).ok();
        self.nwc_registry_ready = self.nwc_ledger.is_some();
        if self.nwc_ledger.is_none() {
            errors.push("NWC authorization storage".to_string());
        }

        if errors.is_empty() {
            if !self.secrets.delete_secret(WALLET_SEED_KEY.to_string()) {
                errors.push("wallet seed".to_string());
            }
            let _ = self.secrets.delete_secret(NOSTR_SECRET_KEY.to_string());
            for (client_pubkey, name) in nwc_secrets {
                if !self
                    .secrets
                    .delete_secret(nwc_client_secret_key(&client_pubkey))
                {
                    errors.push(format!("NWC secret for {name}"));
                }
            }
        }

        if errors.is_empty() {
            self.state.toast = Some("Wallet deleted. Start over to create or restore.".to_string());
            self.request_haptic(HapticFeedback::NotificationSuccess);
        } else {
            self.state.toast = Some(format!(
                "Wallet reset with cleanup warnings: {}",
                errors.join(", ")
            ));
            self.request_haptic(HapticFeedback::NotificationWarning);
        }
    }

    pub(super) fn select_network(
        &mut self,
        network: WalletNetwork,
        server_address: Option<String>,
        esplora_address: Option<String>,
    ) {
        let server_config = match network {
            WalletNetwork::Regtest => {
                let server_address =
                    server_address.unwrap_or_else(|| network.server_address().to_string());
                let esplora_address =
                    esplora_address.unwrap_or_else(|| network.esplora_address().to_string());
                let server_address = match validate_server_url(&server_address, "ASP") {
                    Ok(address) => address,
                    Err(message) => {
                        self.state.toast = Some(message);
                        self.request_haptic(HapticFeedback::NotificationWarning);
                        return;
                    }
                };
                let esplora_address = match validate_server_url(&esplora_address, "Esplora") {
                    Ok(address) => address,
                    Err(message) => {
                        self.state.toast = Some(message);
                        self.request_haptic(HapticFeedback::NotificationWarning);
                        return;
                    }
                };
                ServerConfig {
                    network,
                    server_address,
                    server_access_token: network.server_access_token().map(str::to_string),
                    esplora_address,
                }
            }
            WalletNetwork::Mainnet | WalletNetwork::Signet => ServerConfig::for_network(network),
        };
        let server_address = server_config.server_address;
        let esplora_address = server_config.esplora_address;

        let wallet_server_changed = self.state.wallet.server_address != server_address
            || self.state.wallet.esplora_address != esplora_address;
        let changed = self.state.wallet.network != network || wallet_server_changed;
        self.state.wallet.network = network;
        self.state.wallet.server_address = server_address;
        self.state.wallet.esplora_address = esplora_address;
        self.state.lightning_address.backing_ark_address = None;
        self.save_app_data();

        if wallet_server_changed {
            if let Some(seed) = self.secrets.get_secret(WALLET_SEED_KEY.to_string()) {
                self.state.busy.opening_wallet = true;
                self.open_wallet(Zeroizing::new(seed), WalletOpenMode::OpenOrCreate);
                self.state.toast = Some("Network changed. Reconnecting wallet.".to_string());
                self.request_haptic(HapticFeedback::NotificationSuccess);
            } else {
                self.state.toast = Some("Network changed.".to_string());
                self.request_haptic(HapticFeedback::NotificationSuccess);
            }
        } else if changed {
            self.ensure_lightning_address();
            self.state.toast = Some("Network changed.".to_string());
            self.request_haptic(HapticFeedback::NotificationSuccess);
        } else {
            self.state.toast = Some("Network already selected.".to_string());
            self.request_haptic(HapticFeedback::NotificationWarning);
        }
    }

    pub(super) fn set_price_currency(&mut self, currency: PriceCurrency) {
        self.state.wallet.price_currency = currency;
        self.state.wallet.btc_price = None;
        self.save_app_data();
        self.request_haptic(HapticFeedback::NotificationSuccess);
        self.refresh_price();
    }

    pub(super) fn load_lightning_address_ark_address(&self) -> Option<String> {
        load_wallet_metadata_value(
            &self.data_dir,
            self.state.wallet.network,
            "lightning_address_ark_address",
        )
    }

    pub(super) fn save_lightning_address_ark_address(&self, address: &str) {
        let _ = save_wallet_metadata_value(
            &self.data_dir,
            self.state.wallet.network,
            "lightning_address_ark_address",
            address,
        );
    }

    pub(super) fn load_app_data(&mut self) {
        let Ok(raw) = std::fs::read_to_string(&self.app_data_path) else {
            return;
        };
        match serde_json::from_str::<PersistedAppData>(&raw) {
            Ok(data) => {
                self.state.nostr = data.nostr;
                self.hydrate_cached_profile_pictures();
                self.sort_contacts();
                self.state.receive.amount_sat = data.receive_amount_sat;
                self.state.receive.memo = if data.receive_memo == "Rebel Wallet" {
                    String::new()
                } else {
                    data.receive_memo
                };
                self.state.wallet.network = data.network;
                let server_config = if self.state.wallet.network == WalletNetwork::Regtest
                    && data.servers.network == WalletNetwork::Regtest
                {
                    data.servers
                } else {
                    ServerConfig::for_network(self.state.wallet.network)
                };
                self.state.wallet.server_address = server_config.server_address;
                self.state.wallet.esplora_address = server_config.esplora_address;
                self.state.wallet.price_currency = data.price_currency.currency;
                self.state.lightning_address.backing_ark_address = data
                    .lightning_address_ark_address
                    .filter(|address| !address.trim().is_empty());
                self.state.lightning_address.custom_address = data
                    .custom_lightning_address
                    .filter(|address| !address.trim().is_empty());
                self.state.lightning_address.custom_name = data.custom_lightning_address_name;
                let pending_custom_lightning_address = data
                    .pending_custom_lightning_address
                    .filter(pending_custom_lightning_address_matches_name);
                if let Some(pending) = pending_custom_lightning_address {
                    self.state.lightning_address.custom_name = pending.name;
                    self.state.lightning_address.backing_ark_address =
                        Some(pending.ark_address.clone());
                    self.state.lightning_address.registration_address =
                        Some(pending.lightning_address);
                    self.state
                        .lightning_address
                        .registration_payment_ark_address =
                        pending.payment_ark_address.or(Some(pending.ark_address));
                    self.state.lightning_address.registration_invoice = Some(pending.invoice);
                    self.state.lightning_address.registration_purchase_id =
                        Some(pending.purchase_id);
                    self.state.lightning_address.registration_amount_sat =
                        amount_msats_to_sat(pending.amount_msats).unwrap_or(0);
                    self.state.lightning_address.registration_phase =
                        LightningAddressRegistrationPhase::AwaitingPayment;
                    self.state.lightning_address.registration_status_text =
                        "Awaiting payment".to_string();
                } else if self
                    .state
                    .lightning_address
                    .custom_address
                    .as_ref()
                    .is_some_and(|address| !address.trim().is_empty())
                {
                    self.restore_active_custom_lightning_address_name();
                    self.state.lightning_address.registration_phase =
                        LightningAddressRegistrationPhase::Active;
                    self.state.lightning_address.registration_status_text = "Active".to_string();
                }
                self.payment_annotations = data.payment_annotations;
                self.zap_receipts = data.zap_receipts;
                self.state.nwc.connections = data.nwc_connections;
                self.migrate_nwc_connections();
                self.hydrate_nwc_connection_uris();
                self.hydrate_nwc_icon_urls();
                self.prefetch_nwc_icons();
            }
            Err(e) => {
                self.state.toast = Some(format!("Could not load local app data: {e}"));
            }
        }
    }

    pub(super) fn hydrate_cached_profile_pictures(&mut self) {
        let cache_dir = self.cache_dir.clone();
        let profile_db = self.profile_db.as_ref();
        hydrate_own_profile_picture(profile_db, &cache_dir, &mut self.state.nostr);
        for contact in &mut self.state.nostr.contacts {
            hydrate_contact_picture(profile_db, &cache_dir, contact);
        }
        for contact in &mut self.state.send.global_search_results {
            hydrate_contact_picture(profile_db, &cache_dir, contact);
        }
    }

    pub(super) fn sort_contacts(&mut self) {
        crate::state::sort_contacts_by_name_npub(&mut self.state.nostr.contacts);
    }

    pub(super) fn save_app_data(&self) {
        let mut nostr = self.state.nostr.clone();
        sanitize_persisted_contact_pictures(self.profile_db.as_ref(), &mut nostr.contacts);
        let pending_custom_lightning_address = self.pending_custom_lightning_address();
        let custom_lightning_address_name = pending_custom_lightning_address
            .as_ref()
            .map(|pending| pending.name.clone())
            .or_else(|| {
                self.state
                    .lightning_address
                    .custom_address
                    .as_deref()
                    .and_then(lightning_address_local_part)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| self.state.lightning_address.custom_name.clone());
        let mut nwc_connections = self.state.nwc.connections.clone();
        super::redact_persisted_nwc_connection_secrets(&mut nwc_connections);

        let data = PersistedAppData {
            nostr,
            receive_amount_sat: self.state.receive.amount_sat,
            receive_memo: self.state.receive.memo.clone(),
            network: self.state.wallet.network,
            servers: ServerConfig::from_wallet(&self.state.wallet),
            price_currency: PersistedPriceCurrency {
                currency: self.state.wallet.price_currency.clone(),
            },
            lightning_address_ark_address: None,
            custom_lightning_address: self.state.lightning_address.custom_address.clone(),
            custom_lightning_address_name,
            pending_custom_lightning_address,
            payment_annotations: self.payment_annotations.clone(),
            zap_receipts: self.zap_receipts.clone(),
            nwc_connections,
        };
        if let Ok(raw) = serde_json::to_string_pretty(&data) {
            let _ = std::fs::create_dir_all(&self.data_dir);
            let _ = std::fs::write(&self.app_data_path, raw);
        }
    }
}

fn validate_server_url(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    let url = reqwest::Url::parse(value).map_err(|_| format!("Enter a valid {label} URL."))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("Enter a valid HTTP or HTTPS {label} URL."));
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn load_wallet_metadata_value(
    data_dir: &Path,
    network: WalletNetwork,
    key: &str,
) -> Option<String> {
    let db_path = data_dir.join(network.db_file_name());
    let conn = rusqlite::Connection::open(db_path).ok()?;
    ensure_wallet_metadata_table(&conn).ok()?;
    conn.query_row(
        "SELECT value FROM rebel_wallet_metadata WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|value| !value.trim().is_empty())
}

fn save_wallet_metadata_value(
    data_dir: &Path,
    network: WalletNetwork,
    key: &str,
    value: &str,
) -> rusqlite::Result<()> {
    std::fs::create_dir_all(data_dir).ok();
    let db_path = data_dir.join(network.db_file_name());
    let conn = rusqlite::Connection::open(db_path)?;
    ensure_wallet_metadata_table(&conn)?;
    conn.execute(
        "INSERT INTO rebel_wallet_metadata (key, value)
         VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

fn ensure_wallet_metadata_table(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS rebel_wallet_metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_server_url;

    #[test]
    fn validates_and_normalizes_server_urls() {
        assert_eq!(
            validate_server_url("  http://192.168.1.10:3535/  ", "ASP"),
            Ok("http://192.168.1.10:3535".to_string())
        );
        assert!(validate_server_url("ftp://example.com", "ASP").is_err());
        assert!(validate_server_url("not a url", "Esplora").is_err());
    }
}
