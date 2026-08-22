use anyhow::Context;
use nostr_sdk::prelude::Keys;
use nwc_mobile::{Nip98SigningKey, WakeLedger};

pub(crate) use nwc_mobile_http::{ApnsWakeRegistrationConfig as NwcPushConfig, RegistrationPass};

pub(crate) async fn run_registration_worker(
    ledger: &WakeLedger,
    config: nwc_mobile_http::ReadyApnsWakeRegistrationConfig,
    keys: Keys,
) -> anyhow::Result<RegistrationPass> {
    let signing_key = Nip98SigningKey::from_bytes(keys.secret_key().to_secret_bytes())
        .context("invalid wake registration signing key")?;
    nwc_mobile_http::run_registration_worker(ledger, config, signing_key)
        .await
        .context("wake registration outbox pass failed")
}
