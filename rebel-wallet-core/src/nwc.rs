use std::time::Duration;

use anyhow::{anyhow, Context};
use nostr::nips::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, Method, NIP47Error, Request, RequestParams,
    Response, ResponseResult,
};
use nostr_sdk::prelude::{
    nip04, Client as NostrClient, Event, EventBuilder, EventId, Filter, FinalizeEvent, JsonUtil,
    Keys, Kind, Tag,
};

use crate::nostr_support::public_key_from_npub_or_hex;
use crate::{NwcConnection, NwcPermission, NwcWakeRequest, WalletNetwork};

#[derive(Clone, Debug)]
pub(crate) struct NwcServiceContext {
    pub(crate) keys: Keys,
    pub(crate) balance_sat: u64,
    pub(crate) network: WalletNetwork,
    pub(crate) connections: Vec<NwcConnection>,
}

#[derive(Clone, Debug)]
pub(crate) struct NwcProcessedWake {
    pub(crate) wake: NwcWakeRequest,
    pub(crate) client_pubkey: String,
    pub(crate) method: String,
    pub(crate) status: String,
    pub(crate) processed_at: u64,
}

pub(crate) async fn process_nwc_wake_request(
    wake: NwcWakeRequest,
    context: NwcServiceContext,
) -> anyhow::Result<NwcProcessedWake> {
    let event = fetch_nwc_request_event(&wake).await?;
    process_nwc_request_event(wake, event, context).await
}

pub(crate) async fn process_nwc_request_event(
    wake: NwcWakeRequest,
    event: Event,
    context: NwcServiceContext,
) -> anyhow::Result<NwcProcessedWake> {
    let connection = authorized_connection(&event, &context)?;
    let request = decrypt_nwc_request(&event, &context.keys)?;
    let method = request.method.as_str().to_string();
    let response = response_for_request(&request, &context, connection);
    let response_event = build_nwc_response_event(&event, response, &context.keys)?;

    let client = client_for_relay(&wake.relay).await?;
    client
        .send_event(&response_event)
        .to([wake.relay.as_str()])
        .await
        .context("failed to publish NWC response")?;

    Ok(NwcProcessedWake {
        wake,
        client_pubkey: event.pubkey.to_hex(),
        method,
        status: "Responded".to_string(),
        processed_at: crate::time::now_unix(),
    })
}

pub(crate) fn build_nwc_info_event(keys: &Keys) -> anyhow::Result<Event> {
    EventBuilder::new(Kind::WalletConnectInfo, nwc_info_content())
        .tags([Tag::custom("encryption", ["nip04"])])
        .finalize(keys)
        .context("failed to sign NWC info event")
}

fn authorized_connection<'a>(
    event: &Event,
    context: &'a NwcServiceContext,
) -> anyhow::Result<&'a NwcConnection> {
    let client_pubkey = event.pubkey.to_hex();
    context
        .connections
        .iter()
        .find(|connection| connection.client_pubkey == client_pubkey)
        .ok_or_else(|| anyhow!("NWC request client is not authorized: {client_pubkey}"))
}

async fn fetch_nwc_request_event(wake: &NwcWakeRequest) -> anyhow::Result<Event> {
    let client = client_for_relay(&wake.relay).await?;
    let event_id = EventId::from_hex(&wake.event_id).context("invalid NWC event id")?;
    let filter = Filter::new()
        .id(event_id)
        .kind(Kind::WalletConnectRequest)
        .limit(1);
    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await
        .context("failed to fetch NWC request event")?;

    events
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("NWC request event was not found on relay"))
}

async fn client_for_relay(relay: &str) -> anyhow::Result<NostrClient> {
    let client = NostrClient::default();
    client.add_relay(relay).await.context("invalid NWC relay")?;
    client.connect().await;
    Ok(client)
}

fn decrypt_nwc_request(event: &Event, keys: &Keys) -> anyhow::Result<Request> {
    let decrypted = nip04::decrypt(keys.secret_key(), &event.pubkey, &event.content)
        .context("failed to decrypt NWC request")?;
    Request::from_json(&decrypted).context("failed to parse NWC request")
}

fn response_for_request(
    request: &Request,
    context: &NwcServiceContext,
    connection: &NwcConnection,
) -> Response {
    let permission = permission_for_request(&request.params);
    if !connection.allows_permission(permission) {
        return error_response(
            permission.to_method(),
            ErrorCode::Restricted,
            "This NWC connection is not allowed to use this method.",
        );
    }

    match &request.params {
        RequestParams::GetInfo => Response {
            result_type: Method::GetInfo,
            error: None,
            result: Some(ResponseResult::GetInfo(GetInfoResponse {
                alias: Some("Rebel Wallet".to_string()),
                color: None,
                pubkey: Some(context.keys.public_key().to_hex()),
                network: Some(network_name(&context.network).to_string()),
                block_height: None,
                block_hash: None,
                methods: connection_methods(connection),
                notifications: vec![],
            })),
        },
        RequestParams::GetBalance => Response {
            result_type: Method::GetBalance,
            error: None,
            result: Some(ResponseResult::GetBalance(GetBalanceResponse {
                balance: context.balance_sat.saturating_mul(1_000),
            })),
        },
        _ => not_implemented_response(request.method.clone()),
    }
}

fn not_implemented_response(method: Method) -> Response {
    error_response(
            method,
            ErrorCode::NotImplemented,
            "This NWC method is allowed for this connection but is not implemented in Rebel Wallet yet.",
        )
}

fn connection_methods(connection: &NwcConnection) -> Vec<Method> {
    connection
        .enabled_permissions()
        .into_iter()
        .map(|permission| permission.to_method())
        .collect()
}

fn nwc_info_content() -> String {
    NwcPermission::ALL
        .into_iter()
        .map(|permission| permission.method_name())
        .collect::<Vec<_>>()
        .join(" ")
}

fn permission_for_request(params: &RequestParams) -> NwcPermission {
    match params {
        RequestParams::PayInvoice(_) => NwcPermission::PayInvoice,
        RequestParams::PayKeysend(_) => NwcPermission::PayKeysend,
        RequestParams::MakeInvoice(_) => NwcPermission::MakeInvoice,
        RequestParams::LookupInvoice(_) => NwcPermission::LookupInvoice,
        RequestParams::ListTransactions(_) => NwcPermission::ListTransactions,
        RequestParams::GetBalance => NwcPermission::GetBalance,
        RequestParams::GetInfo => NwcPermission::GetInfo,
        RequestParams::MakeHoldInvoice(_) => NwcPermission::MakeHoldInvoice,
        RequestParams::CancelHoldInvoice(_) => NwcPermission::CancelHoldInvoice,
        RequestParams::SettleHoldInvoice(_) => NwcPermission::SettleHoldInvoice,
    }
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

    fn to_method(self) -> Method {
        match self {
            Self::PayInvoice => Method::PayInvoice,
            Self::PayKeysend => Method::PayKeysend,
            Self::MakeInvoice => Method::MakeInvoice,
            Self::LookupInvoice => Method::LookupInvoice,
            Self::ListTransactions => Method::ListTransactions,
            Self::GetBalance => Method::GetBalance,
            Self::GetInfo => Method::GetInfo,
            Self::MakeHoldInvoice => Method::MakeHoldInvoice,
            Self::CancelHoldInvoice => Method::CancelHoldInvoice,
            Self::SettleHoldInvoice => Method::SettleHoldInvoice,
        }
    }
}

fn build_nwc_response_event(
    request_event: &Event,
    response: Response,
    keys: &Keys,
) -> anyhow::Result<Event> {
    let encrypted = nip04::encrypt(keys.secret_key(), &request_event.pubkey, response.as_json())
        .context("failed to encrypt NWC response")?;
    EventBuilder::new(Kind::WalletConnectResponse, encrypted)
        .tags([
            Tag::public_key(request_event.pubkey),
            Tag::event(request_event.id),
        ])
        .finalize(keys)
        .context("failed to sign NWC response")
}

fn error_response(method: Method, code: ErrorCode, message: &str) -> Response {
    Response {
        result_type: method,
        error: Some(NIP47Error {
            code,
            message: message.to_string(),
        }),
        result: None,
    }
}

pub(crate) fn validate_wallet_service_pubkey(expected: &str, keys: &Keys) -> anyhow::Result<()> {
    let expected = public_key_from_npub_or_hex(expected)
        .context("invalid NWC wallet service pubkey; expected npub or 64-character hex pubkey")?;
    if expected != keys.public_key() {
        anyhow::bail!(
            "NWC wake request targets a different wallet service pubkey: expected {}, got {}",
            keys.public_key().to_hex(),
            expected.to_hex()
        );
    }
    Ok(())
}

fn network_name(network: &WalletNetwork) -> &'static str {
    match network {
        WalletNetwork::Mainnet => "mainnet",
        WalletNetwork::Signet => "signet",
    }
}
