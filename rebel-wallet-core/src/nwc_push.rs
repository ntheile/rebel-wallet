use anyhow::Context;
use nostr_sdk::prelude::Keys;
use nwc_mobile::{
    Clock, HostError, HostErrorKind, HostFuture, NeverCancelled, Nip98Authorization,
    Nip98SigningKey, OperationBudget, OperationContext, SecureWakeServerUrl, SystemClock,
    WakeLedger, WakeRegistrationChange, WakeRegistrationTransport, WakeRegistrationWorker,
};
use nwc_mobile_tokio::run_with_context;
use serde::Serialize;
use std::fmt;
use std::time::Duration;

const REGISTRATION_BATCH_SIZE: usize = 20;
const REGISTRATION_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONFIG_VALUE_BYTES: usize = 2_048;

#[derive(Clone, Default, Eq, PartialEq)]
pub(crate) struct NwcPushConfig {
    pub(crate) server_url: Option<String>,
    pub(crate) push_token: Option<String>,
    pub(crate) app_id: String,
    pub(crate) environment: String,
    pub(crate) install_id: String,
    pub(crate) enabled: bool,
}

impl fmt::Debug for NwcPushConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NwcPushConfig")
            .field("configured", &self.server_url.is_some())
            .field("has_push_token", &self.push_token.is_some())
            .finish_non_exhaustive()
    }
}

impl NwcPushConfig {
    pub(crate) fn ready(&self) -> anyhow::Result<ReadyNwcPushConfig> {
        let server_url = required_bounded(self.server_url.as_deref(), "wake server")?;
        let push_token = required_bounded(self.push_token.as_deref(), "APNs device token")?;
        let app_id = required_bounded(Some(&self.app_id), "app identifier")?;
        let environment = required_bounded(Some(&self.environment), "push environment")?;
        let install_id = required_bounded(Some(&self.install_id), "install identifier")?;
        if !matches!(environment, "sandbox" | "production") {
            anyhow::bail!("push environment is invalid");
        }
        Ok(ReadyNwcPushConfig {
            server_url: SecureWakeServerUrl::parse(server_url)
                .context("wake server must be a secure HTTPS URL")?,
            push_token: push_token.to_string(),
            app_id: app_id.to_string(),
            environment: environment.to_string(),
            install_id: install_id.to_string(),
            enabled: self.enabled,
        })
    }
}

fn required_bounded<'a>(value: Option<&'a str>, label: &str) -> anyhow::Result<&'a str> {
    let value = value.filter(|value| !value.trim().is_empty());
    match value {
        Some(value) if value.len() <= MAX_CONFIG_VALUE_BYTES => Ok(value),
        Some(_) => anyhow::bail!("{label} is too long"),
        None => anyhow::bail!("{label} is not configured"),
    }
}

#[derive(Clone)]
pub(crate) struct ReadyNwcPushConfig {
    server_url: SecureWakeServerUrl,
    push_token: String,
    app_id: String,
    environment: String,
    install_id: String,
    enabled: bool,
}

impl ReadyNwcPushConfig {
    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }
}

impl fmt::Debug for ReadyNwcPushConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadyNwcPushConfig")
            .field("server_url", &self.server_url)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RegistrationPass {
    pub(crate) applied: usize,
    pub(crate) deferred: usize,
    pub(crate) next_attempt_at: Option<u64>,
}

pub(crate) async fn run_registration_worker(
    ledger: &WakeLedger,
    config: ReadyNwcPushConfig,
    keys: Keys,
) -> anyhow::Result<RegistrationPass> {
    let transport = NwcPushTransport::new(config.clone(), keys)?;
    let worker = WakeRegistrationWorker::new(ledger, &transport, &config.server_url, &SystemClock);
    let report = worker
        .run(
            REGISTRATION_BATCH_SIZE,
            OperationBudget::new(REGISTRATION_OPERATION_TIMEOUT)
                .context("invalid wake registration budget")?,
            &NeverCancelled,
        )
        .await
        .context("wake registration outbox pass failed")?;
    let next_attempt_at = ledger
        .next_wake_registration_at()
        .context("could not schedule pending wake registration")?
        .map(|timestamp| timestamp.as_secs());
    Ok(RegistrationPass {
        applied: report.applied(),
        deferred: report.deferred(),
        next_attempt_at,
    })
}

struct NwcPushTransport {
    client: reqwest::Client,
    config: ReadyNwcPushConfig,
    keys: Keys,
}

impl NwcPushTransport {
    fn new(config: ReadyNwcPushConfig, keys: Keys) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build wake registration client")?;
        Ok(Self {
            client,
            config,
            keys,
        })
    }

    async fn apply_change(
        &self,
        server_url: &SecureWakeServerUrl,
        change: &WakeRegistrationChange,
    ) -> Result<(), HostError> {
        let base_url = reqwest::Url::parse(server_url.as_str())
            .map_err(|_| HostError::new(HostErrorKind::Internal))?;
        let url = base_url
            .join("register-nwc-push")
            .map_err(|_| HostError::new(HostErrorKind::Internal))?;
        let endpoint = SecureWakeServerUrl::parse(url.as_str())
            .map_err(|_| HostError::new(HostErrorKind::Internal))?;
        let client_pubkey = change.client_pubkey().to_hex();
        let wallet_service_pubkey = change.wallet_service_pubkey().to_hex();
        if self.keys.public_key().to_hex() != wallet_service_pubkey {
            return Err(HostError::new(HostErrorKind::Rejected));
        }
        for relay in change.relays() {
            let payload = RegisterNwcPushPayload {
                id: &self.config.install_id,
                connection_id: change.connection_id().as_str(),
                connection_revision: change.connection_revision().value(),
                push_service: "apns",
                push_token: &self.config.push_token,
                app_id: &self.config.app_id,
                environment: &self.config.environment,
                client_pubkey: &client_pubkey,
                wallet_service_pubkey: &wallet_service_pubkey,
                relay: relay.as_str(),
                name: "NWC connection",
                enabled: change.enabled(),
            };
            let body = serde_json::to_vec(&payload)
                .map_err(|_| HostError::new(HostErrorKind::Internal))?;
            let signing_key = Nip98SigningKey::from_bytes(self.keys.secret_key().to_secret_bytes())
                .map_err(|_| HostError::new(HostErrorKind::Internal))?;
            let auth = Nip98Authorization::for_registration_post(
                &endpoint,
                &body,
                &signing_key,
                SystemClock.now(),
            )
            .map_err(|_| HostError::new(HostErrorKind::Internal))?;
            let response = self
                .client
                .post(url.clone())
                .header(reqwest::header::AUTHORIZATION, auth.as_header_value())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(classify_request_error)?;
            let status = response.status();
            if !status.is_success() {
                let kind = if status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                {
                    HostErrorKind::Unavailable
                } else {
                    HostErrorKind::Rejected
                };
                return Err(HostError::new(kind));
            }
        }
        Ok(())
    }
}

impl WakeRegistrationTransport for NwcPushTransport {
    fn apply<'a>(
        &'a self,
        server_url: &'a SecureWakeServerUrl,
        change: &'a WakeRegistrationChange,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<(), HostError>> {
        Box::pin(
            async move { run_with_context(context, self.apply_change(server_url, change)).await },
        )
    }
}

fn classify_request_error(error: reqwest::Error) -> HostError {
    let kind = if error.is_timeout() {
        HostErrorKind::TimedOut
    } else if error.is_builder() {
        HostErrorKind::Internal
    } else {
        HostErrorKind::Unavailable
    };
    HostError::new(kind)
}

#[derive(Serialize)]
struct RegisterNwcPushPayload<'a> {
    id: &'a str,
    connection_id: &'a str,
    connection_revision: u64,
    push_service: &'static str,
    push_token: &'a str,
    app_id: &'a str,
    environment: &'a str,
    client_pubkey: &'a str,
    wallet_service_pubkey: &'a str,
    relay: &'a str,
    name: &'static str,
    enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(server_url: &str) -> NwcPushConfig {
        NwcPushConfig {
            server_url: Some(server_url.to_string()),
            push_token: Some("super-secret-token".to_string()),
            app_id: "com.example.wallet".to_string(),
            environment: "sandbox".to_string(),
            install_id: "install".to_string(),
            enabled: true,
        }
    }

    #[test]
    fn config_requires_https_and_known_apns_environment() {
        assert!(config("https://wake.example.com").ready().is_ok());
        assert!(config("http://wake.example.com").ready().is_err());
        let mut invalid_environment = config("https://wake.example.com");
        invalid_environment.environment = "development".to_string();
        assert!(invalid_environment.ready().is_err());
    }

    #[test]
    fn config_debug_output_redacts_provider_metadata() {
        let config = config("https://private.example.com/wake");
        let debug = format!("{config:?}");
        assert!(!debug.contains("private.example.com"));
        assert!(!debug.contains("super-secret-token"));
        assert!(!debug.contains("com.example.wallet"));
        assert!(!debug.contains("install"));
    }
}
