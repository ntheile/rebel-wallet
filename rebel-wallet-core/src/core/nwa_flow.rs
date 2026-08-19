use super::*;

impl AppCore {
    pub(super) fn open_nwa_request(&mut self, uri: String) {
        match NwaRequest::parse(&uri, now_unix()) {
            Ok(request) => {
                let icon_url = request.state.icon_url.clone();
                self.state.nwa.request = Some(request.state.clone());
                self.state.nwa.approving = false;
                self.state.nwa.error_message = None;
                self.state.nwa.callback_pending = false;
                self.pending_nwa_request = Some(request);
                self.pending_nwa_callback = None;
                if let Some(icon_url) = icon_url {
                    let icon_display_url = self.nwc_icon_display_url(Some(&icon_url));
                    if let Some(request) = self.state.nwa.request.as_mut() {
                        request.icon_display_url = icon_display_url;
                    }
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
        let Some(request) = self.pending_nwa_request.clone() else {
            self.set_nwa_error("The Nostr Wallet Auth request is no longer available.");
            return;
        };
        let connection = match self.build_authorized_nwc_connection(
            request.state.display_name.clone(),
            request.state.icon_url.clone(),
            relay,
            request.state.client_pubkey.clone(),
            budget_sat,
            budget_interval,
            permissions,
            request.state.expires_at,
        ) {
            Ok(connection) => connection,
            Err(error) => {
                self.set_nwa_error(&format!("{error:#}"));
                return;
            }
        };
        let config = match self.nwc_push_config.ready() {
            Ok(config) => config,
            Err(error) => {
                self.set_nwa_error(&format!("Cannot enable background NWC wake: {error:#}"));
                return;
            }
        };
        let keys = match self.nostr_keys() {
            Ok(keys) => keys,
            Err(error) => {
                self.set_nwa_error(&format!("{error:#}"));
                return;
            }
        };
        let relays = parse_nwc_relay_urls(&connection.relay, "")
            .unwrap_or_default()
            .into_iter()
            .map(|relay| relay.to_string())
            .collect::<Vec<_>>();
        let lud16 = self.state.lightning_address.address.as_deref();
        let callback_url =
            match request.approved_callback(&connection.service_pubkey, &relays, lud16) {
                Ok(callback_url) => callback_url,
                Err(error) => {
                    self.set_nwa_error(&format!("Could not build the app callback: {error:#}"));
                    return;
                }
            };

        self.state.nwa.approving = true;
        self.state.nwa.error_message = None;
        let approval_client_pubkey = connection.client_pubkey.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let message = match register_connections(
                config.clone(),
                keys.clone(),
                vec![connection.clone()],
                true,
            )
            .await
            {
                Ok(()) => AsyncMsg::NwaApprovalSucceeded {
                    connection,
                    callback_url,
                },
                Err(error) => {
                    let _ = register_connections(config, keys, vec![connection], false).await;
                    AsyncMsg::NwaApprovalFailed {
                        client_pubkey: approval_client_pubkey,
                        error: format!("Could not register background NWC wake: {error:#}"),
                    }
                }
            };
            let _ = tx.send(CoreMsg::Async(message));
        });
    }

    pub(super) fn finish_nwa_approval_failure(&mut self, client_pubkey: String, error: String) {
        let failure_matches_pending_request = self
            .pending_nwa_request
            .as_ref()
            .and_then(|request| public_key_from_npub_or_hex(&request.state.client_pubkey).ok())
            .is_some_and(|pubkey| pubkey.to_hex() == client_pubkey);
        if failure_matches_pending_request {
            self.set_nwa_error(&error);
        }
    }

    pub(super) fn finish_nwa_approval(
        &mut self,
        connection: NwcConnection,
        callback_url: Option<String>,
    ) {
        let approval_matches_pending_request = self
            .pending_nwa_request
            .as_ref()
            .and_then(|request| {
                public_key_from_npub_or_hex(&request.state.client_pubkey)
                    .ok()
                    .map(|pubkey| pubkey.to_hex() == connection.client_pubkey)
            })
            .unwrap_or(false);
        if !approval_matches_pending_request {
            self.unregister_nwc_push_connections(vec![connection]);
            return;
        }
        self.state.nwc.default_relay = connection.relay.clone();
        self.state.nwc.connections.push(connection);
        self.state.nwa.approving = false;
        self.state.nwa.error_message = None;
        self.save_app_data();
        self.publish_pending_nwc_info_events();
        self.nwc_registered_fingerprint = None;
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
        if let Some(request) = self.pending_nwa_request.as_ref() {
            if let Ok(Some(url)) = request.cancelled_callback() {
                self.pending_side_effects.push(AppUpdate::OpenUrl {
                    rev: self.rev + 1,
                    url,
                });
            }
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
        let uri = match build_nwc_connection_uri(
            self.secrets.as_ref(),
            self.state.lightning_address.address.clone(),
            connection,
        ) {
            Some(uri) => uri,
            None => {
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
        if self.state.nwc.connections.is_empty() {
            return;
        }
        let Some(fingerprint) = self
            .nwc_push_config
            .fingerprint(&self.state.nwc.connections)
        else {
            return;
        };
        if self.nwc_registered_fingerprint.as_deref() == Some(&fingerprint)
            || self.nwc_registration_in_flight.as_deref() == Some(&fingerprint)
        {
            return;
        }
        let Ok(config) = self.nwc_push_config.ready() else {
            return;
        };
        let Ok(keys) = self.nostr_keys() else {
            return;
        };
        self.nwc_registration_in_flight = Some(fingerprint.clone());
        let connections = self.state.nwc.connections.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let error = register_connections(config, keys, connections, true)
                .await
                .err()
                .map(|error| format!("{error:#}"));
            let _ = tx.send(CoreMsg::Async(AsyncMsg::NwcPushRegistrationFinished {
                fingerprint,
                error,
            }));
        });
    }

    pub(super) fn finish_nwc_push_registration(
        &mut self,
        fingerprint: String,
        error: Option<String>,
    ) {
        if self.nwc_registration_in_flight.as_deref() == Some(&fingerprint) {
            self.nwc_registration_in_flight = None;
        }
        if let Some(error) = error {
            self.state.nwc.last_wake_status = format!("NWC wake registration failed: {error}");
        } else {
            self.nwc_registered_fingerprint = Some(fingerprint);
            self.state.nwc.last_wake_status = format!(
                "Registered {} NWC wake connection{}",
                self.state.nwc.connections.len(),
                if self.state.nwc.connections.len() == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
    }

    pub(super) fn unregister_nwc_push_connections(&mut self, connections: Vec<NwcConnection>) {
        if connections.is_empty() {
            return;
        }
        let Ok(config) = self.nwc_push_config.ready() else {
            return;
        };
        let Ok(keys) = self.nostr_keys() else {
            return;
        };
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let error = register_connections(config, keys, connections, false)
                .await
                .err()
                .map(|error| format!("{error:#}"));
            let _ = tx.send(CoreMsg::Async(AsyncMsg::NwcPushUnregistrationFinished {
                error,
            }));
        });
    }

    pub(super) fn finish_nwc_push_unregistration(&mut self, error: Option<String>) {
        if let Some(error) = error {
            self.state.nwc.last_wake_status = format!("NWC wake removal failed: {error}");
        }
    }

    pub(super) fn set_nwa_error(&mut self, message: &str) {
        self.state.nwa.approving = false;
        self.state.nwa.error_message = Some(message.to_string());
        self.request_haptic(HapticFeedback::NotificationError);
    }

    fn clear_nwa_request(&mut self) {
        self.pending_nwa_request = None;
        self.pending_nwa_callback = None;
        self.state.nwa = crate::NwaState::default();
    }
}
