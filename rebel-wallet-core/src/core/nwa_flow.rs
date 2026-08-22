use super::*;
use crate::NwaRequestState;

impl AppCore {
    pub(super) fn open_nwa_request(&mut self, uri: String) {
        let Some(service) = self.nwc_service.as_ref() else {
            self.set_nwa_error("NWC authorization storage is unavailable.");
            return;
        };
        if service.pending_nwa_request().ok().flatten().is_some() {
            self.state.toast = Some(
                "Finish or cancel the current Nostr Wallet Auth request before opening another."
                    .to_string(),
            );
            self.request_haptic(HapticFeedback::NotificationWarning);
            return;
        }
        match service.open_nwa_request(&uri) {
            Ok(request) => {
                let state = match nwa_request_state(request) {
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
                self.pending_nwa_callback = None;
                if let Some(icon_url) = icon_url {
                    let icon_display_url = self.nwc_icon_display_url(Some(&icon_url));
                    self.state.nwa.icon_display_url = icon_display_url;
                    self.prefetch_nwc_icon(icon_url);
                }
                if self.state.setup == SetupState::Ready
                    && self.state.router.screen_stack.last() != Some(&Screen::Nwc)
                {
                    self.state.router.screen_stack.push(Screen::Nwc);
                }
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
        if !self.nwc_service_ready {
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
        let approved = self
            .nwc_service
            .as_ref()
            .expect("checked above")
            .approve_application_nwa(nwc_mobile::NwaApprovalSelection::new(
                request.request_id_hex.clone(),
                service_keys.public_key().to_hex(),
                relay,
                self.state.nwc.default_relay.clone(),
                permissions.into_iter().map(Into::into).collect(),
                budget_sat,
                budget_interval.into(),
                NWC_ENCRYPTION,
                request.expires_at.map(nwc_mobile::UnixTimestamp::from_secs),
                self.state.lightning_address.address.clone(),
            ));
        let approved = match approved {
            Ok(approved) => approved,
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
        self.finish_nwa_approval(request, approved);
    }

    fn finish_nwa_approval(
        &mut self,
        request: NwaRequestState,
        approved: nwc_mobile::ApprovedApplicationConnection,
    ) {
        let approval = approved.approval();
        let icon_display_url = self.nwc_icon_display_url(request.icon_url.as_deref());
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
        let callback_url = approval.callback_url().map(str::to_owned);
        self.state.nwc.default_relay = connection.relay.clone();
        self.state.nwc.connections.push(connection);
        self.state.nwa.approving = false;
        self.state.nwa.error_message = None;
        self.save_app_data();
        self.publish_pending_nwc_info_events();
        self.sync_nwc_push_registrations();
        self.request_haptic(HapticFeedback::NotificationSuccess);

        if let Some(url) = callback_url {
            self.pending_nwa_callback = Some(url.clone());
            self.state.nwa.callback_pending = true;
            self.pending_side_effects.push(AppUpdate::OpenUrl {
                rev: self.rev + 1,
                url,
            });
        } else {
            self.state.toast = Some("NWC client authorized.".to_string());
            self.clear_nwa_request();
        }
    }

    pub(super) fn retry_nwa_callback(&mut self) {
        let Some(url) = self.pending_nwa_callback.clone() else {
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
        if self.pending_nwa_callback.is_none() {
            return;
        }
        if opened {
            self.clear_nwa_request();
        } else {
            self.state.nwa.callback_pending = true;
            self.set_nwa_error(
                "The connection was approved, but the requesting app could not be reopened. Return to it manually or retry.",
            );
        }
    }

    pub(super) fn cancel_nwa_request(&mut self) {
        if self.state.nwa.approving {
            return;
        }
        self.clear_nwa_request();
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
            .nwc_service
            .as_ref()
            .expect("connection state requires the shared service")
            .export_wallet_connection_uri(
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
        if self.nwc_registration_in_flight {
            return;
        }
        let Ok(config) = self.nwc_push_config.ready() else {
            return;
        };
        let Ok(keys) = self.nostr_keys() else {
            return;
        };
        if self.nwc_registration_refresh_pending {
            let refresh_result = self
                .nwc_service
                .as_ref()
                .context("NWC authorization storage is unavailable")
                .and_then(|service| {
                    service
                        .refresh_wake_registrations(config.enabled())
                        .context("could not refresh NWC wake registrations")
                });
            if refresh_result.is_err() {
                self.state.nwc.last_wake_status =
                    "NWC wake registration storage is unavailable".to_string();
                return;
            }
            self.nwc_registration_refresh_pending = false;
        }

        self.nwc_registration_retry_nonce = self.nwc_registration_retry_nonce.wrapping_add(1);
        self.nwc_registration_in_flight = true;
        let data_dir = self.data_dir.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = match open_nwc_service(&data_dir) {
                Ok(service) => run_registration_worker(service.ledger(), config, keys).await,
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
        self.nwc_registration_in_flight = false;
        if self.nwc_registration_refresh_pending {
            self.sync_nwc_push_registrations();
            return;
        }
        if let Some(error) = error {
            self.state.nwc.last_wake_status = format!("NWC wake registration failed: {error}");
            self.schedule_nwc_push_retry(now_unix().saturating_add(30));
        } else if deferred > 0 {
            self.state.nwc.last_wake_status = "NWC wake registration queued for retry".to_string();
            if let Some(next_attempt_at) = next_attempt_at {
                self.schedule_nwc_push_retry(next_attempt_at);
            }
        } else if applied > 0 {
            self.state.nwc.last_wake_status = format!(
                "Applied {applied} NWC wake registration{}",
                if applied == 1 { "" } else { "s" }
            );
            if let Some(next_attempt_at) = next_attempt_at {
                self.schedule_nwc_push_retry(next_attempt_at);
            }
        } else if let Some(next_attempt_at) = next_attempt_at {
            self.schedule_nwc_push_retry(next_attempt_at);
        }
    }

    fn schedule_nwc_push_retry(&mut self, next_attempt_at: u64) {
        self.nwc_registration_retry_nonce = self.nwc_registration_retry_nonce.wrapping_add(1);
        let nonce = self.nwc_registration_retry_nonce;
        let delay = nwc_push_retry_delay(next_attempt_at, now_unix());
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(CoreMsg::Async(AsyncMsg::NwcPushRetryDue { nonce }));
        });
    }

    pub(super) fn set_nwa_error(&mut self, message: &str) {
        self.state.nwa.approving = false;
        self.state.nwa.error_message = Some(message.to_string());
        self.request_haptic(HapticFeedback::NotificationError);
    }

    fn clear_nwa_request(&mut self) {
        if let Some(service) = self.nwc_service.as_ref() {
            let _ = service.clear_pending_nwa();
        }
        self.pending_nwa_callback = None;
        self.state.nwa = crate::NwaState::default();
    }
}
