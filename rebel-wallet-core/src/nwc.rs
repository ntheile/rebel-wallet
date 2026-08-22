use std::time::Duration;

use anyhow::Context;
use nostr_sdk::prelude::{Keys, PublicKey};
use nwc_mobile::{NwcMethod, NwcSecretKey, PublicKey as MobilePublicKey};
use nwc_mobile_nostr::publish_nwc_info_event as publish_shared_nwc_info_event;

use crate::nwc_mobile_registry::{permission_method, NWC_ENCRYPTION};
use crate::NwcPermission;

const INFO_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn publish_nwc_info_event(relay: String, keys: Keys) -> anyhow::Result<()> {
    publish_info_event(relay, keys, None, supported_methods()).await
}

pub(crate) async fn publish_targeted_nwc_info_event(
    relay: String,
    keys: Keys,
    client_pubkey: PublicKey,
    permissions: Vec<NwcPermission>,
) -> anyhow::Result<()> {
    let client_pubkey = MobilePublicKey::from_hex(&client_pubkey.to_hex())
        .context("invalid NWC client public key")?;
    publish_info_event(
        relay,
        keys,
        Some(client_pubkey),
        supported_methods_for(&permissions),
    )
    .await
}

async fn publish_info_event(
    relay: String,
    keys: Keys,
    client_pubkey: Option<MobilePublicKey>,
    methods: Vec<NwcMethod>,
) -> anyhow::Result<()> {
    let secret = keys.secret_key();
    let secret = NwcSecretKey::from_bytes(secret.to_secret_bytes())
        .context("invalid NWC wallet service key")?;
    publish_shared_nwc_info_event(
        &relay,
        &secret,
        client_pubkey.as_ref(),
        methods,
        NWC_ENCRYPTION,
        INFO_PUBLISH_TIMEOUT,
    )
    .await
    .context("failed to publish NWC info event")
}

fn supported_methods() -> Vec<NwcMethod> {
    supported_methods_for(&NwcPermission::IMPLEMENTED)
}

fn supported_methods_for(permissions: &[NwcPermission]) -> Vec<NwcMethod> {
    NwcPermission::IMPLEMENTED
        .into_iter()
        .filter(|permission| permissions.contains(permission))
        .filter_map(permission_method)
        .collect()
}
