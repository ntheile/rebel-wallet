use anyhow::Context;
use nostr_sdk::prelude::{
    Client as NostrClient, Event, EventBuilder, FinalizeEvent, Keys, Kind, PublicKey, Tag,
};

use crate::NwcPermission;

pub(crate) fn build_nwc_info_event(keys: &Keys) -> anyhow::Result<Event> {
    EventBuilder::new(Kind::WalletConnectInfo, nwc_info_content())
        .tags([Tag::custom("encryption", ["nip04"])])
        .finalize(keys)
        .context("failed to sign NWC info event")
}

pub(crate) fn build_targeted_nwc_info_event(
    keys: &Keys,
    client_pubkey: PublicKey,
    permissions: &[NwcPermission],
) -> anyhow::Result<Event> {
    EventBuilder::new(
        Kind::WalletConnectInfo,
        nwc_info_content_for_permissions(permissions),
    )
    .tags([
        Tag::custom("encryption", ["nip04"]),
        Tag::custom("p", [client_pubkey.to_hex()]),
    ])
    .finalize(keys)
    .context("failed to sign targeted NWC info event")
}

pub(crate) async fn publish_nwc_info_event(relay: String, keys: Keys) -> anyhow::Result<()> {
    let client = client_for_relay(&relay).await?;
    let info_event = build_nwc_info_event(&keys)?;
    let result = client
        .send_event(&info_event)
        .to([relay.as_str()])
        .await
        .context("failed to publish NWC info event")
        .map(|_| ());
    client.shutdown().await;
    result
}

pub(crate) async fn publish_targeted_nwc_info_event(
    relay: String,
    keys: Keys,
    client_pubkey: PublicKey,
    permissions: Vec<NwcPermission>,
) -> anyhow::Result<()> {
    let client = client_for_relay(&relay).await?;
    let info_event = build_targeted_nwc_info_event(&keys, client_pubkey, &permissions)?;
    let result = client
        .send_event(&info_event)
        .to([relay.as_str()])
        .await
        .context("failed to publish targeted NWC info event")
        .map(|_| ());
    client.shutdown().await;
    result
}

async fn client_for_relay(relay: &str) -> anyhow::Result<NostrClient> {
    let client = NostrClient::default();
    client.add_relay(relay).await?;
    client.connect().await;
    Ok(client)
}

fn nwc_info_content() -> String {
    nwc_info_content_for_permissions(&NwcPermission::IMPLEMENTED)
}

fn nwc_info_content_for_permissions(permissions: &[NwcPermission]) -> String {
    NwcPermission::IMPLEMENTED
        .into_iter()
        .filter(|permission| permissions.contains(permission))
        .map(|permission| permission.method_name())
        .collect::<Vec<_>>()
        .join(" ")
}

impl NwcPermission {
    fn method_name(&self) -> &'static str {
        match self {
            Self::PayInvoice => "pay_invoice",
            Self::PayKeysend => "pay_keysend",
            Self::MakeInvoice => "make_invoice",
            Self::LookupInvoice => "lookup_invoice",
            Self::ListTransactions => "list_transactions",
            Self::GetBalance => "get_balance",
            Self::GetInfo => "get_info",
            Self::MakeHoldInvoice => "make_hold_invoice",
            Self::CancelHoldInvoice => "cancel_hold_invoice",
            Self::SettleHoldInvoice => "settle_hold_invoice",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targeted_info_event_identifies_the_authorized_client() {
        let wallet_keys = Keys::generate();
        let client_pubkey = Keys::generate().public_key();

        let event =
            build_targeted_nwc_info_event(&wallet_keys, client_pubkey, &NwcPermission::IMPLEMENTED)
                .expect("targeted info event");

        assert_eq!(event.kind, Kind::WalletConnectInfo);
        assert_eq!(event.pubkey, wallet_keys.public_key());
        assert!(event.content.contains("get_info"));
        assert!(!event.content.contains("pay_keysend"));
        assert!(event.tags.iter().any(|tag| {
            let fields = tag.as_slice();
            fields.first().is_some_and(|field| field == "p")
                && fields
                    .get(1)
                    .is_some_and(|field| field == &client_pubkey.to_hex())
        }));
    }

    #[test]
    fn targeted_info_event_only_advertises_granted_methods() {
        let wallet_keys = Keys::generate();
        let client_pubkey = Keys::generate().public_key();
        let event = build_targeted_nwc_info_event(
            &wallet_keys,
            client_pubkey,
            &[NwcPermission::GetInfo, NwcPermission::GetBalance],
        )
        .expect("targeted info event");

        assert_eq!(event.content, "get_info get_balance");
    }
}
