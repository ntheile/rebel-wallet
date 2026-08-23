use super::*;
use crate::nwc::{run_registration_worker, RebelSecretProvider, NWC_ENCRYPTION};
use crate::profile_cache::normalize_profile_picture_to_jpeg;
use crate::{NwaRequestState, NwcBudgetInterval, NwcConnection, NwcPermission};
use nwc_mobile::{
    registration_retry_delay, ApplicationConnectionMetadata, ApplicationIconUrl,
    ApplicationRegistrationCompletion, ApplicationRegistrationPass, MobileServiceError,
    NwaApprovalError, NwaCallbackBegin, NwaCallbackCompletion, RegistrationStart, UnixTimestamp,
};
use nwc_mobile_uniffi::MobileConnectionMetadata;

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

    pub(super) fn hydrate_nwc_icon_urls(&mut self) {
        let icon_cache = &self.nwc_icon_cache;
        for connection in &mut self.state.nwc.connections {
            connection.icon_display_url = connection
                .icon_url
                .as_deref()
                .and_then(|url| cached_nwc_icon_url(icon_cache, url));
        }
        if let Some(request) = self.state.nwa.request.as_ref() {
            self.state.nwa.icon_display_url = request
                .icon_url
                .as_deref()
                .and_then(|url| cached_nwc_icon_url(icon_cache, url));
        }
    }

    pub(super) fn prefetch_nwc_icons(&mut self) {
        let mut urls = self
            .state
            .nwc
            .connections
            .iter()
            .filter_map(|connection| connection.icon_url.clone())
            .collect::<Vec<_>>();
        if let Some(url) = self
            .state
            .nwa
            .request
            .as_ref()
            .and_then(|request| request.icon_url.clone())
        {
            urls.push(url);
        }
        urls.sort();
        urls.dedup();
        for url in urls {
            self.prefetch_nwc_icon(url);
        }
    }

    pub(super) fn prefetch_nwc_icon(&mut self, remote_url: String) {
        let Ok(icon_url) = ApplicationIconUrl::parse(&remote_url) else {
            return;
        };
        if self
            .nwc_icon_cache
            .cached_file_url(&icon_url)
            .ok()
            .flatten()
            .is_some()
            || !self.nwc_icon_downloads.insert(remote_url.clone())
        {
            self.refresh_nwc_icon_url(&remote_url);
            return;
        }

        let tx = self.tx.clone();
        let icon_cache = self.nwc_icon_cache.clone();
        let semaphore = self.profile_picture_download_semaphore.clone();
        self.rt.spawn(async move {
            let failed_url = remote_url.clone();
            let result = async {
                let _permit = semaphore.acquire().await?;
                let bytes = nwc_mobile_http::download_application_icon(&icon_url).await?;
                let normalized = normalize_profile_picture_to_jpeg(&bytes)?;
                icon_cache.store(&icon_url, &normalized)?;
                Ok::<_, anyhow::Error>(remote_url)
            }
            .await;
            let message = match result {
                Ok(remote_url) => AsyncMsg::NwcIconCached { remote_url },
                Err(_) => AsyncMsg::NwcIconCacheFailed {
                    remote_url: failed_url,
                },
            };
            let _ = tx.send(CoreMsg::Async(message));
        });
    }

    pub(super) fn finish_nwc_icon_cache(&mut self, remote_url: String, succeeded: bool) {
        self.nwc_icon_downloads.remove(&remote_url);
        if succeeded {
            self.refresh_nwc_icon_url(&remote_url);
        }
    }

    pub(super) fn nwc_icon_display_url(&self, remote_url: Option<&str>) -> Option<String> {
        remote_url.and_then(|url| cached_nwc_icon_url(&self.nwc_icon_cache, url))
    }

    fn refresh_nwc_icon_url(&mut self, remote_url: &str) {
        let Some(file_url) = cached_nwc_icon_url(&self.nwc_icon_cache, remote_url) else {
            return;
        };
        for connection in &mut self.state.nwc.connections {
            if connection.icon_url.as_deref() == Some(remote_url) {
                connection.icon_display_url = Some(file_url.clone());
            }
        }
        if let Some(request) = self.state.nwa.request.as_ref() {
            if request.icon_url.as_deref() == Some(remote_url) {
                self.state.nwa.icon_display_url = Some(file_url);
            }
        }
    }
}

fn cached_nwc_icon_url(
    cache: &nwc_mobile::ApplicationIconCache,
    remote_url: &str,
) -> Option<String> {
    let remote_url = ApplicationIconUrl::parse(remote_url).ok()?;
    cache.cached_file_url(&remote_url).ok().flatten()
}

#[cfg(test)]
mod tests {
    use crate::core::tests::test_core;

    use super::*;

    #[test]
    fn inbound_nwa_cannot_replace_the_request_being_reviewed() {
        const CLIENT: &str = "687dd8ece211539364549b1f32c63eceec1e0661009ba65cf8ff2e73ba000746";
        let (_data_dir, _cache_dir, mut core) = test_core();
        core.open_nwa_request(format!(
            "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&name=First"
        ));
        let first = core.state.nwa.request.clone().expect("first request");

        core.open_nwa_request(format!(
            "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&name=Second"
        ));

        let current = core.state.nwa.request.as_ref().expect("current request");
        assert_eq!(current.request_id_hex, first.request_id_hex);
        assert_eq!(current.display_name, "First");
        assert!(core
            .state
            .toast
            .as_deref()
            .is_some_and(|message| message.contains("current Nostr Wallet Auth request")));
    }

    #[test]
    fn cancelling_nwa_never_opens_the_requester_callback() {
        const CLIENT: &str = "687dd8ece211539364549b1f32c63eceec1e0661009ba65cf8ff2e73ba000746";
        const STATE: &str = "0123456789abcdef0123456789abcdef";
        let (_data_dir, _cache_dir, mut core) = test_core();
        core.open_nwa_request(format!(
            "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&return_to=https%3A%2F%2Fapp.example.com%2Fnwa&state={STATE}"
        ));
        core.pending_side_effects.clear();

        core.cancel_nwa_request();

        assert!(core
            .nwc_manager
            .as_ref()
            .expect("manager")
            .service()
            .pending_nwa_request()
            .expect("pending request")
            .is_none());
        assert!(core.pending_side_effects.is_empty());
    }

    #[test]
    fn push_registration_retry_delay_has_a_floor() {
        assert_eq!(
            registration_retry_delay(100, UnixTimestamp::from_secs(100)),
            Duration::from_secs(5)
        );
        assert_eq!(
            registration_retry_delay(99, UnixTimestamp::from_secs(100)),
            Duration::from_secs(5)
        );
        assert_eq!(
            registration_retry_delay(110, UnixTimestamp::from_secs(100)),
            Duration::from_secs(10)
        );
    }
}
