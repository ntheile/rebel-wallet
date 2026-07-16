use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use bark::actions::lightning::pay::LightningSendState;
use bark::ark::lightning::PaymentHash;
use bark::lightning_invoice::Bolt11Invoice;
use bark::movement::{Movement, MovementStatus};
use bark::persist::models::LightningReceive;
use bip39::Mnemonic;
use bitcoin::Amount;
use nostr::nips::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceResponse, Method, NIP47Error, PayInvoiceResponse, Request,
    RequestParams, Response, ResponseResult, TransactionState, TransactionType,
};
use nostr_sdk::prelude::{
    nip04, Client as NostrClient, ClientNotification, Event, EventBuilder, EventId, Filter,
    FinalizeEvent, JsonUtil, Keys, Kind, PublicKey, StreamExt, Tag, Timestamp, ToBech32,
};
use tokio_util::sync::CancellationToken;

use crate::nostr_support::public_key_from_npub_or_hex;
use crate::persistence::ServerConfig;
use crate::wallet::{open_bark_wallet, WalletOpenMode};
use crate::{NwcBudgetInterval, NwcConnection, NwcPermission, NwcWakeRequest, WalletNetwork};

const NWC_EXTENSION_PAYMENT_SETTLE_TIMEOUT: Duration = Duration::from_secs(22);
const NWC_EXTENSION_PAYMENT_POLL_INTERVAL: Duration = Duration::from_millis(750);
const NWC_EXTENSION_TOTAL_BUDGET: Duration = Duration::from_secs(26);
const NWC_EXTENSION_RELAY_LINGER: Duration = Duration::from_millis(500);
const NWC_EXTENSION_MIN_LINGER_BUDGET: Duration = Duration::from_millis(250);
const NWC_EXTENSION_MAX_LINGER_EVENTS: usize = 12;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct NwcWakeSnapshot {
    pub(crate) version: u32,
    pub(crate) nostr_secret: String,
    pub(crate) wallet_seed: Option<String>,
    pub(crate) wallet_data_dir: Option<String>,
    pub(crate) balance_sat: u64,
    pub(crate) network: WalletNetwork,
    pub(crate) connections: Vec<NwcConnection>,
}

#[derive(Clone, Debug)]
pub(crate) struct NwcServiceContext {
    pub(crate) keys: Keys,
    pub(crate) wallet_seed: Option<String>,
    pub(crate) wallet_data_dir: Option<PathBuf>,
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
    pub(crate) amount_sat: u64,
    pub(crate) processed_at: u64,
    pub(crate) updated_connections: Option<Vec<NwcConnection>>,
    pub(crate) updated_snapshot_json: Option<String>,
    pub(crate) processed_event_ids: Vec<String>,
}

pub(crate) fn build_nwc_wake_snapshot(
    nostr_secret: String,
    wallet_seed: Option<String>,
    wallet_data_dir: String,
    balance_sat: u64,
    network: WalletNetwork,
    mut connections: Vec<NwcConnection>,
) -> anyhow::Result<String> {
    for connection in &mut connections {
        connection.uri.clear();
    }

    let snapshot = NwcWakeSnapshot {
        version: 2,
        nostr_secret,
        wallet_seed,
        wallet_data_dir: Some(wallet_data_dir),
        balance_sat,
        network,
        connections,
    };
    serde_json::to_string(&snapshot).context("failed to encode NWC wake snapshot")
}

pub(crate) async fn process_nwc_wake_from_snapshot(
    snapshot_json: String,
    wake: NwcWakeRequest,
) -> anyhow::Result<NwcProcessedWake> {
    let (context, _) = context_from_snapshot(snapshot_json)?;
    validate_wallet_service_pubkey(&wake.wallet_service_pubkey, &context.keys)?;
    let event = fetch_nwc_request_event(&wake).await?;
    process_nwc_event_with_extension_policy(wake, event, context).await
}

pub(crate) async fn process_nwc_event_from_snapshot(
    snapshot_json: String,
    wake: NwcWakeRequest,
    event_json: String,
) -> anyhow::Result<NwcProcessedWake> {
    let (context, _) = context_from_snapshot(snapshot_json)?;
    validate_wallet_service_pubkey(&wake.wallet_service_pubkey, &context.keys)?;
    let event = Event::from_json(event_json).context("failed to parse embedded NWC event")?;
    event.verify().context("invalid embedded NWC event")?;
    if event.id.to_hex() != wake.event_id {
        anyhow::bail!(
            "embedded NWC event id mismatch: expected {}, got {}",
            wake.event_id,
            event.id.to_hex()
        );
    }
    if event.kind != Kind::WalletConnectRequest {
        anyhow::bail!("embedded NWC event was not a wallet connect request");
    }

    process_nwc_event_with_extension_policy(wake, event, context).await
}

fn context_from_snapshot(
    snapshot_json: String,
) -> anyhow::Result<(NwcServiceContext, NwcWakeSnapshot)> {
    let snapshot: NwcWakeSnapshot =
        serde_json::from_str(&snapshot_json).context("failed to parse NWC wake snapshot")?;
    if snapshot.version != 1 {
        if snapshot.version != 2 {
            anyhow::bail!("unsupported NWC wake snapshot version {}", snapshot.version);
        }
    }
    let keys = Keys::parse(&snapshot.nostr_secret).context("invalid NWC wake snapshot key")?;
    let context = NwcServiceContext {
        keys,
        wallet_seed: snapshot.wallet_seed.clone(),
        wallet_data_dir: snapshot.wallet_data_dir.clone().map(PathBuf::from),
        balance_sat: snapshot.balance_sat,
        network: snapshot.network,
        connections: snapshot.connections.clone(),
    };

    Ok((context, snapshot))
}

async fn process_nwc_event_with_extension_policy(
    wake: NwcWakeRequest,
    event: Event,
    context: NwcServiceContext,
) -> anyhow::Result<NwcProcessedWake> {
    let extension_deadline = Instant::now() + NWC_EXTENSION_TOTAL_BUDGET;
    let request = decrypt_nwc_request(&event, &context.keys)?;
    match request.params {
        RequestParams::GetInfo
        | RequestParams::GetBalance
        | RequestParams::MakeInvoice(_)
        | RequestParams::PayInvoice(_)
        | RequestParams::LookupInvoice(_)
        | RequestParams::ListTransactions(_) => {}
        _ => {
            anyhow::bail!(
                "NSE wake responder skipped {} request; queued for app",
                request.method.as_str()
            );
        }
    }

    process_nwc_request_event_inner(wake, event, context, Some(extension_deadline)).await
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
    process_nwc_request_event_inner(wake, event, context, None).await
}

async fn process_nwc_request_event_inner(
    wake: NwcWakeRequest,
    event: Event,
    context: NwcServiceContext,
    extension_deadline: Option<Instant>,
) -> anyhow::Result<NwcProcessedWake> {
    let response_started_at = Instant::now();
    let relay = wake.relay.clone();
    let relay_connect_started_at = Instant::now();
    let client = client_for_relay(&relay).await?;
    let relay_connect_ms = relay_connect_started_at.elapsed().as_millis();

    let result = async {
        let mut processed =
            process_nwc_request_event_with_client(&client, wake, event, &context).await?;
        processed.status = format!(
            "Response published in {} ms; relay_connect_ms={relay_connect_ms}; {}",
            response_started_at.elapsed().as_millis(),
            processed.status
        );

        if let Some(deadline) = extension_deadline {
            linger_for_followup_nwc_requests(&client, &relay, context, &mut processed, deadline)
                .await;
        }

        Ok(processed)
    }
    .await;

    client.shutdown().await;
    result
}

async fn process_nwc_request_event_with_client(
    client: &NostrClient,
    wake: NwcWakeRequest,
    event: Event,
    context: &NwcServiceContext,
) -> anyhow::Result<NwcProcessedWake> {
    let request_started_at = Instant::now();
    let connection = authorized_connection(&event, &context)?;
    let request = decrypt_nwc_request(&event, &context.keys)?;
    let request_auth_ms = request_started_at.elapsed().as_millis();
    let method = request.method.as_str().to_string();
    let response_started_at = Instant::now();
    let (response, amount_sat, updated_connections, request_timings) =
        response_for_request(&request, &context, connection).await;
    let request_handler_ms = response_started_at.elapsed().as_millis();
    let response_build_started_at = Instant::now();
    let updated_snapshot_json = updated_connections.clone().and_then(|connections| {
        snapshot_json_from_context(context, connections)
            .map_err(|e| eprintln!("failed to encode updated NWC snapshot: {e:#}"))
            .ok()
    });
    let response_event = build_nwc_response_event(&event, response, &context.keys)?;
    let response_build_ms = response_build_started_at.elapsed().as_millis();

    let relay_publish_started_at = Instant::now();
    client
        .send_event(&response_event)
        .to([wake.relay.as_str()])
        .await
        .context("failed to publish NWC response")?;
    let relay_publish_ms = relay_publish_started_at.elapsed().as_millis();

    let request_timings = request_timings
        .map(|timings| format!("; {timings}"))
        .unwrap_or_default();

    let processed_event_id = wake.event_id.clone();
    Ok(NwcProcessedWake {
        wake,
        client_pubkey: event.pubkey.to_hex(),
        method,
        status: format!(
            "request_auth_ms={request_auth_ms}; request_handler_ms={request_handler_ms}{request_timings}; response_build_ms={response_build_ms}; relay_publish_ms={relay_publish_ms}"
        ),
        amount_sat,
        processed_at: crate::time::now_unix(),
        updated_connections,
        updated_snapshot_json,
        processed_event_ids: vec![processed_event_id],
    })
}

async fn linger_for_followup_nwc_requests(
    client: &NostrClient,
    relay: &str,
    mut context: NwcServiceContext,
    processed: &mut NwcProcessedWake,
    deadline: Instant,
) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining < NWC_EXTENSION_MIN_LINGER_BUDGET {
        return;
    }

    let authors = authorized_client_pubkeys(&context);
    if authors.is_empty() {
        return;
    }

    let filter = Filter::new()
        .kind(Kind::WalletConnectRequest)
        .pubkey(context.keys.public_key())
        .authors(authors)
        .since(Timestamp::now() - Duration::from_secs(2))
        .limit(16);
    let mut notifications = client.notifications();
    let subscription = match client.subscribe(vec![(relay, vec![filter])]).await {
        Ok(output) => output,
        Err(_) => return,
    };

    let linger_until = Instant::now() + remaining.min(NWC_EXTENSION_RELAY_LINGER);
    let mut seen_event_ids = HashSet::from([processed.wake.event_id.clone()]);
    let mut followups = 0usize;

    while Instant::now() < linger_until && followups < NWC_EXTENSION_MAX_LINGER_EVENTS {
        let wait_for = linger_until.saturating_duration_since(Instant::now());
        if wait_for.is_zero() {
            break;
        }

        let Some(notification) = tokio::time::timeout(wait_for, notifications.next())
            .await
            .ok()
            .flatten()
        else {
            break;
        };

        let ClientNotification::Event {
            subscription_id,
            event,
            ..
        } = notification
        else {
            continue;
        };

        if &subscription_id != subscription.id() || event.kind != Kind::WalletConnectRequest {
            continue;
        }

        let event_id = event.id.to_hex();
        if !seen_event_ids.insert(event_id.clone()) {
            continue;
        }

        if event.verify().is_err() || authorized_connection(&event, &context).is_err() {
            continue;
        }

        let wake = NwcWakeRequest {
            relay: relay.to_string(),
            event_id,
            wallet_service_pubkey: context.keys.public_key().to_hex(),
            received_at: crate::time::now_unix(),
        };

        if let Ok(followup) =
            process_nwc_request_event_with_client(client, wake, *event, &context).await
        {
            if let Some(updated_connections) = followup.updated_connections.clone() {
                context.connections = updated_connections.clone();
                processed.updated_connections = Some(updated_connections);
            }
            if followup.updated_snapshot_json.is_some() {
                processed.updated_snapshot_json = followup.updated_snapshot_json;
            }
            processed
                .processed_event_ids
                .extend(followup.processed_event_ids);
            followups += 1;
        }
    }

    let _ = client.unsubscribe(subscription.id()).await;
    if followups > 0 {
        processed.status = format!(
            "{}; linger processed {followups} follow-up request{}",
            processed.status,
            if followups == 1 { "" } else { "s" }
        );
    }
}

fn authorized_client_pubkeys(context: &NwcServiceContext) -> Vec<PublicKey> {
    context
        .connections
        .iter()
        .filter_map(|connection| public_key_from_npub_or_hex(&connection.client_pubkey).ok())
        .collect()
}

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

fn authorized_connection<'a>(
    event: &Event,
    context: &'a NwcServiceContext,
) -> anyhow::Result<&'a NwcConnection> {
    let client_pubkey = event.pubkey.to_hex();
    let connection = context
        .connections
        .iter()
        .find(|connection| connection.client_pubkey == client_pubkey)
        .ok_or_else(|| anyhow!("NWC request client is not authorized: {client_pubkey}"))?;
    if connection_is_expired(connection, crate::time::now_unix()) {
        anyhow::bail!("NWC connection has expired");
    }
    Ok(connection)
}

fn connection_is_expired(connection: &NwcConnection, now: u64) -> bool {
    connection
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
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

async fn response_for_request(
    request: &Request,
    context: &NwcServiceContext,
    connection: &NwcConnection,
) -> (Response, u64, Option<Vec<NwcConnection>>, Option<String>) {
    let permission = permission_for_request(&request.params);
    if !connection.allows_permission(permission) {
        return (
            error_response(
                permission.to_method(),
                ErrorCode::Restricted,
                "This NWC connection is not allowed to use this method.",
            ),
            0,
            None,
            None,
        );
    }

    let (response, amount_sat, updated_connections, request_timings) = match &request.params {
        RequestParams::GetInfo => (
            Response {
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
            0,
            None,
            None,
        ),
        RequestParams::GetBalance => match get_balance_response(context).await {
            Ok(response) => (response, 0, None, None),
            Err(e) => (
                error_response(
                    Method::GetBalance,
                    ErrorCode::Internal,
                    &format!("Could not read wallet balance: {e:#}"),
                ),
                0,
                None,
                None,
            ),
        },
        RequestParams::MakeInvoice(params) => match make_invoice_response(params, context).await {
            Ok(response) => (response, 0, None, None),
            Err(e) => (
                error_response(
                    Method::MakeInvoice,
                    ErrorCode::Internal,
                    &format!("Could not create invoice: {e:#}"),
                ),
                0,
                None,
                None,
            ),
        },
        RequestParams::PayInvoice(params) => {
            match pay_invoice_response(params, context, connection).await {
                Ok((response, amount_sat, updated_connections, timings)) => (
                    response,
                    amount_sat,
                    Some(updated_connections),
                    Some(timings),
                ),
                Err(e) => (
                    error_response(
                        Method::PayInvoice,
                        ErrorCode::PaymentFailed,
                        &format!("Could not pay invoice: {e:#}"),
                    ),
                    0,
                    None,
                    None,
                ),
            }
        }
        RequestParams::LookupInvoice(params) => {
            match lookup_invoice_response(params, context).await {
                Ok(response) => (response, 0, None, None),
                Err(e) => (
                    error_response(
                        Method::LookupInvoice,
                        ErrorCode::NotFound,
                        &format!("Could not find invoice: {e:#}"),
                    ),
                    0,
                    None,
                    None,
                ),
            }
        }
        RequestParams::ListTransactions(params) => {
            match list_transactions_response(params, context).await {
                Ok(response) => (response, 0, None, None),
                Err(e) => (
                    error_response(
                        Method::ListTransactions,
                        ErrorCode::Internal,
                        &format!("Could not list wallet transactions: {e:#}"),
                    ),
                    0,
                    None,
                    None,
                ),
            }
        }
        _ => (
            not_implemented_response(request.method.clone()),
            0,
            None,
            None,
        ),
    };

    (response, amount_sat, updated_connections, request_timings)
}

async fn get_balance_response(context: &NwcServiceContext) -> anyhow::Result<Response> {
    Ok(Response {
        result_type: Method::GetBalance,
        error: None,
        result: Some(ResponseResult::GetBalance(GetBalanceResponse {
            balance: context.balance_sat.saturating_mul(1_000),
        })),
    })
}

async fn lookup_invoice_response(
    params: &LookupInvoiceRequest,
    context: &NwcServiceContext,
) -> anyhow::Result<Response> {
    let payment_hash = lookup_payment_hash(params)?;
    let wallet = open_wallet_for_extension(context).await?;
    sync_wallet_for_nwc(&wallet).await;

    if let Some(receive) = wallet
        .lightning_receive_status(payment_hash)
        .await
        .context("Bark could not read Lightning receive state")?
    {
        return Ok(Response {
            result_type: Method::LookupInvoice,
            error: None,
            result: Some(ResponseResult::LookupInvoice(
                transaction_from_lightning_receive(&receive),
            )),
        });
    }

    if let Some(transaction) = wallet
        .history()
        .await
        .context("Bark could not read wallet history")?
        .iter()
        .find(|movement| movement.lightning_payment_hash() == Some(payment_hash))
        .and_then(transaction_from_movement)
    {
        return Ok(Response {
            result_type: Method::LookupInvoice,
            error: None,
            result: Some(ResponseResult::LookupInvoice(transaction)),
        });
    }

    match wallet
        .lightning_send_state(payment_hash)
        .await
        .context("Bark could not read Lightning send state")?
    {
        LightningSendState::Paid(paid) => Ok(Response {
            result_type: Method::LookupInvoice,
            error: None,
            result: Some(ResponseResult::LookupInvoice(LookupInvoiceResponse {
                transaction_type: Some(TransactionType::Outgoing),
                state: Some(TransactionState::Settled),
                invoice: params.invoice.clone(),
                description: None,
                description_hash: None,
                preimage: Some(paid.preimage.to_string()),
                payment_hash: payment_hash.to_string(),
                amount: params
                    .invoice
                    .as_deref()
                    .and_then(invoice_amount_msat_from_str)
                    .unwrap_or(0),
                fees_paid: 0,
                created_at: Timestamp::from_secs(paid.paid_at.timestamp().max(0) as u64),
                expires_at: None,
                settled_at: Some(Timestamp::from_secs(paid.paid_at.timestamp().max(0) as u64)),
                metadata: None,
            })),
        }),
        LightningSendState::InProgress(send) => Ok(Response {
            result_type: Method::LookupInvoice,
            error: None,
            result: Some(ResponseResult::LookupInvoice(
                transaction_from_pending_send(&send),
            )),
        }),
        LightningSendState::Unknown => anyhow::bail!("invoice was not found in Bark"),
    }
}

async fn list_transactions_response(
    params: &ListTransactionsRequest,
    context: &NwcServiceContext,
) -> anyhow::Result<Response> {
    let wallet = open_wallet_for_extension(context).await?;
    sync_wallet_for_nwc(&wallet).await;

    let mut transactions = wallet
        .history()
        .await
        .context("Bark could not read wallet history")?
        .iter()
        .filter_map(transaction_from_movement)
        .collect::<Vec<_>>();

    for receive in wallet
        .pending_lightning_receives()
        .await
        .context("Bark could not read pending Lightning receives")?
    {
        transactions.push(transaction_from_lightning_receive(&receive));
    }

    for send in wallet
        .pending_lightning_sends()
        .await
        .context("Bark could not read pending Lightning sends")?
    {
        transactions.push(transaction_from_pending_send(&send));
    }

    transactions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    transactions.retain(|transaction| transaction_matches_list_request(transaction, params));

    let offset = params.offset.unwrap_or(0) as usize;
    let limit = params.limit.map(|limit| limit as usize);
    let transactions = transactions
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect();

    Ok(Response {
        result_type: Method::ListTransactions,
        error: None,
        result: Some(ResponseResult::ListTransactions(transactions)),
    })
}

async fn make_invoice_response(
    params: &nostr::nips::nip47::MakeInvoiceRequest,
    context: &NwcServiceContext,
) -> anyhow::Result<Response> {
    let wallet = open_wallet_for_extension(context).await?;
    let amount_sat = msats_to_exact_sats(params.amount)
        .context("Rebel Wallet only supports whole-sat NWC invoices")?;
    if amount_sat == 0 {
        anyhow::bail!("invoice amount must be greater than zero");
    }
    let invoice = wallet
        .bolt11_invoice(Amount::from_sat(amount_sat), params.description.clone())
        .await
        .context("Bark could not create a Lightning invoice")?;
    let payment_hash = invoice.payment_hash().to_string();

    Ok(Response {
        result_type: Method::MakeInvoice,
        error: None,
        result: Some(ResponseResult::MakeInvoice(MakeInvoiceResponse {
            invoice: invoice.to_string(),
            payment_hash: Some(payment_hash),
            description: params.description.clone(),
            description_hash: params.description_hash.clone(),
            preimage: None,
            amount: Some(params.amount),
            created_at: None,
            expires_at: None,
        })),
    })
}

async fn pay_invoice_response(
    params: &nostr::nips::nip47::PayInvoiceRequest,
    context: &NwcServiceContext,
    connection: &NwcConnection,
) -> anyhow::Result<(Response, u64, Vec<NwcConnection>, String)> {
    let pay_started_at = Instant::now();
    let mut renewed_connection = connection.clone();
    renew_budget_if_due(&mut renewed_connection, crate::time::now_unix());
    let wallet_open_started_at = Instant::now();
    let wallet = open_wallet_for_extension(context).await?;
    let wallet_open_ms = wallet_open_started_at.elapsed().as_millis();
    let invoice_validation_started_at = Instant::now();
    let invoice = Bolt11Invoice::from_str(&params.invoice).context("invalid Lightning invoice")?;
    let amount_sat = pay_invoice_amount_sat(&invoice, params.amount)?;
    enforce_budget(&renewed_connection, amount_sat)?;
    let payment_hash: PaymentHash = (*invoice.payment_hash()).into();

    let user_amount = params
        .amount
        .map(msats_to_exact_sats)
        .transpose()
        .context("Rebel Wallet only supports whole-sat NWC payment amounts")?
        .filter(|amount| *amount > 0)
        .map(Amount::from_sat);
    let invoice_validation_ms = invoice_validation_started_at.elapsed().as_millis();

    // The full app keeps Bark's mailbox stream alive. The NSE opens a short-lived
    // wallet, so mirror that behavior while paying to receive the preimage as soon
    // as the Ark server finishes instead of relying only on status polling.
    let mailbox_shutdown = CancellationToken::new();
    let mailbox_task = {
        let wallet = wallet.clone();
        let shutdown = mailbox_shutdown.clone();
        tokio::spawn(async move {
            wallet
                .subscribe_process_mailbox_messages(None, shutdown)
                .await
        })
    };
    let payment_attempt: anyhow::Result<PayInvoiceAttempt> = async {
        let initial_state_started_at = Instant::now();
        let initial_state = wallet
            .lightning_send_state(payment_hash)
            .await
            .context("could not read invoice state")?;
        let initial_state_ms = initial_state_started_at.elapsed().as_millis();
        let (initial_state_name, payment_start_ms, settlement) = match initial_state {
            LightningSendState::Paid(paid) => (
                "paid",
                0,
                PaidInvoiceWait {
                    preimage: paid.preimage.to_string(),
                    elapsed_ms: 0,
                    checks: 0,
                    check_io_ms: 0,
                },
            ),
            LightningSendState::InProgress(_) => (
                "in_progress",
                0,
                wait_for_paid_invoice(&wallet, payment_hash).await?,
            ),
            LightningSendState::Unknown => {
                let payment_start_started_at = Instant::now();
                wallet
                    .pay_lightning_invoice(invoice.clone(), user_amount, false)
                    .await
                    .context("Bark payment failed")?;
                let payment_start_ms = payment_start_started_at.elapsed().as_millis();
                (
                    "unknown",
                    payment_start_ms,
                    wait_for_paid_invoice(&wallet, payment_hash).await?,
                )
            }
        };

        Ok(PayInvoiceAttempt {
            initial_state_name,
            initial_state_ms,
            payment_start_ms,
            settlement,
        })
    }
    .await;
    let mailbox_status = if mailbox_task.is_finished() {
        "stopped"
    } else {
        "active"
    };
    mailbox_shutdown.cancel();
    mailbox_task.abort();
    let _ = mailbox_task.await;
    let payment_attempt = payment_attempt?;
    let settlement = payment_attempt.settlement;
    let preimage = settlement.preimage;

    let mut updated_connections = context.connections.clone();
    if let Some(updated) = updated_connections
        .iter_mut()
        .find(|candidate| candidate.id == connection.id)
    {
        updated.spent_sat = renewed_connection.spent_sat.saturating_add(amount_sat);
        updated.spent_display = crate::state::format_sats(updated.spent_sat);
        updated.budget_period_started_at = renewed_connection.budget_period_started_at;
        updated.last_used_at = Some(crate::time::now_unix());
    }

    let timings = format!(
        "pay_phases wallet_open_ms={wallet_open_ms} invoice_validation_ms={invoice_validation_ms} initial_state={} initial_state_ms={} payment_start_ms={} settlement_ms={} settlement_checks={} settlement_check_io_ms={} mailbox={mailbox_status} pay_total_ms={}",
        payment_attempt.initial_state_name,
        payment_attempt.initial_state_ms,
        payment_attempt.payment_start_ms,
        settlement.elapsed_ms,
        settlement.checks,
        settlement.check_io_ms,
        pay_started_at.elapsed().as_millis()
    );

    Ok((
        Response {
            result_type: Method::PayInvoice,
            error: None,
            result: Some(ResponseResult::PayInvoice(PayInvoiceResponse {
                preimage,
                fees_paid: None,
            })),
        },
        amount_sat,
        updated_connections,
        timings,
    ))
}

async fn sync_wallet_for_nwc(wallet: &bark::Wallet) {
    let _ = tokio::time::timeout(Duration::from_secs(8), wallet.sync()).await;
}

fn lookup_payment_hash(params: &LookupInvoiceRequest) -> anyhow::Result<PaymentHash> {
    if let Some(invoice) = params.invoice.as_deref() {
        let invoice = Bolt11Invoice::from_str(invoice).context("invalid Lightning invoice")?;
        return Ok((*invoice.payment_hash()).into());
    }

    let payment_hash = params
        .payment_hash
        .as_deref()
        .ok_or_else(|| anyhow!("lookup_invoice requires invoice or payment_hash"))?;
    PaymentHash::from_str(payment_hash).context("invalid payment hash")
}

fn transaction_from_movement(movement: &Movement) -> Option<LookupInvoiceResponse> {
    let payment_hash = movement.lightning_payment_hash()?;
    let transaction_type = movement_transaction_type(movement);
    let amount_sat = movement_amount_sat(movement, transaction_type);
    let invoice = movement.lightning_invoice().map(ToString::to_string);
    let preimage = movement_preimage(movement);
    let created_at = timestamp_from_chrono(movement.time.created_at);
    let settled_at = match movement.status {
        MovementStatus::Successful => Some(timestamp_from_chrono(
            movement
                .time
                .completed_at
                .unwrap_or(movement.time.updated_at),
        )),
        _ => None,
    };

    Some(LookupInvoiceResponse {
        transaction_type: Some(transaction_type),
        state: Some(transaction_state_from_movement(movement.status)),
        invoice,
        description: None,
        description_hash: None,
        preimage,
        payment_hash: payment_hash.to_string(),
        amount: amount_sat.saturating_mul(1_000),
        fees_paid: movement.offchain_fee.to_sat().saturating_mul(1_000),
        created_at,
        expires_at: movement
            .lightning_invoice()
            .and_then(invoice_expires_at)
            .map(Timestamp::from_secs),
        settled_at,
        metadata: Some(serde_json::Value::Object(movement.metadata.clone())),
    })
}

fn transaction_from_lightning_receive(receive: &LightningReceive) -> LookupInvoiceResponse {
    let created_at = Timestamp::from_secs(receive.invoice.duration_since_epoch().as_secs());
    let settled_at = receive
        .finished_at
        .or(receive.preimage_revealed_at)
        .map(timestamp_from_chrono);

    LookupInvoiceResponse {
        transaction_type: Some(TransactionType::Incoming),
        state: Some(if receive.finished_at.is_some() {
            TransactionState::Settled
        } else {
            TransactionState::Pending
        }),
        invoice: Some(receive.invoice.to_string()),
        description: None,
        description_hash: None,
        preimage: receive
            .preimage_revealed_at
            .map(|_| receive.payment_preimage.to_string()),
        payment_hash: receive.payment_hash.to_string(),
        amount: receive.invoice.amount_milli_satoshis().unwrap_or(0),
        fees_paid: 0,
        created_at,
        expires_at: receive
            .invoice
            .expires_at()
            .map(|expiry| Timestamp::from_secs(expiry.as_secs())),
        settled_at,
        metadata: None,
    }
}

fn transaction_from_pending_send(
    send: &bark::actions::lightning::pay::LightningSend,
) -> LookupInvoiceResponse {
    LookupInvoiceResponse {
        transaction_type: Some(TransactionType::Outgoing),
        state: Some(TransactionState::Pending),
        invoice: Some(send.invoice.to_string()),
        description: None,
        description_hash: None,
        preimage: None,
        payment_hash: send.invoice.payment_hash().to_string(),
        amount: send.payment_amount.to_sat().saturating_mul(1_000),
        fees_paid: send.fee.to_sat().saturating_mul(1_000),
        created_at: Timestamp::now(),
        expires_at: None,
        settled_at: None,
        metadata: None,
    }
}

fn movement_transaction_type(movement: &Movement) -> TransactionType {
    if !movement.received_on.is_empty() && movement.sent_to.is_empty() {
        TransactionType::Incoming
    } else if movement.effective_balance.to_sat() >= 0 {
        TransactionType::Incoming
    } else {
        TransactionType::Outgoing
    }
}

fn movement_amount_sat(movement: &Movement, transaction_type: TransactionType) -> u64 {
    let destinations = match transaction_type {
        TransactionType::Incoming => &movement.received_on,
        TransactionType::Outgoing => &movement.sent_to,
    };
    let destination_total = destinations
        .iter()
        .map(|destination| destination.amount.to_sat())
        .sum::<u64>();
    if destination_total > 0 {
        return destination_total;
    }

    movement.effective_balance.to_sat().unsigned_abs()
}

fn transaction_state_from_movement(status: MovementStatus) -> TransactionState {
    match status {
        MovementStatus::Pending => TransactionState::Pending,
        MovementStatus::Successful => TransactionState::Settled,
        MovementStatus::Failed | MovementStatus::Canceled => TransactionState::Failed,
    }
}

fn movement_preimage(movement: &Movement) -> Option<String> {
    movement
        .metadata
        .get("payment_preimage")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn invoice_expires_at(invoice: &bark::ark::lightning::Invoice) -> Option<u64> {
    match invoice {
        bark::ark::lightning::Invoice::Bolt11(invoice) => {
            invoice.expires_at().map(|expiry| expiry.as_secs())
        }
        bark::ark::lightning::Invoice::Bolt12(_) => None,
    }
}

fn invoice_amount_msat_from_str(invoice: &str) -> Option<u64> {
    Bolt11Invoice::from_str(invoice)
        .ok()
        .and_then(|invoice| invoice.amount_milli_satoshis())
}

fn timestamp_from_chrono(timestamp: chrono::DateTime<chrono::Local>) -> Timestamp {
    Timestamp::from_secs(timestamp.timestamp().max(0) as u64)
}

fn transaction_matches_list_request(
    transaction: &LookupInvoiceResponse,
    params: &ListTransactionsRequest,
) -> bool {
    if let Some(from) = params.from {
        if transaction.created_at < from {
            return false;
        }
    }
    if let Some(until) = params.until {
        if transaction.created_at > until {
            return false;
        }
    }
    if let Some(transaction_type) = params.transaction_type {
        if transaction.transaction_type != Some(transaction_type) {
            return false;
        }
    }
    if !params.unpaid.unwrap_or(true) && transaction.state != Some(TransactionState::Settled) {
        return false;
    }
    true
}

struct PaidInvoiceWait {
    preimage: String,
    elapsed_ms: u128,
    checks: u32,
    check_io_ms: u128,
}

struct PayInvoiceAttempt {
    initial_state_name: &'static str,
    initial_state_ms: u128,
    payment_start_ms: u128,
    settlement: PaidInvoiceWait,
}

async fn wait_for_paid_invoice(
    wallet: &bark::Wallet,
    payment_hash: PaymentHash,
) -> anyhow::Result<PaidInvoiceWait> {
    let started_at = Instant::now();
    let deadline = Instant::now() + NWC_EXTENSION_PAYMENT_SETTLE_TIMEOUT;
    let mut checks = 0u32;
    let mut check_io_ms = 0u128;

    loop {
        let check_started_at = Instant::now();
        let state = wallet
            .check_lightning_payment(payment_hash, false)
            .await
            .context("could not drive paid invoice state")?;
        check_io_ms += check_started_at.elapsed().as_millis();
        checks += 1;

        match state {
            LightningSendState::Paid(paid) => {
                return Ok(PaidInvoiceWait {
                    preimage: paid.preimage.to_string(),
                    elapsed_ms: started_at.elapsed().as_millis(),
                    checks,
                    check_io_ms,
                })
            }
            LightningSendState::Unknown => anyhow::bail!("paid invoice record was not found"),
            LightningSendState::InProgress(_) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("payment is still in progress after extension wait window")
                }
                tokio::time::sleep(NWC_EXTENSION_PAYMENT_POLL_INTERVAL).await;
            }
        }
    }
}

fn not_implemented_response(method: Method) -> Response {
    error_response(
            method,
            ErrorCode::NotImplemented,
            "This NWC method is allowed for this connection but is not implemented in Rebel Wallet yet.",
        )
}

async fn open_wallet_for_extension(context: &NwcServiceContext) -> anyhow::Result<bark::Wallet> {
    let seed = context
        .wallet_seed
        .as_deref()
        .ok_or_else(|| anyhow!("NWC wake snapshot does not include wallet seed"))?;
    let data_dir = context
        .wallet_data_dir
        .clone()
        .ok_or_else(|| anyhow!("NWC wake snapshot does not include wallet data directory"))?;
    let mnemonic = Mnemonic::from_str(seed).context("invalid wallet seed in NWC wake snapshot")?;
    open_bark_wallet(
        data_dir,
        &mnemonic,
        WalletOpenMode::OpenOrCreate,
        ServerConfig::for_network(context.network),
    )
    .await
}

fn pay_invoice_amount_sat(
    invoice: &Bolt11Invoice,
    amount_msat: Option<u64>,
) -> anyhow::Result<u64> {
    if let Some(amount_msat) = amount_msat {
        return msats_to_exact_sats(amount_msat)
            .context("Rebel Wallet only supports whole-sat NWC payment amounts");
    }

    let invoice_msat = invoice
        .amount_milli_satoshis()
        .ok_or_else(|| anyhow!("invoice does not include an amount"))?;
    msats_to_exact_sats(invoice_msat)
        .context("Rebel Wallet only supports whole-sat NWC invoice amounts")
}

fn msats_to_exact_sats(amount_msat: u64) -> anyhow::Result<u64> {
    if amount_msat % 1_000 != 0 {
        anyhow::bail!("amount must be a whole number of sats");
    }
    Ok(amount_msat / 1_000)
}

fn enforce_budget(connection: &NwcConnection, amount_sat: u64) -> anyhow::Result<()> {
    if connection.budget_sat == 0 {
        anyhow::bail!("NWC connection has no payment budget");
    }
    let next_spent = connection.spent_sat.saturating_add(amount_sat);
    if next_spent > connection.budget_sat {
        anyhow::bail!(
            "NWC payment would exceed budget: {} > {} sats",
            next_spent,
            connection.budget_sat
        );
    }
    Ok(())
}

fn renew_budget_if_due(connection: &mut NwcConnection, now: u64) {
    let period_seconds = match connection.budget_interval {
        NwcBudgetInterval::Never => return,
        NwcBudgetInterval::Hourly => 60 * 60,
        NwcBudgetInterval::Daily => 24 * 60 * 60,
        NwcBudgetInterval::Weekly => 7 * 24 * 60 * 60,
        NwcBudgetInterval::Monthly => 30 * 24 * 60 * 60,
        NwcBudgetInterval::Yearly => 365 * 24 * 60 * 60,
    };
    let period_started_at = if connection.budget_period_started_at == 0 {
        connection.created_at
    } else {
        connection.budget_period_started_at
    };
    let elapsed = now.saturating_sub(period_started_at);
    if elapsed < period_seconds {
        connection.budget_period_started_at = period_started_at;
        return;
    }

    let completed_periods = elapsed / period_seconds;
    connection.budget_period_started_at =
        period_started_at.saturating_add(completed_periods.saturating_mul(period_seconds));
    connection.spent_sat = 0;
    connection.spent_display = crate::state::format_sats(0);
}

fn snapshot_json_from_context(
    context: &NwcServiceContext,
    mut connections: Vec<NwcConnection>,
) -> anyhow::Result<String> {
    for connection in &mut connections {
        connection.uri.clear();
    }
    let snapshot = NwcWakeSnapshot {
        version: 2,
        nostr_secret: context
            .keys
            .secret_key()
            .to_bech32()
            .context("failed to encode NWC service secret")?,
        wallet_seed: context.wallet_seed.clone(),
        wallet_data_dir: context
            .wallet_data_dir
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        balance_sat: context.balance_sat,
        network: context.network,
        connections,
    };
    serde_json::to_string(&snapshot).context("failed to encode updated NWC wake snapshot")
}

fn connection_methods(connection: &NwcConnection) -> Vec<Method> {
    connection
        .enabled_permissions()
        .into_iter()
        .filter(|permission| NwcPermission::IMPLEMENTED.contains(permission))
        .map(|permission| permission.to_method())
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_connection(interval: NwcBudgetInterval) -> NwcConnection {
        NwcConnection {
            id: "test".to_string(),
            name: "Test".to_string(),
            icon_url: None,
            relay: "wss://relay.example.com".to_string(),
            uri: String::new(),
            wallet_managed_secret: false,
            service_pubkey: Keys::generate().public_key().to_hex(),
            client_pubkey: Keys::generate().public_key().to_hex(),
            budget_sat: 1_000,
            spent_sat: 500,
            budget_display: "1,000 sats".to_string(),
            spent_display: "500 sats".to_string(),
            budget_interval: interval,
            budget_interval_display: interval.display_name().to_string(),
            permissions: NwcPermission::IMPLEMENTED.to_vec(),
            permissions_configured: true,
            allow_get_balance: true,
            allow_pay_invoice: true,
            created_at: 1_000,
            last_used_at: None,
            expires_at: None,
            budget_period_started_at: 1_000,
            pending_info_event_relays: Vec::new(),
        }
    }

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

    #[test]
    fn expired_connection_is_rejected_at_the_expiration_boundary() {
        let mut connection = test_connection(NwcBudgetInterval::Never);
        connection.expires_at = Some(2_000);

        assert!(!connection_is_expired(&connection, 1_999));
        assert!(connection_is_expired(&connection, 2_000));
    }

    #[test]
    fn daily_budget_renews_after_completed_periods() {
        let mut connection = test_connection(NwcBudgetInterval::Daily);
        renew_budget_if_due(&mut connection, 1_000 + 2 * 24 * 60 * 60 + 60);

        assert_eq!(connection.spent_sat, 0);
        assert_eq!(
            connection.budget_period_started_at,
            1_000 + 2 * 24 * 60 * 60
        );
    }

    #[test]
    fn never_budget_does_not_renew() {
        let mut connection = test_connection(NwcBudgetInterval::Never);
        renew_budget_if_due(&mut connection, u64::MAX);

        assert_eq!(connection.spent_sat, 500);
        assert_eq!(connection.budget_period_started_at, 1_000);
    }
}
