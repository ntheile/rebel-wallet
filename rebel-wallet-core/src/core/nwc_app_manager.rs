use super::*;
use crate::NwaRequestState;
use nwc_mobile::{
    ApplicationRegistrationCompletion, ApplicationRegistrationPass, NwaCallbackBegin,
    NwaCallbackCompletion,
};

impl AppCore {
    pub(super) fn open_nwa_request(&mut self, uri: String) {
        let Some(manager) = self.nwc_manager.as_ref() else {
            self.set_nwa_error("NWC authorization storage is unavailable.");
            return;
        };
        match manager.open_nwa_request(&uri) {
            Ok(request) => {
                let state = match NwaRequestState::try_from(request)
                    .context("shared NWA presentation is not representable by the native contract")
                {
                    Ok(state) => state,
                    Err(error) => {
                        self.state.toast =
                            Some(format!("Nostr Wallet Auth request rejected: {error:#}"));
                        self.request_haptic(HapticFeedback::NotificationError);
                        return;
                    }
                };
                let icon_url = state.icon_url.clone();
                self.state.nwa.request = Some(state);
                self.state.nwa.approving = false;
                self.state.nwa.error_message = None;
                self.state.nwa.callback_pending = false;
                self.state.nwa.icon_display_url = None;
                if let Some(icon_url) = icon_url {
                    self.state.nwa.icon_display_url = self.nwc_icon_display_url(Some(&icon_url));
                    self.prefetch_nwc_icon(icon_url);
                }
                if self.state.setup == SetupState::Ready
                    && self.state.router.screen_stack.last() != Some(&Screen::Nwc)
                {
                    self.state.router.screen_stack.push(Screen::Nwc);
                }
            }
            Err(MobileServiceError::NwaAlreadyPending) => {
                self.state.toast = Some(
                    "Finish or cancel the current Nostr Wallet Auth request before opening another."
                        .to_string(),
                );
                self.request_haptic(HapticFeedback::NotificationWarning);
            }
            Err(error) => {
                self.state.toast = Some(format!("Nostr Wallet Auth request rejected: {error:#}"));
                self.request_haptic(HapticFeedback::NotificationError);
            }
        }
    }

    pub(super) fn approve_nwa_request(
        &mut self,
        relay: String,
        budget_sat: u64,
        budget_interval: NwcBudgetInterval,
        permissions: Vec<NwcPermission>,
    ) {
        if self.state.nwa.approving {
            return;
        }
        let Some(request) = self.state.nwa.request.clone() else {
            self.set_nwa_error("The Nostr Wallet Auth request is no longer available.");
            return;
        };
        if self.nwc_manager.is_none() {
            self.set_nwa_error("NWC authorization storage is unavailable.");
            return;
        }
        if !self.ensure_wallet_derived_nostr_key() {
            self.set_nwa_error("Create or open the wallet before adding NWC.");
            return;
        }
        let service_keys = match self.nostr_keys() {
            Ok(keys) => keys,
            Err(error) => {
                self.set_nwa_error(&format!("{error:#}"));
                return;
            }
        };
        self.state.nwa.approving = true;
        self.state.nwa.error_message = None;
        let selection = nwc_mobile::NwaApprovalSelection::new(
            request.request_id_hex.clone(),
            service_keys.public_key().to_hex(),
            relay,
            self.state.nwc.default_relay.clone(),
            permissions.into_iter().map(Into::into).collect(),
            budget_sat,
            budget_interval.into(),
            NWC_ENCRYPTION,
            request.expires_at.map(UnixTimestamp::from_secs),
            self.state.lightning_address.address.clone(),
        );
        let approved = self
            .nwc_manager
            .as_mut()
            .expect("checked above")
            .approve_nwa(selection);
        let (approved, callback) = match approved {
            Ok(approved) => approved.into_parts(),
            Err(nwc_mobile::ApplicationWorkflowError::Service(
                MobileServiceError::NwaApproval(NwaApprovalError::AuthorityEscalation),
            )) => {
                self.set_nwa_error("The approval exceeds the requested authority.");
                return;
            }
            Err(error) => {
                self.set_nwa_error(&format!("Could not approve NWC authorization: {error:#}"));
                return;
            }
        };
        self.finish_nwa_approval(request, approved, callback);
    }

    fn finish_nwa_approval(
        &mut self,
        request: NwaRequestState,
        approved: nwc_mobile::ApprovedApplicationConnection,
        callback: NwaCallbackBegin,
    ) {
        let icon_display_url = self.nwc_icon_display_url(request.icon_url.as_deref());
        let metadata = ApplicationConnectionMetadata::new(
            request.display_name.clone(),
            request.icon_url.clone(),
            approved.draft().authorization().relay_urls().to_vec(),
        );
        let stored = metadata.and_then(|metadata| {
            self.nwc_manager
                .as_ref()
                .expect("manager exists during approval")
                .set_connection_metadata(approved.draft().id(), metadata)
                .map_err(|_| nwc_mobile::RegistryError::InvalidConnection)
        });
        if stored.is_err() {
            let _ = self
                .nwc_manager
                .as_ref()
                .expect("manager exists during approval")
                .service()
                .revoke_host_connection(approved.draft().id());
            self.set_nwa_error("Could not persist NWC connection metadata.");
            return;
        }
        let connection = NwcConnection::from_approved(
            &approved,
            MobileConnectionMetadata {
                name: request.display_name,
                icon_url: request.icon_url,
                icon_display_url,
                wallet_managed_secret: false,
            },
        )
        .expect("shared NWA approval must produce a native connection view");
        self.state.nwc.default_relay = connection.relay.clone();
        self.state.nwc.connections.push(connection);
        self.state.nwa.approving = false;
        self.state.nwa.error_message = None;
        self.publish_pending_nwc_info_events();
        self.sync_nwc_push_registrations();
        self.request_haptic(HapticFeedback::NotificationSuccess);

        match callback {
            NwaCallbackBegin::OpenUrl(url) => {
                self.state.nwa.callback_pending = true;
                self.pending_side_effects.push(AppUpdate::OpenUrl {
                    rev: self.rev + 1,
                    url,
                });
            }
            NwaCallbackBegin::Complete => {
                self.state.toast = Some("NWC client authorized.".to_string());
                self.clear_nwa_request();
            }
            _ => self.set_nwa_error("Unsupported NWA callback action."),
        }
    }

    pub(super) fn retry_nwa_callback(&mut self) {
        let Some(url) = self
            .nwc_manager
            .as_ref()
            .and_then(NwcApplicationManager::retry_nwa_callback)
        else {
            self.set_nwa_error("There is no callback to retry.");
            return;
        };
        self.state.nwa.error_message = None;
        self.pending_side_effects.push(AppUpdate::OpenUrl {
            rev: self.rev + 1,
            url,
        });
    }

    pub(super) fn complete_nwa_callback_open(&mut self, opened: bool) {
        let Some(manager) = self.nwc_manager.as_mut() else {
            return;
        };
        match manager.complete_nwa_callback(opened) {
            NwaCallbackCompletion::Ignored => {}
            NwaCallbackCompletion::Complete => self.clear_nwa_request(),
            NwaCallbackCompletion::RetryAvailable => {
                self.state.nwa.callback_pending = true;
                self.set_nwa_error(
                    "The connection was approved, but the requesting app could not be reopened. Return to it manually or retry.",
                );
            }
            _ => {}
        }
    }

    pub(super) fn cancel_nwa_request(&mut self) {
        if !self.state.nwa.approving {
            self.clear_nwa_request();
        }
    }

    pub(super) fn set_nwa_error(&mut self, message: &str) {
        self.state.nwa.approving = false;
        self.state.nwa.error_message = Some(message.to_string());
        self.request_haptic(HapticFeedback::NotificationError);
    }

    fn clear_nwa_request(&mut self) {
        if let Some(manager) = self.nwc_manager.as_mut() {
            let _ = manager.cancel_nwa();
        }
        self.state.nwa = crate::NwaState::default();
    }

    pub(super) fn request_nwc_connection_export(&mut self, id: String, copy_to_clipboard: bool) {
        let Some(connection) = self
            .state
            .nwc
            .connections
            .iter()
            .find(|connection| connection.id == id)
        else {
            self.state.toast = Some("NWC connection was not found.".to_string());
            return;
        };
        let provider = RebelSecretProvider::new(self.secrets.clone());
        let uri = match self
            .nwc_manager
            .as_ref()
            .expect("connection state requires the shared manager")
            .export_connection_uri(
                &connection.id,
                self.state.lightning_address.address.clone(),
                &provider,
            ) {
            Ok(uri) => uri,
            Err(_) => {
                self.state.toast = Some(
                    "This client-created NWC connection cannot be exported by the wallet."
                        .to_string(),
                );
                self.request_haptic(HapticFeedback::NotificationWarning);
                return;
            }
        };
        self.pending_side_effects
            .push(AppUpdate::NwcConnectionExportReady {
                rev: self.rev + 1,
                connection_id: connection.id.clone(),
                name: connection.name.clone(),
                uri,
                copy_to_clipboard,
                present_qr: !copy_to_clipboard,
            });
    }

    pub(super) fn sync_nwc_push_registrations(&mut self) {
        let Ok(config) = self.nwc_push_config.ready() else {
            return;
        };
        let Ok(keys) = self.nostr_keys() else {
            return;
        };
        let Some(manager) = self.nwc_manager.as_mut() else {
            return;
        };
        match manager.begin_registration(config.enabled()) {
            Ok(RegistrationStart::Busy) => return,
            Ok(RegistrationStart::Ready) => {}
            Ok(_) => return,
            Err(_) => {
                self.state.nwc.last_wake_status =
                    "NWC wake registration storage is unavailable".to_string();
                return;
            }
        }
        let data_dir = self.data_dir.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = match NwcApplicationManager::open(&data_dir) {
                Ok(manager) => {
                    run_registration_worker(manager.service().ledger(), config, keys).await
                }
                Err(error) => Err(error.into()),
            };
            let message = match result {
                Ok(pass) => AsyncMsg::NwcPushRegistrationFinished {
                    applied: pass.applied(),
                    deferred: pass.deferred(),
                    next_attempt_at: pass.next_attempt_at(),
                    error: None,
                },
                Err(_) => AsyncMsg::NwcPushRegistrationFinished {
                    applied: 0,
                    deferred: 0,
                    next_attempt_at: None,
                    error: Some("durable registration pass failed".to_string()),
                },
            };
            let _ = tx.send(CoreMsg::Async(message));
        });
    }

    pub(super) fn finish_nwc_push_registration(
        &mut self,
        applied: usize,
        deferred: usize,
        next_attempt_at: Option<u64>,
        error: Option<String>,
    ) {
        let pass = if error.is_some() {
            ApplicationRegistrationPass::failed()
        } else {
            ApplicationRegistrationPass::completed(applied, deferred, next_attempt_at)
        };
        let Some(manager) = self.nwc_manager.as_mut() else {
            return;
        };
        match manager.finish_registration(pass, UnixTimestamp::from_secs(now_unix())) {
            ApplicationRegistrationCompletion::Ignored => {}
            ApplicationRegistrationCompletion::RunAgain => self.sync_nwc_push_registrations(),
            ApplicationRegistrationCompletion::Failed { retry_at } => {
                let error = error.unwrap_or_else(|| "durable registration pass failed".to_string());
                self.state.nwc.last_wake_status = format!("NWC wake registration failed: {error}");
                self.schedule_nwc_push_retry(retry_at);
            }
            ApplicationRegistrationCompletion::Deferred { retry_at } => {
                self.state.nwc.last_wake_status =
                    "NWC wake registration queued for retry".to_string();
                if let Some(retry_at) = retry_at {
                    self.schedule_nwc_push_retry(retry_at);
                }
            }
            ApplicationRegistrationCompletion::Applied { applied, retry_at } => {
                self.state.nwc.last_wake_status = format!(
                    "Applied {applied} NWC wake registration{}",
                    if applied == 1 { "" } else { "s" }
                );
                if let Some(retry_at) = retry_at {
                    self.schedule_nwc_push_retry(retry_at);
                }
            }
            ApplicationRegistrationCompletion::Idle {
                retry_at: Some(retry_at),
            } => self.schedule_nwc_push_retry(retry_at),
            ApplicationRegistrationCompletion::Idle { retry_at: None } => {}
            _ => {}
        }
    }

    fn schedule_nwc_push_retry(&mut self, next_attempt_at: u64) {
        let Some(manager) = self.nwc_manager.as_mut() else {
            return;
        };
        let nonce = manager.schedule_registration_retry();
        let delay = registration_retry_delay(next_attempt_at, UnixTimestamp::from_secs(now_unix()));
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(CoreMsg::Async(AsyncMsg::NwcPushRetryDue { nonce }));
        });
    }
}
