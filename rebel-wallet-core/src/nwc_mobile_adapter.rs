use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use bark::actions::lightning::pay::{LightningSend, LightningSendState};
use bark::actions::lightning::receive::{LightningReceive, LightningReceiveState};
use bark::ark::lightning::PaymentHash as BarkPaymentHash;
use bark::movement::{Movement, MovementStatus};
use bark::persist::models::SettledLightningReceive;
use bark::Wallet;
use bitcoin::Amount;
use nostr_sdk::prelude::SecretKey;
use nwc_mobile::{
    AmountMsat, ConnectionId, CreatedInvoice, HostError, HostErrorKind, HostFuture, InvoiceLookup,
    ListTransactionsRequest, MakeInvoiceRequest, NwcMethod, NwcSecretKey, OperationContext,
    PayInvoiceRequest, PaymentFailure, PaymentHash, PaymentPreimage, PaymentQuote, PaymentStatus,
    PublicKey, SecretProvider, TransactionDirection, UnixTimestamp, WakeLedger, WalletBackend,
    WalletInfo, WalletTransaction,
};
use nwc_mobile_bolt11::{
    created_invoice, exact_sats, parse_invoice, payment_amount_sats, quote_invoice_sats,
    sats_to_msats,
};
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
use nwc_mobile_tokio::run_with_context;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::core::NOSTR_SECRET_KEY;
use crate::SecretStore;

const NWC_LEDGER_FILE: &str = "nwc-mobile.sqlite3";

/// Opens the one cross-process ledger shared by the app and its NSE.
pub(crate) fn open_nwc_ledger(data_dir: &Path) -> Result<WakeLedger, nwc_mobile::LedgerError> {
    WakeLedger::open(nwc_ledger_path(data_dir))
}

pub(crate) fn nwc_ledger_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(NWC_LEDGER_FILE)
}

/// Loads the wallet-service secret from the platform secret store on demand.
pub(crate) struct RebelSecretProvider {
    secrets: Arc<dyn SecretStore>,
}

impl RebelSecretProvider {
    pub(crate) fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self { secrets }
    }
}

impl SecretProvider for RebelSecretProvider {
    fn load_nwc_secret(&self, _connection_id: &ConnectionId) -> Result<NwcSecretKey, HostError> {
        // Rebel uses one wallet-service Nostr identity for every connection;
        // per-connection client secrets never enter this provider.
        let encoded = self
            .secrets
            .get_secret(NOSTR_SECRET_KEY.to_string())
            .map(Zeroizing::new)
            .ok_or_else(|| host_error(HostErrorKind::Unavailable))?;
        let secret = SecretKey::parse(&encoded).map_err(|_| host_error(HostErrorKind::Internal))?;
        NwcSecretKey::from_bytes(secret.to_secret_bytes())
            .map_err(|_| host_error(HostErrorKind::Internal))
    }
}

/// Adapts Rebel's already-open Bark wallet to the nwc-mobile host contract.
#[derive(Clone)]
pub(crate) struct RebelWalletBackend {
    wallet: Wallet,
    service_pubkey: PublicKey,
}

impl RebelWalletBackend {
    pub(crate) fn new(wallet: Wallet, service_pubkey: PublicKey) -> Self {
        Self {
            wallet,
            service_pubkey,
        }
    }
}

impl WalletBackend for RebelWalletBackend {
    fn get_info<'a>(
        &'a self,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<WalletInfo, HostError>> {
        Box::pin(async move {
            run_with_context(context, async {
                Ok(WalletInfo::new(
                    Some(self.service_pubkey.clone()),
                    supported_methods(),
                ))
            })
            .await
        })
    }

    fn get_balance<'a>(
        &'a self,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<AmountMsat, HostError>> {
        Box::pin(async move {
            run_with_context(context, async {
                let balance = self
                    .wallet
                    .balance()
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))?;
                Ok(sats_to_msats(balance.spendable.to_sat()))
            })
            .await
        })
    }

    fn make_invoice<'a>(
        &'a self,
        request: MakeInvoiceRequest,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<CreatedInvoice, HostError>> {
        Box::pin(async move {
            let amount_sat = exact_sats(request.amount())?;
            if amount_sat == 0 {
                return Err(host_error(HostErrorKind::Rejected));
            }
            run_with_context(context, async {
                let invoice = self
                    .wallet
                    .bolt11_invoice(
                        Amount::from_sat(amount_sat),
                        request.description().map(ToString::to_string),
                        None,
                    )
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))?;
                created_invoice(invoice)
            })
            .await
        })
    }

    fn quote_payment<'a>(
        &'a self,
        invoice: &'a str,
        amount: Option<AmountMsat>,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<PaymentQuote, HostError>> {
        Box::pin(async move {
            let quote = quote_invoice_sats(invoice, amount)?;
            run_with_context(context, async move { Ok(quote) }).await
        })
    }

    fn payment_status<'a>(
        &'a self,
        payment_hash: &'a PaymentHash,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<PaymentStatus, HostError>> {
        Box::pin(async move {
            let payment_hash = bark_payment_hash(payment_hash)?;
            run_with_context(context, payment_status(&self.wallet, payment_hash)).await
        })
    }

    fn start_payment<'a>(
        &'a self,
        request: PayInvoiceRequest,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<PaymentStatus, HostError>> {
        Box::pin(async move {
            let invoice = parse_invoice(request.invoice())?;
            let amount_sat = payment_amount_sats(&invoice, request.amount())?;
            let payment_hash: BarkPaymentHash = (*invoice.payment_hash()).into();
            run_with_context(context, async {
                let existing = payment_status(&self.wallet, payment_hash).await?;
                if !matches!(existing, PaymentStatus::Unknown) {
                    return Ok(existing);
                }

                let fee = self
                    .wallet
                    .estimate_lightning_send_fee(Amount::from_sat(amount_sat))
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))?;
                if fee.fee.to_sat() > request.maximum_fee().as_sat() {
                    return Ok(PaymentStatus::Failed {
                        reason: PaymentFailure::Other,
                    });
                }

                // Bark persists sends by payment hash before network execution, so
                // the engine's event idempotency key composes with Bark's stronger
                // global invoice idempotency without a second mutable mapping.
                let user_amount = request
                    .amount()
                    .map(exact_sats)
                    .transpose()?
                    .map(Amount::from_sat);
                self.wallet
                    .pay_lightning_invoice(invoice, user_amount, false)
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))?;
                payment_status(&self.wallet, payment_hash).await
            })
            .await
        })
    }

    fn lookup_invoice<'a>(
        &'a self,
        request: InvoiceLookup,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<Option<WalletTransaction>, HostError>> {
        Box::pin(async move {
            let payment_hash = lookup_payment_hash(&request)?;
            run_with_context(context, lookup_transaction(&self.wallet, payment_hash)).await
        })
    }

    fn list_transactions<'a>(
        &'a self,
        request: ListTransactionsRequest,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<Vec<WalletTransaction>, HostError>> {
        Box::pin(async move {
            run_with_context(context, async {
                let mut transactions = self
                    .wallet
                    .history()
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))?
                    .iter()
                    .filter_map(transaction_from_movement)
                    .collect::<Vec<_>>();
                let mut known_hashes = transactions
                    .iter()
                    .filter_map(|transaction| transaction.payment_hash.as_ref())
                    .map(PaymentHash::to_hex)
                    .collect::<BTreeSet<_>>();

                let receives = self
                    .wallet
                    .pending_lightning_receives()
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))?;
                transactions.extend(receives.iter().filter_map(|receive| {
                    let hash = receive.payment_hash.to_string();
                    known_hashes
                        .insert(hash)
                        .then(|| transaction_from_pending_receive(receive))
                        .flatten()
                }));

                let sends = self
                    .wallet
                    .pending_lightning_sends()
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))?;
                transactions.extend(sends.iter().filter_map(|send| {
                    let hash = send.invoice.payment_hash().to_string();
                    known_hashes
                        .insert(hash)
                        .then(|| transaction_from_pending_send(send))
                        .flatten()
                }));

                transactions.retain(|transaction| transaction_matches(transaction, request));
                transactions.sort_by(|left, right| right.created_at.cmp(&left.created_at));
                Ok(transactions
                    .into_iter()
                    .skip(request.offset as usize)
                    .take(usize::from(request.limit))
                    .collect())
            })
            .await
        })
    }
}

async fn payment_status(
    wallet: &Wallet,
    payment_hash: BarkPaymentHash,
) -> Result<PaymentStatus, HostError> {
    let state = wallet
        .lightning_send_state(payment_hash)
        .await
        .map_err(|_| host_error(HostErrorKind::Internal))?;
    match state {
        LightningSendState::InProgress(_) => Ok(PaymentStatus::Pending),
        LightningSendState::Paid(paid) => {
            let history = wallet
                .history()
                .await
                .map_err(|_| host_error(HostErrorKind::Internal))?;
            let movement = history
                .iter()
                .find(|movement| movement.lightning_payment_hash() == Some(payment_hash))
                .ok_or_else(|| host_error(HostErrorKind::Internal))?;
            let direction = movement_direction(movement);
            let amount = movement_amount_sat(movement, direction);
            let preimage = PaymentPreimage::from_hex(&paid.preimage.to_string())
                .map_err(|_| host_error(HostErrorKind::Internal))?;
            Ok(PaymentStatus::Succeeded {
                preimage,
                amount: sats_to_msats(amount),
                fee: sats_to_msats(movement.offchain_fee.to_sat()),
            })
        }
        LightningSendState::Unknown => {
            let history = wallet
                .history()
                .await
                .map_err(|_| host_error(HostErrorKind::Internal))?;
            Ok(history
                .iter()
                .find(|movement| movement.lightning_payment_hash() == Some(payment_hash))
                .map(payment_status_from_movement)
                .transpose()?
                .unwrap_or(PaymentStatus::Unknown))
        }
    }
}

fn payment_status_from_movement(movement: &Movement) -> Result<PaymentStatus, HostError> {
    match movement.status {
        MovementStatus::Pending => Ok(PaymentStatus::Pending),
        MovementStatus::Failed | MovementStatus::Canceled => Ok(PaymentStatus::Failed {
            reason: PaymentFailure::Other,
        }),
        MovementStatus::Successful => {
            let preimage =
                movement_preimage(movement).ok_or_else(|| host_error(HostErrorKind::Internal))?;
            let direction = movement_direction(movement);
            Ok(PaymentStatus::Succeeded {
                preimage,
                amount: sats_to_msats(movement_amount_sat(movement, direction)),
                fee: sats_to_msats(movement.offchain_fee.to_sat()),
            })
        }
    }
}

async fn lookup_transaction(
    wallet: &Wallet,
    payment_hash: BarkPaymentHash,
) -> Result<Option<WalletTransaction>, HostError> {
    if let Ok(receive) = wallet.lightning_receive_state(payment_hash).await {
        return Ok(transaction_from_receive_state(&receive));
    }
    let history = wallet
        .history()
        .await
        .map_err(|_| host_error(HostErrorKind::Internal))?;
    if let Some(transaction) = history
        .iter()
        .find(|movement| movement.lightning_payment_hash() == Some(payment_hash))
        .and_then(transaction_from_movement)
    {
        return Ok(Some(transaction));
    }
    match wallet
        .lightning_send_state(payment_hash)
        .await
        .map_err(|_| host_error(HostErrorKind::Internal))?
    {
        LightningSendState::InProgress(send) => Ok(transaction_from_pending_send(&send)),
        LightningSendState::Paid(_) | LightningSendState::Unknown => Ok(None),
    }
}

fn transaction_from_movement(movement: &Movement) -> Option<WalletTransaction> {
    let payment_hash = movement
        .lightning_payment_hash()
        .and_then(|hash| PaymentHash::from_hex(&hash.to_string()).ok())?;
    let direction = movement_direction(movement);
    let status = payment_status_from_movement(movement).ok()?;
    Some(WalletTransaction {
        payment_hash: Some(payment_hash),
        direction,
        amount: sats_to_msats(movement_amount_sat(movement, direction)),
        fee: sats_to_msats(movement.offchain_fee.to_sat()),
        created_at: timestamp(movement.time.created_at.timestamp()),
        settled_at: movement
            .time
            .completed_at
            .map(|time| timestamp(time.timestamp())),
        status,
    })
}

fn transaction_from_pending_receive(receive: &LightningReceive) -> Option<WalletTransaction> {
    Some(WalletTransaction {
        payment_hash: Some(PaymentHash::from_hex(&receive.payment_hash.to_string()).ok()?),
        direction: TransactionDirection::Incoming,
        amount: AmountMsat::from_msat(receive.invoice.amount_milli_satoshis()?),
        fee: AmountMsat::default(),
        created_at: UnixTimestamp::from_secs(receive.invoice.duration_since_epoch().as_secs()),
        settled_at: None,
        status: PaymentStatus::Pending,
    })
}

fn transaction_from_settled_receive(
    receive: &SettledLightningReceive,
) -> Option<WalletTransaction> {
    Some(WalletTransaction {
        payment_hash: Some(PaymentHash::from_hex(&receive.payment_hash.to_string()).ok()?),
        direction: TransactionDirection::Incoming,
        amount: sats_to_msats(receive.amount.to_sat()),
        fee: AmountMsat::default(),
        created_at: UnixTimestamp::from_secs(receive.invoice.duration_since_epoch().as_secs()),
        settled_at: Some(timestamp(receive.settled_at.timestamp())),
        status: PaymentStatus::Succeeded {
            preimage: PaymentPreimage::from_hex(&receive.preimage.to_string()).ok()?,
            amount: sats_to_msats(receive.amount.to_sat()),
            fee: AmountMsat::default(),
        },
    })
}

fn transaction_from_receive_state(receive: &LightningReceiveState) -> Option<WalletTransaction> {
    match receive {
        LightningReceiveState::InProgress(receive) => transaction_from_pending_receive(receive),
        LightningReceiveState::Settled(receive) => transaction_from_settled_receive(receive),
    }
}

fn transaction_from_pending_send(send: &LightningSend) -> Option<WalletTransaction> {
    Some(WalletTransaction {
        payment_hash: Some(PaymentHash::from_hex(&send.invoice.payment_hash().to_string()).ok()?),
        direction: TransactionDirection::Outgoing,
        amount: sats_to_msats(send.payment_amount.to_sat()),
        fee: sats_to_msats(send.fee.to_sat()),
        created_at: UnixTimestamp::from_secs(0),
        settled_at: None,
        status: PaymentStatus::Pending,
    })
}

fn transaction_matches(transaction: &WalletTransaction, request: ListTransactionsRequest) -> bool {
    if request
        .from
        .is_some_and(|from| transaction.created_at < from)
        || request
            .until
            .is_some_and(|until| transaction.created_at > until)
        || request
            .direction
            .is_some_and(|direction| transaction.direction != direction)
    {
        return false;
    }
    request.include_unpaid || matches!(transaction.status, PaymentStatus::Succeeded { .. })
}

fn movement_direction(movement: &Movement) -> TransactionDirection {
    if (!movement.received_on.is_empty() && movement.sent_to.is_empty())
        || movement.effective_balance.to_sat() >= 0
    {
        TransactionDirection::Incoming
    } else {
        TransactionDirection::Outgoing
    }
}

fn movement_amount_sat(movement: &Movement, direction: TransactionDirection) -> u64 {
    let destinations = match direction {
        TransactionDirection::Incoming => &movement.received_on,
        TransactionDirection::Outgoing => &movement.sent_to,
        _ => return movement.effective_balance.to_sat().unsigned_abs(),
    };
    let total = destinations
        .iter()
        .map(|destination| destination.amount.to_sat())
        .sum();
    if total > 0 {
        total
    } else {
        movement.effective_balance.to_sat().unsigned_abs()
    }
}

fn movement_preimage(movement: &Movement) -> Option<PaymentPreimage> {
    movement
        .metadata
        .get("payment_preimage")
        .and_then(Value::as_str)
        .and_then(|preimage| PaymentPreimage::from_hex(preimage).ok())
}

fn lookup_payment_hash(request: &InvoiceLookup) -> Result<BarkPaymentHash, HostError> {
    match request {
        InvoiceLookup::PaymentHash(hash) => bark_payment_hash(hash),
        InvoiceLookup::Invoice(invoice) => {
            parse_invoice(invoice).map(|invoice| (*invoice.payment_hash()).into())
        }
        _ => Err(host_error(HostErrorKind::Rejected)),
    }
}

fn bark_payment_hash(payment_hash: &PaymentHash) -> Result<BarkPaymentHash, HostError> {
    BarkPaymentHash::from_str(&payment_hash.to_hex())
        .map_err(|_| host_error(HostErrorKind::Rejected))
}

fn timestamp(seconds: i64) -> UnixTimestamp {
    UnixTimestamp::from_secs(seconds.max(0) as u64)
}

fn supported_methods() -> [NwcMethod; 6] {
    [
        NwcMethod::GetInfo,
        NwcMethod::GetBalance,
        NwcMethod::MakeInvoice,
        NwcMethod::PayInvoice,
        NwcMethod::LookupInvoice,
        NwcMethod::ListTransactions,
    ]
}

const fn host_error(kind: HostErrorKind) -> HostError {
    HostError::new(kind)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const SECRET_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct TestSecrets(Mutex<Option<String>>);

    impl SecretStore for TestSecrets {
        fn get_secret(&self, _key: String) -> Option<String> {
            self.0.lock().expect("secret lock").clone()
        }

        fn set_secret(&self, _key: String, _value: String) -> bool {
            false
        }

        fn delete_secret(&self, _key: String) -> bool {
            false
        }
    }

    #[test]
    fn secret_provider_loads_without_caching_or_exposing_material() {
        let store = Arc::new(TestSecrets(Mutex::new(Some(SECRET_HEX.to_string()))));
        let provider = RebelSecretProvider::new(store);
        let connection = ConnectionId::parse("connection:test").expect("connection");

        let secret = provider.load_nwc_secret(&connection).expect("secret");

        assert_eq!(format!("{secret:?}"), "NwcSecretKey([redacted])");
    }

    #[test]
    fn secret_provider_fails_closed_for_missing_or_invalid_material() {
        let connection = ConnectionId::parse("connection:test").expect("connection");
        for value in [None, Some("not-a-secret".to_string())] {
            let provider = RebelSecretProvider::new(Arc::new(TestSecrets(Mutex::new(value))));
            assert!(provider.load_nwc_secret(&connection).is_err());
        }
    }

    #[test]
    fn ledger_path_stays_inside_supplied_app_group_directory() {
        let directory = tempfile::tempdir().expect("directory");
        let ledger = open_nwc_ledger(directory.path()).expect("ledger");
        drop(ledger);
        assert!(directory.path().join(NWC_LEDGER_FILE).is_file());
    }
}
