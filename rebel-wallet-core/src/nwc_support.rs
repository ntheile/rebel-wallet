//! Rebel Wallet's NWC orchestration and its narrow `AppCore` integration.

use std::time::Duration;

use anyhow::Context;
use nostr_sdk::prelude::{Keys, PublicKey as NostrPublicKey};
use nwc_mobile::{
    parse_connection_relays, ApplicationConnectionMetadata, ForegroundWakeDecision,
    ForegroundWakeOutcome, ForegroundWakeRetryCause, NeverCancelled, NwcApplicationManager,
    OperationBudget, UnixTimestamp, WakeDisposition, WakeEnvelope, WalletConnectionRequest,
    DEFAULT_MAXIMUM_CONNECTION_RELAYS,
};
use nwc_mobile_bark::execute_bark_wake;
use nwc_mobile_uniffi::{MobileConnectionMetadata, MobileConnectionView};

use super::AppCore;
use crate::nostr_support::public_key_from_npub_or_hex;
use crate::nwc::{
    publish_nwc_info_event, NostrRelayTransport, NwcPushConfig, RebelSecretProvider, NWC_ENCRYPTION,
};
use crate::updates::{AppUpdate, AsyncMsg, CoreMsg, HapticFeedback};
use crate::wallet::remove_wallet_database_files;
use crate::{NwcBudgetInterval, NwcPermission, NwcProcessedWakeRequest, NwcWakeRequest};

const NWC_CLIENT_SECRET_KEY_PREFIX: &str = "nwc_client_secret:";
const MAX_NWC_WAKE_HISTORY: usize = 30;
const NWC_INFO_EVENT_PUBLISH_ATTEMPTS: u8 = 3;
const NWC_FOREGROUND_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct NwcWalletCleanup {
    secrets: Vec<(String, String)>,
}

fn nwc_client_secret_key(client_pubkey: &str) -> String {
    format!("{NWC_CLIENT_SECRET_KEY_PREFIX}{client_pubkey}")
}

fn nwc_info_event_key(client_pubkey: &str, relay: &str) -> String {
    format!("{client_pubkey}|{relay}")
}

async fn publish_nwc_info_event_with_retry(
    relay: String,
    keys: Keys,
    client_pubkey: Option<NostrPublicKey>,
    permissions: Vec<NwcPermission>,
) -> anyhow::Result<()> {
    nwc_mobile_tokio::retry_with_exponential_backoff(
        NWC_INFO_EVENT_PUBLISH_ATTEMPTS,
        Duration::from_secs(1),
        || {
            publish_nwc_info_event(
                relay.clone(),
                keys.clone(),
                client_pubkey,
                permissions.clone(),
            )
        },
    )
    .await
}

fn parse_nwc_relay_urls(value: &str, fallback: &str) -> anyhow::Result<Vec<String>> {
    parse_connection_relays(value, fallback, DEFAULT_MAXIMUM_CONNECTION_RELAYS).map_err(Into::into)
}

pub(crate) fn nwc_relay_input_is_valid(value: &str) -> bool {
    parse_nwc_relay_urls(value, "").is_ok()
}

pub(crate) fn nwc_budget_interval_display(interval: NwcBudgetInterval) -> &'static str {
    match interval {
        NwcBudgetInterval::Never => "Never",
        NwcBudgetInterval::Hourly => "Hourly",
        NwcBudgetInterval::Daily => "Daily",
        NwcBudgetInterval::Weekly => "Weekly",
        NwcBudgetInterval::Monthly => "Monthly",
        NwcBudgetInterval::Yearly => "Yearly",
    }
}

impl AppCore {
    pub(super) fn reset_nwc_wallet_session(&mut self) {
        self.nwc_wake_coordinator.reset();
    }

    pub(super) fn load_nwc_connections(&mut self) {
        let Some(manager) = self.nwc_manager.as_ref() else {
            self.state.nwc.last_wake_status =
                "NWC authorization storage is unavailable".to_string();
            return;
        };
        let presentations = match manager.connections() {
            Ok(presentations) => presentations,
            Err(error) => {
                self.state.nwc.last_wake_status =
                    format!("NWC authorization storage is unavailable: {error}");
                return;
            }
        };
        let connections = presentations
            .into_iter()
            .enumerate()
            .map(|(index, presentation)| {
                let wallet_managed_secret = self
                    .secrets
                    .get_secret(nwc_client_secret_key(presentation.client_pubkey_hex()))
                    .is_some();
                MobileConnectionView::from_presentation(
                    presentation,
                    format!("NWC {}", index + 1),
                    None,
                    wallet_managed_secret,
                )
                .map_err(anyhow::Error::from)
            })
            .collect::<anyhow::Result<Vec<_>>>();
        match connections {
            Ok(connections) => {
                self.state.nwc.connections = connections;
            }
            Err(error) => {
                self.state.nwc.last_wake_status =
                    format!("NWC authorization data is invalid: {error:#}");
            }
        }
    }

    pub(super) fn refresh_nwc_connection_usage(&mut self) {
        let Some(manager) = self.nwc_manager.as_ref() else {
            return;
        };
        let Ok(presentations) = manager.connections() else {
            self.state.nwc.last_wake_status =
                "NWC connection usage is temporarily unavailable".to_string();
            return;
        };
        for connection in &mut self.state.nwc.connections {
            if let Some(presentation) = presentations
                .iter()
                .find(|presentation| presentation.id() == connection.id)
            {
                connection.last_used_at = presentation.last_used_at().map(UnixTimestamp::as_secs);
                connection.spent_sat = presentation.spent_sat();
                connection.budget_period_started_at =
                    presentation.budget_period_started_at().as_secs();
            }
        }
    }

    pub(super) fn create_nwc_connection(
        &mut self,
        name: String,
        relay: String,
        budget_sat: u64,
        budget_interval: NwcBudgetInterval,
        permissions: Vec<NwcPermission>,
    ) {
        if self.nwc_manager.is_none() {
            self.state.toast = Some("NWC authorization storage is unavailable.".to_string());
            self.request_haptic(HapticFeedback::NotificationError);
            return;
        }
        if !self.ensure_wallet_derived_nostr_key() {
            self.state.toast = Some("Create or open the wallet before adding NWC.".to_string());
            self.request_haptic(HapticFeedback::NotificationWarning);
            return;
        }

        let service_keys = match self.nostr_keys() {
            Ok(keys) => keys,
            Err(e) => {
                self.state.toast = Some(format!("{e:#}"));
                self.request_haptic(HapticFeedback::NotificationError);
                return;
            }
        };

        let provider = RebelSecretProvider::new(self.secrets.clone());
        let created = self
            .nwc_manager
            .as_ref()
            .expect("checked above")
            .create_connection(
                WalletConnectionRequest::new(
                    service_keys.public_key().to_hex(),
                    relay,
                    self.state.nwc.default_relay.clone(),
                    permissions.into_iter().map(Into::into).collect(),
                    budget_sat,
                    budget_interval.into(),
                    NWC_ENCRYPTION,
                    None,
                    self.state.lightning_address.address.clone(),
                ),
                &provider,
            );
        let created = match created {
            Ok(created) => created,
            Err(error) => {
                self.state.toast = Some(format!("Could not create NWC connection: {error}"));
                self.request_haptic(HapticFeedback::NotificationError);
                return;
            }
        };
        let draft = created.draft();
        let relay_storage = draft.relay_storage().to_string();
        let trimmed_name = name.trim();
        let display_name = if trimmed_name.is_empty() {
            format!("NWC {}", self.state.nwc.connections.len() + 1)
        } else {
            trimmed_name.to_string()
        };
        let metadata = ApplicationConnectionMetadata::new(
            display_name.clone(),
            None,
            draft.authorization().relay_urls().to_vec(),
        );
        let metadata_result = metadata.and_then(|metadata| {
            self.nwc_manager
                .as_ref()
                .expect("checked above")
                .set_connection_metadata(draft.id(), metadata)
                .map_err(|_| nwc_mobile::RegistryError::InvalidConnection)
        });
        if metadata_result.is_err() {
            let _ = self
                .nwc_manager
                .as_ref()
                .expect("checked above")
                .revoke_connection(draft.id(), &provider);
            self.state.toast = Some("Could not persist NWC connection metadata.".to_string());
            self.request_haptic(HapticFeedback::NotificationError);
            return;
        }
        let connection = crate::NwcConnection::from_created(
            &created,
            MobileConnectionMetadata {
                name: display_name,
                icon_url: None,
                icon_display_url: None,
                wallet_managed_secret: true,
            },
        )
        .expect("shared workflow result must produce a native connection view");
        self.state.nwc.connections.push(connection);
        self.state.nwc.default_relay = relay_storage;
        self.state.toast = Some("NWC string created.".to_string());
        self.request_haptic(HapticFeedback::NotificationSuccess);
        self.save_app_data();
        self.publish_pending_nwc_info_events();
        self.sync_nwc_push_registrations();
        let connection = self.state.nwc.connections.last().expect("just inserted");
        self.pending_side_effects
            .push(AppUpdate::NwcConnectionExportReady {
                rev: self.rev + 1,
                connection_id: connection.id.clone(),
                name: connection.name.clone(),
                uri: created.uri().to_owned(),
                copy_to_clipboard: false,
                present_qr: true,
            });
    }

    pub(super) fn delete_nwc_connection(&mut self, id: String) {
        let deleted_connections = self
            .state
            .nwc
            .connections
            .iter()
            .filter(|connection| connection.id == id)
            .cloned()
            .collect::<Vec<_>>();
        if deleted_connections.is_empty() {
            return;
        }
        let provider = RebelSecretProvider::new(self.secrets.clone());
        let revocation_result = self
            .nwc_manager
            .as_ref()
            .context("NWC authorization storage is unavailable")
            .and_then(|manager| {
                deleted_connections.iter().try_for_each(|connection| {
                    let revoked = manager
                        .revoke_connection(&connection.id, &provider)
                        .context("could not revoke the NWC authorization")?;
                    anyhow::ensure!(
                        revoked.client_secret_deleted(),
                        "could not delete the NWC client secret"
                    );
                    Ok(())
                })
            });
        if let Err(error) = revocation_result {
            self.state.toast = Some(format!("Could not revoke NWC connection: {error:#}"));
            self.request_haptic(HapticFeedback::NotificationError);
            return;
        }
        let deleted_client_pubkeys = self
            .state
            .nwc
            .connections
            .iter()
            .filter(|connection| connection.id == id)
            .map(|connection| connection.client_pubkey.clone())
            .collect::<Vec<_>>();
        let before = self.state.nwc.connections.len();
        self.state
            .nwc
            .connections
            .retain(|connection| connection.id != id);
        if self.state.nwc.connections.len() < before {
            self.nwc_in_flight_info_events.retain(|key| {
                !deleted_client_pubkeys
                    .iter()
                    .any(|client_pubkey| key.starts_with(&format!("{client_pubkey}|")))
            });
            self.state.toast = Some("NWC string deleted.".to_string());
            self.request_haptic(HapticFeedback::NotificationWarning);
            self.save_app_data();
            self.sync_nwc_push_registrations();
        }
    }

    pub(super) fn publish_pending_nwc_info_events(&mut self) {
        let Ok(keys) = self.nostr_keys() else {
            return;
        };
        let pending = self
            .state
            .nwc
            .connections
            .iter()
            .flat_map(|connection| {
                let client_pubkey = connection.client_pubkey.clone();
                let targeted = self
                    .secrets
                    .get_secret(nwc_client_secret_key(&client_pubkey))
                    .is_none();
                let permissions = connection.enabled_permissions();
                connection
                    .pending_info_event_relays
                    .iter()
                    .cloned()
                    .map(move |relay| (client_pubkey.clone(), relay, targeted, permissions.clone()))
            })
            .collect::<Vec<_>>();

        for (client_pubkey_hex, relay, targeted, permissions) in pending {
            let in_flight_key = nwc_info_event_key(&client_pubkey_hex, &relay);
            if !self.nwc_in_flight_info_events.insert(in_flight_key) {
                continue;
            }
            let client_pubkey = if targeted {
                match public_key_from_npub_or_hex(&client_pubkey_hex) {
                    Ok(client_pubkey) => Some(client_pubkey),
                    Err(_) => {
                        self.nwc_in_flight_info_events
                            .remove(&nwc_info_event_key(&client_pubkey_hex, &relay));
                        continue;
                    }
                }
            } else {
                None
            };
            let tx = self.tx.clone();
            let keys = keys.clone();
            self.rt.spawn(async move {
                let result = publish_nwc_info_event_with_retry(
                    relay.clone(),
                    keys,
                    client_pubkey,
                    permissions,
                )
                .await;
                let message = match result {
                    Ok(()) => AsyncMsg::NwcInfoEventPublished {
                        client_pubkey: client_pubkey_hex,
                        relay,
                    },
                    Err(error) => AsyncMsg::NwcInfoEventFailed {
                        client_pubkey: client_pubkey_hex,
                        relay,
                        error: format!("{error:#}"),
                    },
                };
                let _ = tx.send(CoreMsg::Async(message));
            });
        }
    }

    pub(super) fn refresh_nwc_connection_uris_for_lud16(&mut self) {
        self.state.refresh_derived();
    }

    pub(super) fn update_nwc_push_registration(
        &mut self,
        apns_device_token: Option<String>,
        registration_status: String,
        wake_server_url: Option<String>,
        app_id: String,
        environment: String,
        install_id: String,
    ) {
        let wake_enabled = registration_status != "Permission denied";
        self.state.push_notifications.apns_device_token = apns_device_token.clone();
        self.state.push_notifications.registration_status = registration_status;
        let config = NwcPushConfig::new(
            wake_server_url,
            apns_device_token,
            app_id,
            environment,
            install_id,
            wake_enabled,
        );
        if self.nwc_push_config != config {
            if let Some(manager) = self.nwc_manager.as_mut() {
                manager.mark_registration_refresh_pending();
            }
            self.nwc_push_config = config;
        }
        self.sync_nwc_push_registrations();
    }

    pub(super) fn enqueue_nwc_wake_requests(&mut self, requests: Vec<NwcWakeRequest>) {
        let mut added = 0usize;
        for request in requests {
            if !self.nwc_wake_request_is_known(&request.event_id_hex) {
                self.state.nwc.pending_wake_requests.push(request);
                added += 1;
            }
        }
        self.cap_pending_nwc_wake_requests();
        self.state.nwc.last_wake_status = match added {
            0 => "No new NWC wake requests".to_string(),
            1 => "Queued 1 NWC wake request".to_string(),
            count => format!("Queued {count} NWC wake requests"),
        };
        self.process_pending_nwc_wake_requests();
    }

    fn cap_pending_nwc_wake_requests(&mut self) {
        let len = self.state.nwc.pending_wake_requests.len();
        if len > MAX_NWC_WAKE_HISTORY {
            self.state
                .nwc
                .pending_wake_requests
                .drain(0..len - MAX_NWC_WAKE_HISTORY);
        }
    }

    fn cap_processed_nwc_wake_requests(&mut self) {
        let len = self.state.nwc.processed_wake_requests.len();
        if len > MAX_NWC_WAKE_HISTORY {
            self.state
                .nwc
                .processed_wake_requests
                .drain(0..len - MAX_NWC_WAKE_HISTORY);
        }
    }

    fn nwc_wake_request_is_known(&self, event_id: &str) -> bool {
        self.state
            .nwc
            .pending_wake_requests
            .iter()
            .any(|request| request.event_id_hex == event_id)
            || self
                .state
                .nwc
                .processed_wake_requests
                .iter()
                .any(|request| request.event_id_hex == event_id)
            || self
                .nwc_wake_coordinator
                .is_in_flight(&event_id.to_string())
    }

    pub(super) fn process_pending_nwc_wake_requests(&mut self) {
        if self.nwc_manager.is_none() {
            self.state.nwc.last_wake_status =
                "NWC wake queued: authorization storage is unavailable".to_string();
            return;
        }
        let Some(request) = self
            .state
            .nwc
            .pending_wake_requests
            .iter()
            .find(|request| {
                !self
                    .nwc_wake_coordinator
                    .is_in_flight(&request.event_id_hex)
            })
            .cloned()
        else {
            return;
        };

        let Some(wallet) = self.wallet.clone() else {
            self.state.nwc.last_wake_status = "NWC wake queued: wallet is not open yet".to_string();
            return;
        };
        if !self.ensure_wallet_derived_nostr_key() {
            self.state.nwc.last_wake_status =
                "NWC wake queued: Nostr key is not available".to_string();
            return;
        }

        self.nwc_wake_coordinator
            .begin(request.event_id_hex.clone());
        self.process_nwc_wake_request(request, wallet);
    }

    fn process_nwc_wake_request(&self, request: NwcWakeRequest, wallet: bark::Wallet) {
        let tx = self.tx.clone();
        let data_dir = self.data_dir.clone();
        let secrets = self.secrets.clone();
        let generation = self.wallet_generation;
        self.rt.spawn(async move {
            let event_id = request.event_id_hex.clone();
            let result = async {
                let wake = WakeEnvelope::new(
                    request.relay_url.clone(),
                    request.event_id_hex.clone(),
                    request.wallet_service_public_key_hex.clone(),
                    request.embedded_event_json.clone(),
                    request.received_at_seconds,
                )
                .validate()
                .context("invalid NWC wake envelope")?;
                let manager =
                    NwcApplicationManager::open(&data_dir).context("NWC ledger is unavailable")?;
                let relays = NostrRelayTransport;
                let secrets = RebelSecretProvider::new(secrets);
                let budget = OperationBudget::new(NWC_FOREGROUND_OPERATION_TIMEOUT)
                    .context("invalid NWC foreground budget")?;
                Ok::<_, anyhow::Error>(
                    execute_bark_wake(
                        manager.service().ledger(),
                        wallet,
                        &relays,
                        &secrets,
                        wake,
                        budget,
                        &NeverCancelled,
                    )
                    .await,
                )
            }
            .await;

            let msg = match result {
                Ok(disposition) => AsyncMsg::NwcWakeEngineFinished {
                    generation,
                    request,
                    disposition,
                },
                Err(e) => AsyncMsg::NwcWakeRequestFailed {
                    generation,
                    event_id,
                    error: format!("{e:#}"),
                },
            };
            let _ = tx.send(CoreMsg::Async(msg));
        });
    }

    pub(super) fn finish_nwc_wake_engine(
        &mut self,
        generation: u64,
        request: NwcWakeRequest,
        disposition: WakeDisposition,
    ) {
        let decision = self
            .nwc_wake_coordinator
            .handle_disposition(&request.event_id_hex, disposition);
        match decision {
            ForegroundWakeDecision::Finished(outcome) => match outcome {
                ForegroundWakeOutcome::Completed => {
                    self.finish_nwc_wake(request, "completed", true)
                }
                ForegroundWakeOutcome::AlreadyProcessed => {
                    self.finish_nwc_wake(request, "already_processed", false)
                }
                ForegroundWakeOutcome::Rejected(code) => {
                    self.finish_nwc_wake(request, &format!("rejected:{code:?}"), false)
                }
                ForegroundWakeOutcome::RetryExhausted => {
                    self.finish_nwc_wake(request, "retry_exhausted", false)
                }
                _ => self.finish_nwc_wake(request, "unsupported_disposition", false),
            },
            ForegroundWakeDecision::Retry { delay, cause } => {
                self.state.nwc.last_wake_status = match cause {
                    ForegroundWakeRetryCause::Engine(reason) => {
                        format!("NWC wake retry scheduled: {reason:?}")
                    }
                    ForegroundWakeRetryCause::QueuedForApplication(reason) => {
                        format!("NWC wake queued: {reason:?}")
                    }
                    _ => "NWC wake retry scheduled".to_string(),
                };
                self.schedule_nwc_wake_retry(generation, request.event_id_hex, delay);
                self.process_pending_nwc_wake_requests();
            }
            _ => self.finish_nwc_wake(request, "unsupported_disposition", false),
        }
    }

    fn finish_nwc_wake(&mut self, request: NwcWakeRequest, status: &str, success: bool) {
        self.nwc_wake_coordinator.forget(&request.event_id_hex);
        self.state
            .nwc
            .pending_wake_requests
            .retain(|pending| pending.event_id_hex != request.event_id_hex);
        self.state.nwc.last_wake_status = format!("NWC wake {status}.");
        self.state
            .nwc
            .processed_wake_requests
            .push(NwcProcessedWakeRequest {
                relay_url: request.relay_url,
                event_id_hex: request.event_id_hex,
                client_public_key_hex: String::new(),
                method: "request".to_string(),
                status: status.to_string(),
                amount_sat: 0,
                received_at_seconds: request.received_at_seconds,
                processed_at_seconds: crate::time::now_unix(),
            });
        self.cap_processed_nwc_wake_requests();
        self.refresh_nwc_connection_usage();
        if success {
            self.request_haptic(HapticFeedback::NotificationSuccess);
        } else if status.starts_with("rejected")
            || matches!(status, "unsupported_disposition" | "retry_exhausted")
        {
            self.request_haptic(HapticFeedback::NotificationWarning);
        }
        self.process_pending_nwc_wake_requests();
    }

    pub(super) fn fail_nwc_wake_request(&mut self, event_id: String, error: String) {
        self.nwc_wake_coordinator.forget(&event_id);
        self.state.nwc.last_wake_status = format!("NWC wake failed: {error}");
        self.state
            .nwc
            .pending_wake_requests
            .retain(|request| request.event_id_hex != event_id);
        self.request_haptic(HapticFeedback::NotificationWarning);
        self.process_pending_nwc_wake_requests();
    }

    pub(super) fn retry_nwc_wake_request(&mut self, event_id: String) {
        self.nwc_wake_coordinator.retry_due(&event_id);
        self.process_pending_nwc_wake_requests();
    }

    fn schedule_nwc_wake_retry(&self, generation: u64, event_id: String, delay: Duration) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(CoreMsg::Async(AsyncMsg::NwcWakeRetryDue {
                generation,
                event_id,
            }));
        });
    }

    pub(super) fn finish_nwc_info_event(
        &mut self,
        client_pubkey: String,
        relay: String,
        error: Option<String>,
    ) {
        self.nwc_in_flight_info_events
            .remove(&nwc_info_event_key(&client_pubkey, &relay));
        if let Some(error) = error {
            self.state.nwc.last_wake_status = format!("NWC info event failed on {relay}: {error}");
            return;
        }
        if let Some(connection) = self
            .state
            .nwc
            .connections
            .iter_mut()
            .find(|connection| connection.client_pubkey == client_pubkey)
        {
            if let Some(manager) = self.nwc_manager.as_ref() {
                let _ = manager.acknowledge_nwc_info_event(&connection.id, &relay);
            }
            connection
                .pending_info_event_relays
                .retain(|pending_relay| pending_relay != &relay);
        }
        self.state.nwc.last_wake_status = format!("NWC info event published to {relay}");
    }

    pub(super) fn handle_nwc_push_retry_due(&mut self, nonce: u64) {
        if self
            .nwc_manager
            .as_ref()
            .is_some_and(|manager| manager.registration_retry_is_current(nonce))
        {
            self.sync_nwc_push_registrations();
        }
    }

    pub(super) fn remove_nwc_wallet_data(&mut self) -> (NwcWalletCleanup, Vec<String>) {
        let cleanup = NwcWalletCleanup {
            secrets: self
                .state
                .nwc
                .connections
                .iter()
                .map(|connection| (connection.client_pubkey.clone(), connection.name.clone()))
                .collect(),
        };
        self.nwc_manager = None;
        let database_path = NwcApplicationManager::database_path(&self.data_dir);
        let errors = remove_wallet_database_files(&database_path)
            .err()
            .map(|error| vec![format!("{error:#}")])
            .unwrap_or_default();
        (cleanup, errors)
    }

    pub(super) fn reset_nwc_after_wallet_deletion(&mut self) -> Vec<String> {
        self.nwc_wake_coordinator.reset();
        self.nwc_manager = NwcApplicationManager::open(&self.data_dir).ok();
        if self.nwc_manager.is_none() {
            vec!["could not reopen NWC authorization storage".to_string()]
        } else {
            Vec::new()
        }
    }

    pub(super) fn delete_nwc_wallet_secrets(&self, cleanup: NwcWalletCleanup) -> Vec<String> {
        cleanup
            .secrets
            .into_iter()
            .filter_map(|(client_pubkey, name)| {
                (!self
                    .secrets
                    .delete_secret(nwc_client_secret_key(&client_pubkey)))
                .then(|| format!("NWC secret for {name}"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::core::tests::{recording_secret_core, test_core};
    use crate::core::{NOSTR_SECRET_KEY, WALLET_SEED_KEY};
    use crate::{NwcConnection, NwcPermission};

    use super::*;

    fn test_nwc_connection(client_pubkey: &str) -> NwcConnection {
        NwcConnection {
            id: format!("nwc-{client_pubkey}"),
            name: "Test".to_string(),
            icon_url: None,
            icon_display_url: None,
            relay: "wss://relay.example.com".to_string(),
            wallet_managed_secret: true,
            service_pubkey: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            client_pubkey: client_pubkey.to_string(),
            budget_sat: 0,
            spent_sat: 0,
            budget_display: String::new(),
            spent_display: String::new(),
            budget_interval: NwcBudgetInterval::Never,
            budget_interval_display: String::new(),
            permissions: vec![NwcPermission::GetInfo],
            created_at: 0,
            last_used_at: None,
            expires_at: None,
            budget_period_started_at: 0,
            pending_info_event_relays: Vec::new(),
        }
    }

    fn test_nwc_wake_request() -> NwcWakeRequest {
        NwcWakeRequest {
            relay_url: "wss://relay.example.com".to_string(),
            event_id_hex: "event".to_string(),
            wallet_service_public_key_hex: "wallet".to_string(),
            embedded_event_json: None,
            received_at_seconds: 100,
        }
    }

    #[test]
    fn relay_input_validation_matches_creation_policy() {
        assert!(nwc_relay_input_is_valid("wss://relay.example/path/"));
        assert!(nwc_relay_input_is_valid(
            "wss://relay.example/one\nwss://relay.example/two"
        ));
        assert!(!nwc_relay_input_is_valid(""));
        assert!(!nwc_relay_input_is_valid("ws://relay.example"));
        assert!(!nwc_relay_input_is_valid(
            "wss://relay.example/1,wss://relay.example/2,wss://relay.example/3"
        ));
    }

    #[test]
    fn pending_wake_processing_fails_closed_until_service_is_ready() {
        let (_data_dir, _cache_dir, mut core) = test_core();
        core.nwc_manager = None;
        core.state
            .nwc
            .pending_wake_requests
            .push(test_nwc_wake_request());

        core.process_pending_nwc_wake_requests();

        assert!(!core.nwc_wake_coordinator.is_in_flight(&"event".to_string()));
        assert!(core
            .state
            .nwc
            .last_wake_status
            .contains("authorization storage is unavailable"));
    }

    #[test]
    fn loads_connections_from_the_authoritative_nwc_mobile_ledger() {
        let (_data_dir, _cache_dir, mut core) = test_core();
        let service_keys = Keys::generate();
        let client_keys = Keys::generate();
        let mut connection = test_nwc_connection(&client_keys.public_key().to_hex());
        connection.service_pubkey = service_keys.public_key().to_hex();
        connection.name = "Stored display name".to_string();
        let authorization = nwc_mobile::HostConnectionAuthorization::new(
            connection.id.clone(),
            connection.client_pubkey.clone(),
            connection.service_pubkey.clone(),
            vec![connection.relay.clone()],
            vec![nwc_mobile::NwcMethod::GetInfo],
            connection.budget_sat,
            connection.budget_interval.into(),
            nwc_mobile::FeePolicy::CountTowardBudget {
                maximum_fee_sat: nwc_mobile::maximum_mobile_fee_sat(connection.budget_sat),
            },
            NWC_ENCRYPTION,
            None,
        );
        core.nwc_manager
            .as_ref()
            .expect("manager")
            .service()
            .create_host_connection(authorization)
            .expect("persist connection");
        core.nwc_manager
            .as_ref()
            .expect("manager")
            .set_connection_metadata(
                &connection.id,
                ApplicationConnectionMetadata::new(
                    connection.name.clone(),
                    None,
                    vec![connection.relay.clone()],
                )
                .expect("metadata"),
            )
            .expect("persist metadata");

        core.load_nwc_connections();

        assert_eq!(core.state.nwc.connections.len(), 1);
        let loaded = &core.state.nwc.connections[0];
        assert_eq!(loaded.name, "Stored display name");
        assert_eq!(loaded.client_pubkey, client_keys.public_key().to_hex());
        assert_eq!(loaded.service_pubkey, service_keys.public_key().to_hex());
        assert!(!loaded.wallet_managed_secret);
    }

    #[test]
    fn completed_engine_wake_leaves_the_queue_and_enters_history() {
        let (_data_dir, _cache_dir, mut core) = test_core();
        let request = test_nwc_wake_request();
        core.state.nwc.pending_wake_requests.push(request.clone());
        core.nwc_wake_coordinator
            .begin(request.event_id_hex.clone());

        core.finish_nwc_wake_engine(
            core.wallet_generation,
            request,
            WakeDisposition::Completed {
                notification: nwc_mobile::NotificationHint::Completed,
            },
        );

        assert!(core.state.nwc.pending_wake_requests.is_empty());
        assert!(!core.nwc_wake_coordinator.is_in_flight(&"event".to_string()));
        assert_eq!(core.state.nwc.processed_wake_requests.len(), 1);
        assert_eq!(
            core.state.nwc.processed_wake_requests[0].status,
            "completed"
        );
    }

    #[test]
    fn retryable_engine_wake_remains_owned_and_queued() {
        let (_data_dir, _cache_dir, mut core) = test_core();
        let request = test_nwc_wake_request();
        core.state.nwc.pending_wake_requests.push(request.clone());
        core.nwc_wake_coordinator
            .begin(request.event_id_hex.clone());

        core.finish_nwc_wake_engine(
            core.wallet_generation,
            request,
            WakeDisposition::RetryAfter {
                delay: Duration::from_secs(60),
                reason: nwc_mobile::RetryReason::RelayUnavailable,
                notification: nwc_mobile::NotificationHint::Processing,
            },
        );

        assert_eq!(core.state.nwc.pending_wake_requests.len(), 1);
        assert!(core.nwc_wake_coordinator.is_in_flight(&"event".to_string()));
        assert_eq!(
            core.nwc_wake_coordinator
                .retry_attempts(&"event".to_string()),
            1
        );
        assert!(core.state.nwc.processed_wake_requests.is_empty());
    }

    #[test]
    fn exhausted_wake_retries_leave_the_queue_and_enter_history() {
        let (_data_dir, _cache_dir, mut core) = test_core();
        let request = test_nwc_wake_request();
        core.state.nwc.pending_wake_requests.push(request.clone());
        core.nwc_wake_coordinator
            .begin(request.event_id_hex.clone());
        for _ in 0..nwc_mobile::DEFAULT_FOREGROUND_WAKE_RETRY_ATTEMPTS {
            let _ = core.nwc_wake_coordinator.handle_disposition(
                &request.event_id_hex,
                WakeDisposition::RetryAfter {
                    delay: Duration::from_secs(1),
                    reason: nwc_mobile::RetryReason::WalletUnavailable,
                    notification: nwc_mobile::NotificationHint::Processing,
                },
            );
        }

        core.finish_nwc_wake_engine(
            core.wallet_generation,
            request,
            WakeDisposition::QueuedForApplication {
                reason: nwc_mobile::QueueReason::WalletUnavailable,
                notification: nwc_mobile::NotificationHint::OpenApplication,
            },
        );

        assert!(core.state.nwc.pending_wake_requests.is_empty());
        assert!(!core.nwc_wake_coordinator.is_in_flight(&"event".to_string()));
        assert_eq!(
            core.nwc_wake_coordinator
                .retry_attempts(&"event".to_string()),
            0
        );
        assert_eq!(core.state.nwc.processed_wake_requests.len(), 1);
        assert_eq!(
            core.state.nwc.processed_wake_requests[0].status,
            "retry_exhausted"
        );
        assert!(core
            .pending_haptics
            .contains(&HapticFeedback::NotificationWarning));
    }

    #[cfg(unix)]
    #[test]
    fn wallet_deletion_removes_secrets_when_empty_ledger_cannot_reopen() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (store, mut core) = recording_secret_core(data_dir.path());
        core.nwc_manager = None;
        remove_wallet_database_files(&NwcApplicationManager::database_path(data_dir.path()))
            .expect("remove initial ledger");
        std::fs::set_permissions(data_dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("make data directory read-only");

        core.delete_wallet();

        std::fs::set_permissions(data_dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore data directory permissions");
        assert_eq!(
            store.deleted_keys(),
            vec![WALLET_SEED_KEY.to_string(), NOSTR_SECRET_KEY.to_string()]
        );
        let toast = core.state.toast.as_deref().expect("cleanup warning toast");
        assert!(toast.contains("could not reopen NWC authorization storage"));
    }
}
