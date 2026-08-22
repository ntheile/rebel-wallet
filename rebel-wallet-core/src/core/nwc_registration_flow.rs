use super::*;
use nwc_mobile::{
    ApplicationRegistrationBegin, ApplicationRegistrationCompletion, ApplicationRegistrationPass,
};

impl AppCore {
    pub(super) fn sync_nwc_push_registrations(&mut self) {
        let Ok(config) = self.nwc_push_config.ready() else {
            return;
        };
        let Ok(keys) = self.nostr_keys() else {
            return;
        };
        match self.nwc_registration.begin() {
            ApplicationRegistrationBegin::Busy => return,
            ApplicationRegistrationBegin::RefreshRequired => {
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
                self.nwc_registration.complete_refresh();
                if self.nwc_registration.begin() != ApplicationRegistrationBegin::Ready {
                    return;
                }
            }
            ApplicationRegistrationBegin::Ready => {}
            _ => return,
        }

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
        let pass = if error.is_some() {
            ApplicationRegistrationPass::failed()
        } else {
            ApplicationRegistrationPass::completed(applied, deferred, next_attempt_at)
        };
        match self.nwc_registration.finish(pass, now_unix()) {
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
        let nonce = self.nwc_registration.schedule_retry();
        let delay = nwc_push_retry_delay(next_attempt_at, now_unix());
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(CoreMsg::Async(AsyncMsg::NwcPushRetryDue { nonce }));
        });
    }
}
