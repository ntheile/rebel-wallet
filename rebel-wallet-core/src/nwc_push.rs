use anyhow::Context;
use nostr_sdk::prelude::Keys;
use serde::Serialize;

use crate::nostr_support::nostr_http_auth_header;
use crate::NwcConnection;

const REGISTRATION_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, Default)]
pub(crate) struct NwcPushConfig {
    pub(crate) server_url: Option<String>,
    pub(crate) push_token: Option<String>,
    pub(crate) app_id: String,
    pub(crate) environment: String,
    pub(crate) install_id: String,
}

impl NwcPushConfig {
    pub(crate) fn ready(&self) -> anyhow::Result<ReadyNwcPushConfig> {
        let server_url = self
            .server_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .context("wake server is not configured")?;
        let push_token = self
            .push_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .context("APNs device token is not available")?;
        if self.app_id.is_empty() || self.install_id.is_empty() {
            anyhow::bail!("push registration metadata is incomplete");
        }
        Ok(ReadyNwcPushConfig {
            server_url: reqwest::Url::parse(server_url).context("invalid wake server URL")?,
            push_token: push_token.to_string(),
            app_id: self.app_id.clone(),
            environment: self.environment.clone(),
            install_id: self.install_id.clone(),
        })
    }

    pub(crate) fn fingerprint(&self, connections: &[NwcConnection]) -> Option<String> {
        let ready = self.ready().ok()?;
        let mut connection_values = connections
            .iter()
            .map(|connection| {
                format!(
                    "{}|{}|{}|{}|{}",
                    connection.id,
                    connection.client_pubkey,
                    connection.service_pubkey,
                    connection.relay,
                    connection.name
                )
            })
            .collect::<Vec<_>>();
        connection_values.sort();
        Some(format!(
            "{}\n{}\n{}",
            ready.push_token,
            ready.app_id,
            connection_values.join("\n")
        ))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReadyNwcPushConfig {
    server_url: reqwest::Url,
    push_token: String,
    app_id: String,
    environment: String,
    install_id: String,
}

#[derive(Serialize)]
struct RegisterNwcPushPayload<'a> {
    id: &'a str,
    push_service: &'static str,
    push_token: &'a str,
    app_id: &'a str,
    environment: &'a str,
    client_pubkey: &'a str,
    wallet_service_pubkey: &'a str,
    relay: &'a str,
    name: &'a str,
    enabled: bool,
}

pub(crate) async fn register_connections(
    config: ReadyNwcPushConfig,
    keys: Keys,
    connections: Vec<NwcConnection>,
    enabled: bool,
) -> anyhow::Result<()> {
    for connection in connections {
        for relay in relay_values(&connection.relay) {
            let mut last_error = None;
            for attempt in 0..REGISTRATION_ATTEMPTS {
                match register_connection(&config, &keys, &connection, &relay, enabled).await {
                    Ok(()) => {
                        last_error = None;
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
                if attempt + 1 < REGISTRATION_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
                }
            }
            if let Some(error) = last_error {
                return Err(error);
            }
        }
    }
    Ok(())
}

async fn register_connection(
    config: &ReadyNwcPushConfig,
    keys: &Keys,
    connection: &NwcConnection,
    relay: &str,
    enabled: bool,
) -> anyhow::Result<()> {
    if connection.service_pubkey != keys.public_key().to_hex() {
        anyhow::bail!("NWC connection service pubkey does not match registration signer");
    }
    let url = config
        .server_url
        .join("register-nwc-push")
        .context("invalid wake registration endpoint")?;
    let payload = RegisterNwcPushPayload {
        id: &config.install_id,
        push_service: "apns",
        push_token: &config.push_token,
        app_id: &config.app_id,
        environment: &config.environment,
        client_pubkey: &connection.client_pubkey,
        wallet_service_pubkey: &connection.service_pubkey,
        relay,
        name: &connection.name,
        enabled,
    };
    let body = serde_json::to_vec(&payload).context("failed to encode wake registration")?;
    let auth = nostr_http_auth_header(keys, url.as_str(), "POST", &body)
        .context("failed to sign wake registration")?;
    let response = reqwest::Client::new()
        .post(url)
        .header(reqwest::header::AUTHORIZATION, auth)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .context("wake registration request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("wake server rejected registration with {status}: {body}");
    }
    Ok(())
}

fn relay_values(value: &str) -> Vec<String> {
    let mut relays = Vec::new();
    for relay in value
        .split(|character: char| character.is_whitespace() || character == ',')
        .map(str::trim)
        .filter(|relay| !relay.is_empty())
    {
        let relay = relay.trim_end_matches('/').to_string();
        if !relays.contains(&relay) {
            relays.push(relay);
        }
        if relays.len() == 2 {
            break;
        }
    }
    relays
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_across_connection_order() {
        let config = NwcPushConfig {
            server_url: Some("https://wake.example.com".to_string()),
            push_token: Some("token".to_string()),
            app_id: "com.example.wallet".to_string(),
            environment: "sandbox".to_string(),
            install_id: "install".to_string(),
        };
        let mut first = test_connection("a");
        let mut second = test_connection("b");
        first.name = "First".to_string();
        second.name = "Second".to_string();
        assert_eq!(
            config.fingerprint(&[first.clone(), second.clone()]),
            config.fingerprint(&[second, first])
        );
    }

    fn test_connection(id: &str) -> NwcConnection {
        NwcConnection {
            id: id.to_string(),
            name: id.to_string(),
            icon_url: None,
            relay: "wss://relay.example.com".to_string(),
            uri: String::new(),
            wallet_managed_secret: false,
            service_pubkey: "service".to_string(),
            client_pubkey: id.to_string(),
            budget_sat: 0,
            spent_sat: 0,
            budget_display: String::new(),
            spent_display: String::new(),
            budget_interval: crate::NwcBudgetInterval::Never,
            budget_interval_display: String::new(),
            permissions: Vec::new(),
            permissions_configured: true,
            allow_get_balance: false,
            allow_pay_invoice: false,
            created_at: 0,
            last_used_at: None,
            expires_at: None,
            budget_period_started_at: 0,
            pending_info_event_relays: Vec::new(),
        }
    }
}
