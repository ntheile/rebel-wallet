use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use bitcoin::hashes::Hash as _;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};
use nostr_sdk::prelude::{
    Alphabet, Event, EventBuilder, Filter, FinalizeEvent, JsonUtil, Keys, Kind,
    PublicKey as NostrPublicKey, RelayUrl, SingleLetterTag, ZapRequestData,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::nostr_support::{nostr_client, public_key_from_npub_or_hex, NOSTR_RELAYS};
use crate::payments::{lnurl_pay_url, msats_to_display_sats};
use crate::persistence::ZapReceiptRecord;

#[derive(Clone, Debug)]
pub(crate) struct ZapEndpoint {
    pub(crate) callback: String,
    pub(crate) min_sendable: u64,
    pub(crate) max_sendable: u64,
    pub(crate) lnurl: String,
}

#[derive(Debug, Deserialize)]
struct LnurlZapParams {
    tag: Option<String>,
    callback: String,
    #[serde(rename = "minSendable")]
    min_sendable: u64,
    #[serde(rename = "maxSendable")]
    max_sendable: u64,
    #[serde(rename = "allowsNostr")]
    allows_nostr: Option<bool>,
    #[serde(rename = "nostrPubkey")]
    nostr_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LnurlZapInvoice {
    pr: Option<String>,
    status: Option<String>,
    reason: Option<String>,
}

pub(crate) async fn request_zap_invoice(
    destination: &str,
    recipient_pubkey: NostrPublicKey,
    amount_sat: u64,
    comment: &str,
    keys: &Keys,
) -> anyhow::Result<String> {
    if amount_sat == 0 {
        bail!("Enter an amount before sending a zap.");
    }
    let endpoint = fetch_zap_endpoint(destination).await?;
    let amount_msat = amount_sat
        .checked_mul(1_000)
        .ok_or_else(|| anyhow!("zap amount is too large"))?;
    if amount_msat < endpoint.min_sendable || amount_msat > endpoint.max_sendable {
        bail!(
            "Zap amount must be between {} and {} sats.",
            msats_to_display_sats(endpoint.min_sendable),
            msats_to_display_sats(endpoint.max_sendable)
        );
    }

    let relays = zap_relays()?;
    let data = ZapRequestData::new(recipient_pubkey, relays)
        .message(comment.trim())
        .amount(amount_msat)
        .lnurl(endpoint.lnurl.clone());
    let event = EventBuilder::public_zap_request(data).finalize(keys)?;
    let mut callback =
        reqwest::Url::parse(&endpoint.callback).context("LNURL callback is not a valid URL")?;
    callback
        .query_pairs_mut()
        .append_pair("amount", &amount_msat.to_string())
        .append_pair("nostr", &event.as_json())
        .append_pair("lnurl", &endpoint.lnurl);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build zap client")?;
    let invoice = client
        .get(callback)
        .send()
        .await
        .context("failed to fetch zap invoice")?
        .error_for_status()
        .context("zap invoice request returned an error")?
        .json::<LnurlZapInvoice>()
        .await
        .context("failed to parse zap invoice response")?;

    if invoice.status.as_deref() == Some("ERROR") {
        bail!(
            "{}",
            invoice
                .reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| "LNURL endpoint returned an error".to_string())
        );
    }

    invoice
        .pr
        .filter(|pr| !pr.trim().is_empty())
        .ok_or_else(|| anyhow!("LNURL endpoint did not return an invoice"))
}

async fn fetch_zap_endpoint(destination: &str) -> anyhow::Result<ZapEndpoint> {
    let url = lnurl_pay_url(destination)?;
    let lnurl = encode_lnurl(url.as_str())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build LNURL client")?;
    let params = client
        .get(url)
        .send()
        .await
        .context("failed to fetch LNURL pay request")?
        .error_for_status()
        .context("LNURL pay request returned an error")?
        .json::<LnurlZapParams>()
        .await
        .context("failed to parse LNURL pay request")?;

    if params.tag.as_deref() != Some("payRequest") {
        bail!("LNURL endpoint is not a pay request");
    }
    if params.allows_nostr != Some(true) {
        bail!("Recipient does not support zaps.");
    }
    let _nostr_pubkey = params
        .nostr_pubkey
        .filter(|key| public_key_from_npub_or_hex(key).is_ok())
        .ok_or_else(|| anyhow!("Recipient zap endpoint returned an invalid Nostr pubkey"))?;

    Ok(ZapEndpoint {
        callback: params.callback,
        min_sendable: params.min_sendable,
        max_sendable: params.max_sendable,
        lnurl,
    })
}

fn encode_lnurl(url: &str) -> anyhow::Result<String> {
    let hrp = bech32::Hrp::parse("lnurl").context("invalid LNURL HRP")?;
    bech32::encode::<bech32::Bech32>(hrp, url.as_bytes()).context("failed to encode LNURL")
}

fn zap_relays() -> anyhow::Result<Vec<RelayUrl>> {
    NOSTR_RELAYS
        .iter()
        .map(|relay| RelayUrl::parse(relay).map_err(anyhow::Error::from))
        .collect()
}

pub(crate) async fn fetch_received_zap_receipts(
    own_pubkey: NostrPublicKey,
) -> anyhow::Result<Vec<ZapReceiptRecord>> {
    let client = nostr_client().await?;
    add_zap_scan_relays(&client).await;

    let mut receipts = Vec::new();
    let mut seen = HashSet::new();
    let filter = Filter::new()
        .kind(Kind::ZapReceipt)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::P), own_pubkey.to_hex())
        .limit(200);
    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await?;
    for event in events.into_iter() {
        if !seen.insert(event.id) {
            continue;
        }
        if let Some(receipt) = zap_receipt_from_event(&event, &own_pubkey) {
            receipts.push(receipt);
        }
    }
    Ok(receipts)
}

async fn add_zap_scan_relays(client: &nostr_sdk::prelude::Client) {
    for relay in [
        "wss://nos.lol",
        "wss://relay.nostr.band",
        "wss://nostr.mom",
        "wss://relay.snort.social",
        "wss://purplepag.es",
        "wss://relay.benthecarman.com",
    ] {
        let _ = client.add_relay(relay).await;
    }
    client.connect().await;
}

pub(crate) fn zap_receipt_from_event(
    event: &Event,
    own_pubkey: &NostrPublicKey,
) -> Option<ZapReceiptRecord> {
    if event.kind != Kind::ZapReceipt {
        return None;
    }
    if event.verify().is_err() {
        return None;
    }
    let own_hex = own_pubkey.to_hex();
    let tags = tag_map(event);
    let description = tags.get("description").cloned()?;
    let zap_request = Event::from_json(&description).ok()?;
    if zap_request.kind != Kind::ZapRequest || zap_request.verify().is_err() {
        return None;
    }
    let zap_request = Some(zap_request);
    let request_tags = zap_request.as_ref().map(tag_map).unwrap_or_default();
    let tag_p = tag_values(event, "p");
    let tag_upper_p = tag_values(event, "P");
    let request_p = zap_request
        .as_ref()
        .map(|request| tag_values(request, "p"))
        .unwrap_or_default();
    let request_pubkey = zap_request.as_ref().map(|request| request.pubkey.to_hex());

    if !tag_p.iter().any(|value| value == &own_hex) {
        return None;
    }
    if !request_p.is_empty() && !request_p.iter().any(|value| value == &own_hex) {
        return None;
    }

    let sender_pubkey = tag_upper_p
        .first()
        .cloned()
        .or_else(|| request_pubkey.clone())?;
    if request_pubkey
        .as_ref()
        .is_some_and(|pubkey| pubkey != &sender_pubkey)
    {
        return None;
    }
    if sender_pubkey == own_hex {
        return None;
    }
    let comment = zap_request
        .as_ref()
        .map(|event| event.content.trim().to_string())
        .filter(|content| !content.is_empty());

    // The invoice must be bound to the embedded zap request, either by the
    // NIP-57 description hash or by an exact plain description (arkzap-me).
    let invoice = tags.get("bolt11")?;
    let invoice = Bolt11Invoice::from_str(invoice).ok()?;
    let description_hash: [u8; 32] = Sha256::digest(description.as_bytes()).into();
    let invoice_matches_request = match invoice.description() {
        Bolt11InvoiceDescriptionRef::Hash(hash) => *hash.0.as_byte_array() == description_hash,
        Bolt11InvoiceDescriptionRef::Direct(direct) => direct.as_inner().0 == description,
    };
    if !invoice_matches_request {
        return None;
    }

    let receipt_amount_msat = tags
        .get("amount")
        .and_then(|value| value.parse::<u64>().ok());
    let request_amount_msat = request_tags
        .get("amount")
        .and_then(|value| value.parse::<u64>().ok());
    for amount_msat in [receipt_amount_msat, request_amount_msat]
        .into_iter()
        .flatten()
    {
        if invoice.amount_milli_satoshis() != Some(amount_msat) {
            return None;
        }
    }
    let amount_msat = receipt_amount_msat.or(request_amount_msat);
    let lnurl = request_tags.get("lnurl").cloned();
    let payment_hash = Some(invoice.payment_hash().to_string());
    let invoice = tags.get("bolt11").cloned();

    Some(ZapReceiptRecord {
        event_id: event.id.to_hex(),
        sender_pubkey,
        recipient_pubkey: own_hex,
        invoice,
        payment_hash,
        amount_msat,
        lnurl,
        comment,
        created_at: event.created_at.to_string().parse().unwrap_or_default(),
    })
}

fn tag_values(event: &Event, kind: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter(|tag| tag.kind() == kind)
        .filter_map(|tag| tag.content().map(str::to_string))
        .collect()
}

fn tag_map(event: &Event) -> HashMap<String, String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            tag.content()
                .map(|content| (tag.kind().to_string(), content.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::{sha256, Hash};
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use nostr_sdk::prelude::Tag;
    use secp256k1::{Secp256k1, SecretKey};

    fn test_invoice(description: &str, amount_msats: u64, use_description_hash: bool) -> String {
        let private_key = SecretKey::from_slice(&[42u8; 32]).expect("secret key");
        let payment_hash = sha256::Hash::from_slice(&[0u8; 32]).expect("payment hash");
        let builder = InvoiceBuilder::new(Currency::Bitcoin);
        let builder = if use_description_hash {
            builder.description_hash(sha256::Hash::hash(description.as_bytes()))
        } else {
            builder.description(description.to_string())
        };
        builder
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([7u8; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .amount_milli_satoshis(amount_msats)
            .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
            .expect("invoice")
            .to_string()
    }

    #[test]
    fn ignores_receipt_where_only_uppercase_p_matches_own_pubkey() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["P", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["p", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn parses_received_receipt_using_lowercase_p_as_recipient_and_uppercase_p_as_sender() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
                Tag::parse(["bolt11", &test_invoice(&request.as_json(), 21_000, true)]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        let parsed = zap_receipt_from_event(&receipt, &own.public_key()).unwrap();

        assert_eq!(parsed.recipient_pubkey, own.public_key().to_hex());
        assert_eq!(parsed.sender_pubkey, sender.public_key().to_hex());
        assert_eq!(parsed.amount_msat, Some(21_000));
    }

    #[test]
    fn ignores_receipt_with_missing_description() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_with_tampered_zap_request_content() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "thanks")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let mut request_json: serde_json::Value = serde_json::from_str(&request.as_json()).unwrap();
        request_json["content"] = serde_json::Value::from("send refund to attacker");
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request_json.to_string()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_when_zap_request_pubkey_does_not_match_signature() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let impersonated = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "thanks")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let mut request_json: serde_json::Value = serde_json::from_str(&request.as_json()).unwrap();
        request_json["pubkey"] = serde_json::Value::from(impersonated.public_key().to_hex());
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &impersonated.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request_json.to_string()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_when_embedded_event_is_not_a_zap_request() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let not_a_request = EventBuilder::new(Kind::TextNote, "thanks")
            .tags([Tag::parse(["p", &own.public_key().to_hex()]).unwrap()])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &not_a_request.as_json()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_with_tampered_outer_event() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "21000"]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();
        let mut receipt_json: serde_json::Value = serde_json::from_str(&receipt.as_json()).unwrap();
        receipt_json["content"] = serde_json::Value::from("tampered");
        let tampered = Event::from_json(receipt_json.to_string()).unwrap();

        assert!(zap_receipt_from_event(&tampered, &own.public_key()).is_none());
    }

    #[test]
    fn parses_received_receipt_using_lowercase_p_as_recipient() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "thanks")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
                Tag::parse([
                    "bolt11",
                    &test_invoice(&request.as_json(), 1_000_000, false),
                ])
                .unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        let parsed = zap_receipt_from_event(&receipt, &own.public_key()).unwrap();

        assert_eq!(parsed.recipient_pubkey, own.public_key().to_hex());
        assert_eq!(parsed.sender_pubkey, sender.public_key().to_hex());
        assert_eq!(parsed.amount_msat, Some(1_000_000));
        assert_eq!(parsed.comment, Some("thanks".to_string()));
    }

    #[test]
    fn ignores_receipt_with_missing_bolt11() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "thanks")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_with_invoice_for_a_different_description() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "thanks")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let other_request = EventBuilder::new(Kind::ZapRequest, "other")
            .tags([Tag::parse(["p", &own.public_key().to_hex()]).unwrap()])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
                Tag::parse([
                    "bolt11",
                    &test_invoice(&other_request.as_json(), 1_000_000, false),
                ])
                .unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_when_amount_tag_differs_from_invoice() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "thanks")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "2000000"]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
                Tag::parse([
                    "bolt11",
                    &test_invoice(&request.as_json(), 1_000_000, false),
                ])
                .unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_outgoing_receipt_indexed_by_uppercase_p() {
        let own = Keys::generate();
        let recipient = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "")
            .tags([
                Tag::parse(["p", &recipient.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000000"]).unwrap(),
            ])
            .finalize(&own)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &recipient.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_when_description_recipient_differs_from_own_pubkey() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let other_recipient = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "")
            .tags([
                Tag::parse(["p", &other_recipient.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }

    #[test]
    fn ignores_receipt_when_sender_tag_conflicts_with_request_author() {
        let own = Keys::generate();
        let sender = Keys::generate();
        let other_sender = Keys::generate();
        let request = EventBuilder::new(Kind::ZapRequest, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["amount", "1000000"]).unwrap(),
            ])
            .finalize(&sender)
            .unwrap();
        let receipt = EventBuilder::new(Kind::ZapReceipt, "")
            .tags([
                Tag::parse(["p", &own.public_key().to_hex()]).unwrap(),
                Tag::parse(["P", &other_sender.public_key().to_hex()]).unwrap(),
                Tag::parse(["description", &request.as_json()]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap();

        assert!(zap_receipt_from_event(&receipt, &own.public_key()).is_none());
    }
}
