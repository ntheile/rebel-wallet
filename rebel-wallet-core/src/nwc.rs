use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use bark::actions::lightning::pay::LightningSendState;
use bark::ark::lightning::PaymentHash;
use bark::lightning_invoice::Bolt11Invoice;
use bip39::Mnemonic;
use bitcoin::Amount;
use nostr::nips::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, MakeInvoiceResponse, Method, NIP47Error,
    PayInvoiceResponse, Request, RequestParams, Response, ResponseResult,
};
use nostr_sdk::prelude::{
    nip04, Client as NostrClient, Event, EventBuilder, EventId, Filter, FinalizeEvent, JsonUtil,
    Keys, Kind, Tag, ToBech32,
};

use crate::nostr_support::public_key_from_npub_or_hex;
use crate::persistence::ServerConfig;
use crate::wallet::{open_bark_wallet, WalletOpenMode};
use crate::{NwcConnection, NwcPermission, NwcWakeRequest, WalletNetwork};

const NWC_EXTENSION_PAYMENT_SETTLE_TIMEOUT: Duration = Duration::from_secs(22);
const NWC_EXTENSION_PAYMENT_POLL_INTERVAL: Duration = Duration::from_millis(750);

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
    pub(crate) updated_snapshot_json: Option<String>,
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
    let request = decrypt_nwc_request(&event, &context.keys)?;
    match request.params {
        RequestParams::GetInfo
        | RequestParams::GetBalance
        | RequestParams::MakeInvoice(_)
        | RequestParams::PayInvoice(_) => {}
        _ => {
            anyhow::bail!(
                "NSE wake responder skipped {} request; queued for app",
                request.method.as_str()
            );
        }
    }

    process_nwc_request_event(wake, event, context).await
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
    let (response, amount_sat, updated_snapshot_json) =
        response_for_request(&request, &context, connection).await;
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
        amount_sat,
        processed_at: crate::time::now_unix(),
        updated_snapshot_json,
    })
}

pub(crate) fn build_nwc_info_event(keys: &Keys) -> anyhow::Result<Event> {
    EventBuilder::new(Kind::WalletConnectInfo, nwc_info_content())
        .tags([Tag::custom("encryption", ["nip04"])])
        .finalize(keys)
        .context("failed to sign NWC info event")
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

async fn response_for_request(
    request: &Request,
    context: &NwcServiceContext,
    connection: &NwcConnection,
) -> (Response, u64, Option<String>) {
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
        );
    }

    let (response, amount_sat, updated_connections) = match &request.params {
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
        ),
        RequestParams::GetBalance => (
            Response {
                result_type: Method::GetBalance,
                error: None,
                result: Some(ResponseResult::GetBalance(GetBalanceResponse {
                    balance: context.balance_sat.saturating_mul(1_000),
                })),
            },
            0,
            None,
        ),
        RequestParams::MakeInvoice(params) => match make_invoice_response(params, context).await {
            Ok(response) => (response, 0, None),
            Err(e) => (
                error_response(
                    Method::MakeInvoice,
                    ErrorCode::Internal,
                    &format!("Could not create invoice: {e:#}"),
                ),
                0,
                None,
            ),
        },
        RequestParams::PayInvoice(params) => {
            match pay_invoice_response(params, context, connection).await {
                Ok((response, amount_sat, updated_connections)) => {
                    (response, amount_sat, Some(updated_connections))
                }
                Err(e) => (
                    error_response(
                        Method::PayInvoice,
                        ErrorCode::PaymentFailed,
                        &format!("Could not pay invoice: {e:#}"),
                    ),
                    0,
                    None,
                ),
            }
        }
        _ => (not_implemented_response(request.method.clone()), 0, None),
    };

    let updated_snapshot_json = updated_connections.and_then(|connections| {
        snapshot_json_from_context(context, connections)
            .map_err(|e| eprintln!("failed to encode updated NWC snapshot: {e:#}"))
            .ok()
    });

    (response, amount_sat, updated_snapshot_json)
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
) -> anyhow::Result<(Response, u64, Vec<NwcConnection>)> {
    let wallet = open_wallet_for_extension(context).await?;
    let invoice = Bolt11Invoice::from_str(&params.invoice).context("invalid Lightning invoice")?;
    let amount_sat = pay_invoice_amount_sat(&invoice, params.amount)?;
    enforce_budget(connection, amount_sat)?;
    let payment_hash: PaymentHash = (*invoice.payment_hash()).into();

    let user_amount = params
        .amount
        .map(msats_to_exact_sats)
        .transpose()
        .context("Rebel Wallet only supports whole-sat NWC payment amounts")?
        .filter(|amount| *amount > 0)
        .map(Amount::from_sat);

    let preimage = match wallet
        .lightning_send_state(payment_hash)
        .await
        .context("could not read invoice state")?
    {
        LightningSendState::Paid(paid) => paid.preimage.to_string(),
        LightningSendState::InProgress(_) => wait_for_paid_invoice(&wallet, payment_hash).await?,
        LightningSendState::Unknown => {
            wallet
                .pay_lightning_invoice(invoice.clone(), user_amount, false)
                .await
                .context("Bark payment failed")?;
            wait_for_paid_invoice(&wallet, payment_hash).await?
        }
    };

    let mut updated_connections = context.connections.clone();
    if let Some(updated) = updated_connections
        .iter_mut()
        .find(|candidate| candidate.id == connection.id)
    {
        updated.spent_sat = updated.spent_sat.saturating_add(amount_sat);
        updated.spent_display = crate::state::format_sats(updated.spent_sat);
        updated.last_used_at = Some(crate::time::now_unix());
    }

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
    ))
}

async fn wait_for_paid_invoice(
    wallet: &bark::Wallet,
    payment_hash: PaymentHash,
) -> anyhow::Result<String> {
    let deadline = Instant::now() + NWC_EXTENSION_PAYMENT_SETTLE_TIMEOUT;

    loop {
        match wallet
            .check_lightning_payment(payment_hash, false)
            .await
            .context("could not drive paid invoice state")?
        {
            LightningSendState::Paid(paid) => return Ok(paid.preimage.to_string()),
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
