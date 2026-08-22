use super::*;
use crate::NwaRequestState;
use nwc_mobile::{NwaCallbackBegin, NwaCallbackCompletion};

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
                self.nwa_callback.clear();
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
        let callback = self
            .nwa_callback
            .begin(approval.callback_url().map(str::to_owned));
        self.state.nwc.default_relay = connection.relay.clone();
        self.state.nwc.connections.push(connection);
        self.state.nwa.approving = false;
        self.state.nwa.error_message = None;
        self.save_app_data();
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
            _ => {}
        }
    }

    pub(super) fn retry_nwa_callback(&mut self) {
        let Some(url) = self.nwa_callback.retry_url() else {
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
        match self.nwa_callback.complete_open(opened) {
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
        if self.state.nwa.approving {
            return;
        }
        self.clear_nwa_request();
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
        self.nwa_callback.clear();
        self.state.nwa = crate::NwaState::default();
    }
}
