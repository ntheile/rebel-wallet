//! Rebel Wallet's Bark implementation of the `nwc-mobile` host contract.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bark::actions::lightning::pay::{LightningSend, LightningSendState};
use bark::actions::lightning::receive::{LightningReceive, LightningReceiveState};
use bark::ark::lightning::PaymentHash as BarkPaymentHash;
use bark::movement::{Movement, MovementStatus};
use bark::persist::models::SettledLightningReceive;
use bark::{FeeEstimate, Wallet};
use bitcoin::Amount;
use nwc_mobile::{
    AmountMsat, CancellationSignal, CreatedInvoice, EventId, HostError, HostErrorKind, HostFuture,
    InvoiceLookup, InvoiceNotificationError, InvoiceNotificationWorker,
    InvoiceNotificationWorkerReport, ListTransactionsRequest, MakeInvoiceRequest, NotificationHint,
    NwcMethod, NwcNotificationType, NwcWalletBackend, OperationBudget, OperationContext,
    PayInvoiceRequest, PaymentFailure, PaymentHash, PaymentPreimage, PaymentQuote, PaymentStatus,
    PublicKey, RelayTransport, SecretProvider, SystemClock, TransactionDirection, UnixTimestamp,
    WakeDiagnosticCode, WakeDiagnosticSink, WakeDisposition, WakeEngine, WakeInput, WakeLedger,
    WakePolicy, WalletInfo, WalletTransaction,
};
use nwc_mobile_bolt11::{
    created_invoice, exact_sats, parse_invoice, payment_amount_sats, quote_invoice_sats,
    sats_to_msats,
};
use nwc_mobile_tokio::{run_with_context, sleep, spawn_abort_on_drop};
use serde_json::Value;

const PAYMENT_SETTLE_TIMEOUT: Duration = Duration::from_secs(22);
const PAYMENT_POLL_INTERVAL: Duration = Duration::from_millis(750);
const INVOICE_SETTLEMENT_MAX_WAIT: Duration = Duration::from_secs(25);
const INVOICE_SETTLEMENT_COMPLETION_RESERVE: Duration = Duration::from_secs(3);
const INVOICE_SETTLEMENT_POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Adapts an already-open Bark wallet to the `nwc-mobile` host contract.
#[derive(Clone)]
pub(crate) struct NwcBarkWallet {
    wallet: Wallet,
    service_pubkey: PublicKey,
    diagnostics: Option<Arc<dyn WakeDiagnosticSink>>,
    await_invoice_settlement: bool,
}

impl NwcBarkWallet {
    /// Creates an adapter with the public wallet-service identity it advertises.
    #[must_use]
    pub(crate) fn new(wallet: Wallet, service_pubkey: PublicKey) -> Self {
        Self {
            wallet,
            service_pubkey,
            diagnostics: None,
            await_invoice_settlement: false,
        }
    }
    /// Creates an adapter that emits bounded, non-secret execution codes.
    #[must_use]
    pub(crate) fn with_diagnostics(
        wallet: Wallet,
        service_pubkey: PublicKey,
        diagnostics: Arc<dyn WakeDiagnosticSink>,
    ) -> Self {
        Self {
            wallet,
            service_pubkey,
            diagnostics: Some(diagnostics),
            await_invoice_settlement: false,
        }
    }

    fn awaiting_invoice_settlement(mut self) -> Self {
        self.await_invoice_settlement = true;
        self
    }

    fn record_diagnostic(&self, code: WakeDiagnosticCode) {
        if let Some(diagnostics) = self.diagnostics.as_deref() {
            diagnostics.record(code);
        }
    }
}

/// Executes one wake using an already-open Bark wallet and standard policy.
///
/// This is the shared assembly point used by foreground applications and native
/// background extensions. The wake envelope selects the advertised service
/// identity; authorization is still loaded from the durable ledger before any
/// wallet or secret capability is used.
pub(crate) async fn execute_bark_wake(
    ledger: &WakeLedger,
    wallet: Wallet,
    relays: &dyn RelayTransport,
    secrets: &dyn SecretProvider,
    input: WakeInput,
    budget: OperationBudget,
    cancellation: &dyn CancellationSignal,
) -> WakeDisposition {
    let started = Instant::now();
    let request_event_id = input.event_id().clone();
    let wallet = NwcBarkWallet::new(wallet, input.wallet_service_pubkey().clone());
    let disposition = WakeEngine::new(
        ledger,
        &wallet,
        relays,
        secrets,
        &SystemClock,
        WakePolicy::default(),
    )
    .execute(input, budget, cancellation)
    .await;
    if let Ok(notification_budget) =
        OperationBudget::new(budget.timeout().saturating_sub(started.elapsed()))
    {
        let worker = InvoiceNotificationWorker::new(ledger, &wallet, relays, secrets, &SystemClock);
        let _ = run_invoice_notification_worker(
            ledger,
            &worker,
            &request_event_id,
            lingers_for_invoice_settlement(disposition),
            notification_budget,
            cancellation,
        )
        .await;
    }
    disposition
}

/// Executes one wake while reporting bounded, non-secret diagnostic codes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_bark_wake_with_diagnostics(
    ledger: &WakeLedger,
    wallet: Wallet,
    relays: &dyn RelayTransport,
    secrets: &dyn SecretProvider,
    input: WakeInput,
    budget: OperationBudget,
    cancellation: &dyn CancellationSignal,
    diagnostics: Arc<dyn WakeDiagnosticSink>,
) -> WakeDisposition {
    let started = Instant::now();
    let request_event_id = input.event_id().clone();
    let wallet = NwcBarkWallet::with_diagnostics(
        wallet,
        input.wallet_service_pubkey().clone(),
        Arc::clone(&diagnostics),
    );
    let disposition = WakeEngine::new(
        ledger,
        &wallet,
        relays,
        secrets,
        &SystemClock,
        WakePolicy::default(),
    )
    .with_diagnostics(diagnostics.as_ref())
    .execute(input, budget, cancellation)
    .await;
    if let Ok(notification_budget) =
        OperationBudget::new(budget.timeout().saturating_sub(started.elapsed()))
    {
        let worker = InvoiceNotificationWorker::new(ledger, &wallet, relays, secrets, &SystemClock);
        let _ = run_invoice_notification_worker(
            ledger,
            &worker,
            &request_event_id,
            lingers_for_invoice_settlement(disposition),
            notification_budget,
            cancellation,
        )
        .await;
    }
    disposition
}

const fn lingers_for_invoice_settlement(disposition: WakeDisposition) -> bool {
    matches!(
        disposition.notification(),
        NotificationHint::Request {
            method: NwcMethod::MakeInvoice
        }
    )
}

async fn run_invoice_notification_worker(
    ledger: &WakeLedger,
    worker: &InvoiceNotificationWorker<'_>,
    request_event_id: &EventId,
    linger: bool,
    budget: OperationBudget,
    cancellation: &dyn CancellationSignal,
) -> Result<InvoiceNotificationWorkerReport, InvoiceNotificationError> {
    let deadline = Instant::now() + budget.timeout();
    let mut aggregate = InvoiceNotificationWorkerReport::default();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(pass_budget) = OperationBudget::new(remaining) else {
            return Ok(aggregate);
        };
        let report = worker.run(pass_budget, cancellation).await?;
        aggregate.inspected = aggregate.inspected.saturating_add(report.inspected);
        aggregate.pending = report.pending;
        aggregate.expired = aggregate.expired.saturating_add(report.expired);
        aggregate.delivered = aggregate.delivered.saturating_add(report.delivered);
        aggregate.retryable = report.retryable;

        let target_pending = ledger
            .nwc_invoice_monitor(request_event_id)
            .map_err(|_| InvoiceNotificationError::Ledger)?
            .is_some_and(|monitor| !monitor.completed());
        if !linger || !target_pending || cancellation.is_cancelled() {
            return Ok(aggregate);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= INVOICE_SETTLEMENT_POLL_INTERVAL {
            return Ok(aggregate);
        }
        sleep(INVOICE_SETTLEMENT_POLL_INTERVAL).await;
    }
}

/// Reconciles Bark invoices and durably publishes pending NIP-47 payment notifications.
pub(crate) async fn run_bark_notification_worker(
    ledger: &WakeLedger,
    wallet: Wallet,
    wallet_service_pubkey: PublicKey,
    relays: &dyn RelayTransport,
    secrets: &dyn SecretProvider,
    budget: OperationBudget,
    cancellation: &dyn CancellationSignal,
) -> Result<InvoiceNotificationWorkerReport, InvoiceNotificationError> {
    let wallet = NwcBarkWallet::new(wallet, wallet_service_pubkey);
    InvoiceNotificationWorker::new(ledger, &wallet, relays, secrets, &SystemClock)
        .run(budget, cancellation)
        .await
}

/// Reconciles one exact Bark invoice and publishes its NIP-47 notification.
///
/// This is the preferred entry point for a server-scheduled mobile settlement
/// wake because it does not replay the original NIP-47 request and it cannot be
/// delayed behind unrelated pending invoices.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_bark_invoice_notification_worker(
    ledger: &WakeLedger,
    wallet: Wallet,
    wallet_service_pubkey: PublicKey,
    request_event_id: &EventId,
    relays: &dyn RelayTransport,
    secrets: &dyn SecretProvider,
    budget: OperationBudget,
    cancellation: &dyn CancellationSignal,
) -> Result<InvoiceNotificationWorkerReport, InvoiceNotificationError> {
    let wallet = NwcBarkWallet::new(wallet, wallet_service_pubkey).awaiting_invoice_settlement();
    InvoiceNotificationWorker::new(ledger, &wallet, relays, secrets, &SystemClock)
        .run_invoice(request_event_id, budget, cancellation)
        .await
}

impl NwcWalletBackend for NwcBarkWallet {
    fn get_info<'a>(
        &'a self,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<WalletInfo, HostError>> {
        Box::pin(async move {
            run_with_context(context, async {
                Ok(wallet_info(self.service_pubkey.clone()))
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
            run_with_context(
                context,
                run_with_bark_mailbox(
                    self.wallet.clone(),
                    wait_for_payment_terminal(&self.wallet, payment_hash),
                ),
            )
            .await
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
            let payment = async {
                let existing = payment_status_without_mailbox(&self.wallet, payment_hash).await?;
                if !matches!(existing, PaymentStatus::Unknown) {
                    return Ok(existing);
                }

                let fee = self
                    .wallet
                    .estimate_lightning_send_fee(Amount::from_sat(amount_sat))
                    .await
                    .map_err(|_| {
                        self.record_diagnostic(WakeDiagnosticCode::PaymentBackendFailed);
                        host_error(HostErrorKind::Internal)
                    })?;
                let spendable = if fee.vtxos_spent.is_empty() {
                    Some(
                        self.wallet
                            .balance()
                            .await
                            .map_err(|_| {
                                self.record_diagnostic(WakeDiagnosticCode::PaymentBackendFailed);
                                host_error(HostErrorKind::Internal)
                            })?
                            .spendable,
                    )
                } else {
                    None
                };
                if let Some((reason, diagnostic)) = payment_preflight_failure(
                    &fee,
                    spendable,
                    Amount::from_sat(request.maximum_fee().as_sat()),
                ) {
                    self.record_diagnostic(diagnostic);
                    return Ok(PaymentStatus::Failed { reason });
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
                    .map_err(|_| {
                        self.record_diagnostic(WakeDiagnosticCode::PaymentBackendFailed);
                        host_error(HostErrorKind::Internal)
                    })?;
                wait_for_payment_terminal(&self.wallet, payment_hash).await
            };
            run_with_context(context, run_with_bark_mailbox(self.wallet.clone(), payment)).await
        })
    }

    fn lookup_invoice<'a>(
        &'a self,
        request: InvoiceLookup,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<Option<WalletTransaction>, HostError>> {
        Box::pin(async move {
            let payment_hash = lookup_payment_hash(&request)?;
            let settlement_wait = self
                .await_invoice_settlement
                .then(|| invoice_settlement_wait(context))
                .filter(|wait| !wait.is_zero());
            let result = run_with_context(
                context,
                reconcile_then_lookup_transaction(
                    &self.wallet,
                    payment_hash,
                    context,
                    settlement_wait,
                ),
            )
            .await;
            match &result {
                Ok(Some(transaction))
                    if matches!(transaction.status, PaymentStatus::Succeeded { .. }) =>
                {
                    self.record_diagnostic(WakeDiagnosticCode::InvoiceLookupSettled);
                }
                Ok(Some(_)) => {
                    self.record_diagnostic(WakeDiagnosticCode::InvoiceLookupPending);
                }
                Ok(None) => {
                    self.record_diagnostic(WakeDiagnosticCode::InvoiceLookupNotFound);
                }
                Err(_) => {
                    self.record_diagnostic(WakeDiagnosticCode::InvoiceLookupFailed);
                }
            }
            result
        })
    }

    fn list_transactions<'a>(
        &'a self,
        request: ListTransactionsRequest,
        context: OperationContext<'a>,
    ) -> HostFuture<'a, Result<Vec<WalletTransaction>, HostError>> {
        Box::pin(async move {
            run_with_context(context, async {
                // Bark's history and pending receive tables are local views. Refresh
                // the durable mailbox first so an NWC client never receives a stale
                // transaction list after the application has been suspended.
                self.wallet.sync().await;
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
                transactions.sort_by_key(|transaction| std::cmp::Reverse(transaction.created_at));
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

async fn payment_status_without_mailbox(
    wallet: &Wallet,
    payment_hash: BarkPaymentHash,
) -> Result<PaymentStatus, HostError> {
    // Drive one nonblocking step and release Bark's action lock. The mailbox
    // processor may need that same lock to apply the server-delivered preimage.
    let state = wallet
        .check_lightning_payment(payment_hash, false)
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

async fn wait_for_payment_terminal(
    wallet: &Wallet,
    payment_hash: BarkPaymentHash,
) -> Result<PaymentStatus, HostError> {
    let deadline = Instant::now() + PAYMENT_SETTLE_TIMEOUT;
    loop {
        let status = payment_status_without_mailbox(wallet, payment_hash).await?;
        if !matches!(status, PaymentStatus::Pending) || Instant::now() >= deadline {
            return Ok(status);
        }
        sleep(PAYMENT_POLL_INTERVAL).await;
    }
}

/// Keeps Bark's durable mailbox stream alive while a bounded payment operation runs.
///
/// The full wallet app normally owns this stream. Short-lived native workers such as
/// an iOS notification service extension must run it explicitly so the server's
/// payment-finished message can settle the checkpoint and persist the preimage.
async fn run_with_bark_mailbox<T>(
    wallet: Wallet,
    operation: impl Future<Output = Result<T, HostError>>,
) -> Result<T, HostError> {
    let _mailbox = spawn_abort_on_drop(async move {
        let _ = wallet
            .subscribe_process_mailbox_messages(None, Default::default())
            .await;
    })
    .map_err(|_| host_error(HostErrorKind::Internal))?;
    operation.await
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

async fn reconcile_then_lookup_transaction(
    wallet: &Wallet,
    payment_hash: BarkPaymentHash,
    context: OperationContext<'_>,
    settlement_wait: Option<Duration>,
) -> Result<Option<WalletTransaction>, HostError> {
    // A settled receive is already durable and needs no network refresh. This
    // fast path also lets the notification worker publish payment_received in
    // the same NSE wake without repeating the mailbox round trip.
    if let Ok(receive @ LightningReceiveState::Settled(_)) =
        wallet.lightning_receive_state(payment_hash).await
    {
        return Ok(transaction_from_receive_state(&receive));
    }

    let settlement_deadline = settlement_wait.map(|wait| Instant::now() + wait);

    // Preserve the full wallet refresh used by Rebel's original NWC receive
    // flow. Mailbox-only sync is insufficient when Bark also needs refreshed
    // Ark state to drive the receive action. A server-scheduled settlement
    // wake keeps driving this exact invoice until it settles or the bounded
    // wait expires; ordinary lookup_invoice requests still perform one pass.
    if let Some(deadline) = settlement_deadline {
        if let Some(sync_context) = context_before(deadline, context) {
            let sync_result = run_with_context(sync_context, async {
                wallet.sync().await;
                Ok(())
            })
            .await;
            if sync_result.is_err_and(|error| error.kind() == HostErrorKind::Cancelled) {
                return Err(host_error(HostErrorKind::Cancelled));
            }
        }
    } else {
        wallet.sync().await;
    }

    loop {
        let claim_result = if let Some(deadline) = settlement_deadline {
            let Some(claim_context) = context_before(deadline, context) else {
                break;
            };
            run_with_context(claim_context, async {
                wallet
                    .try_claim_lightning_receive(payment_hash, false)
                    .await
                    .map_err(|_| host_error(HostErrorKind::Internal))
            })
            .await
        } else {
            wallet
                .try_claim_lightning_receive(payment_hash, false)
                .await
                .map_err(|_| host_error(HostErrorKind::Internal))
        };

        match claim_result {
            Ok(receive @ LightningReceiveState::Settled(_)) => {
                return Ok(transaction_from_receive_state(&receive));
            }
            Ok(_) => {}
            Err(error) if error.kind() == HostErrorKind::Cancelled => return Err(error),
            Err(_) => {}
        }

        let Some(deadline) = settlement_deadline else {
            break;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(remaining.min(INVOICE_SETTLEMENT_POLL_INTERVAL)).await;
    }

    // Preserve outgoing and history-only lookup behavior, and return the last
    // durable incoming state when a transient claim attempt could not advance.
    lookup_transaction(wallet, payment_hash).await
}

fn invoice_settlement_wait(context: OperationContext<'_>) -> Duration {
    context
        .budget()
        .timeout()
        .saturating_sub(INVOICE_SETTLEMENT_COMPLETION_RESERVE)
        .min(INVOICE_SETTLEMENT_MAX_WAIT)
}

fn context_before<'a>(
    deadline: Instant,
    context: OperationContext<'a>,
) -> Option<OperationContext<'a>> {
    OperationBudget::new(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .map(|budget| OperationContext::new(budget, context.cancellation()))
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

fn supported_notifications() -> [NwcNotificationType; 1] {
    // Do not advertise payment_received until the mobile host has a reliable
    // server-side wake source. NWC clients such as Alby Go then poll
    // lookup_invoice, which wakes the app and refreshes Bark's mailbox.
    [NwcNotificationType::PaymentSent]
}

fn wallet_info(service_pubkey: PublicKey) -> WalletInfo {
    WalletInfo::new(Some(service_pubkey), supported_methods())
        .with_notifications(supported_notifications())
}

fn payment_preflight_failure(
    fee: &FeeEstimate,
    spendable: Option<Amount>,
    maximum_fee: Amount,
) -> Option<(PaymentFailure, WakeDiagnosticCode)> {
    if spendable.is_some_and(|balance| balance < fee.gross_amount) {
        return Some((
            PaymentFailure::InsufficientFunds,
            WakeDiagnosticCode::PaymentInsufficientFunds,
        ));
    }
    if fee.fee > maximum_fee {
        return Some((
            PaymentFailure::Other,
            WakeDiagnosticCode::PaymentFeeLimitExceeded,
        ));
    }
    None
}

const fn host_error(kind: HostErrorKind) -> HostError {
    HostError::new(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transaction(status: PaymentStatus) -> WalletTransaction {
        WalletTransaction {
            payment_hash: None,
            direction: TransactionDirection::Incoming,
            amount: AmountMsat::from_msat(1_000),
            fee: AmountMsat::default(),
            created_at: UnixTimestamp::from_secs(100),
            settled_at: None,
            status,
        }
    }

    fn request() -> ListTransactionsRequest {
        ListTransactionsRequest {
            from: None,
            until: None,
            limit: 10,
            offset: 0,
            direction: None,
            include_unpaid: true,
        }
    }

    #[test]
    fn advertises_only_implemented_nwc_methods() {
        assert_eq!(
            supported_methods(),
            [
                NwcMethod::GetInfo,
                NwcMethod::GetBalance,
                NwcMethod::MakeInvoice,
                NwcMethod::PayInvoice,
                NwcMethod::LookupInvoice,
                NwcMethod::ListTransactions,
            ]
        );
    }

    #[test]
    fn wallet_info_requires_lookup_polling_for_receive_settlement() {
        let service_pubkey =
            PublicKey::from_hex("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .expect("service public key");
        assert_eq!(
            wallet_info(service_pubkey)
                .notifications()
                .collect::<Vec<_>>(),
            vec![NwcNotificationType::PaymentSent]
        );
    }

    #[test]
    fn only_successful_make_invoice_wakes_linger_for_settlement() {
        assert!(lingers_for_invoice_settlement(WakeDisposition::Completed {
            notification: NotificationHint::Request {
                method: NwcMethod::MakeInvoice,
            },
        }));
        assert!(!lingers_for_invoice_settlement(
            WakeDisposition::Completed {
                notification: NotificationHint::Request {
                    method: NwcMethod::LookupInvoice,
                },
            }
        ));
        assert!(!lingers_for_invoice_settlement(WakeDisposition::Rejected {
            code: nwc_mobile::RejectionCode::InvalidRequest,
            notification: NotificationHint::OpenApplication,
        }));
    }

    #[test]
    fn invoice_settlement_wait_preserves_completion_time_and_caps_polling() {
        let cancellation = nwc_mobile::NeverCancelled;
        let context = OperationContext::new(
            OperationBudget::new(Duration::from_secs(40)).expect("budget"),
            &cancellation,
        );
        assert_eq!(invoice_settlement_wait(context), Duration::from_secs(25));

        let context = OperationContext::new(
            OperationBudget::new(Duration::from_secs(20)).expect("budget"),
            &cancellation,
        );
        assert_eq!(invoice_settlement_wait(context), Duration::from_secs(17));

        let context = OperationContext::new(
            OperationBudget::new(Duration::from_secs(2)).expect("budget"),
            &cancellation,
        );
        assert!(invoice_settlement_wait(context).is_zero());
    }

    #[test]
    fn payment_preflight_distinguishes_balance_and_fee_failures() {
        let fee = FeeEstimate::new(
            Amount::from_sat(610),
            Amount::from_sat(600),
            Amount::from_sat(10),
            Vec::new(),
        );

        assert_eq!(
            payment_preflight_failure(&fee, Some(Amount::from_sat(500)), Amount::from_sat(500),),
            Some((
                PaymentFailure::InsufficientFunds,
                WakeDiagnosticCode::PaymentInsufficientFunds,
            ))
        );
        assert_eq!(
            payment_preflight_failure(&fee, Some(Amount::from_sat(1_000)), Amount::from_sat(500),),
            Some((
                PaymentFailure::Other,
                WakeDiagnosticCode::PaymentFeeLimitExceeded,
            ))
        );
        assert_eq!(
            payment_preflight_failure(&fee, Some(Amount::from_sat(1_000)), Amount::from_sat(600),),
            None
        );
    }

    #[test]
    fn transaction_filter_enforces_bounds_direction_and_payment_state() {
        let settled = transaction(PaymentStatus::Succeeded {
            preimage: PaymentPreimage::from_bytes([1_u8; 32]),
            amount: AmountMsat::from_msat(1_000),
            fee: AmountMsat::default(),
        });
        assert!(transaction_matches(&settled, request()));

        let mut filtered = request();
        filtered.from = Some(UnixTimestamp::from_secs(101));
        assert!(!transaction_matches(&settled, filtered));
        filtered = request();
        filtered.until = Some(UnixTimestamp::from_secs(99));
        assert!(!transaction_matches(&settled, filtered));
        filtered = request();
        filtered.direction = Some(TransactionDirection::Outgoing);
        assert!(!transaction_matches(&settled, filtered));

        let mut paid_only = request();
        paid_only.include_unpaid = false;
        assert!(!transaction_matches(
            &transaction(PaymentStatus::Pending),
            paid_only
        ));
    }

    #[test]
    fn negative_wallet_timestamps_fail_closed_to_epoch() {
        assert_eq!(timestamp(-1), UnixTimestamp::from_secs(0));
        assert_eq!(timestamp(42), UnixTimestamp::from_secs(42));
    }
}
