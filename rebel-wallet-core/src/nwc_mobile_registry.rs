use anyhow::{anyhow, Context};
use nwc_mobile::{
    ActiveConnection, ApprovedNwaConnection, BudgetInterval, BudgetPolicy, Clock, ConnectionId,
    ConnectionManager, ConnectionPolicy, FeePolicy, LegacyConnectionImport, NewConnection,
    NwaRequest, NwcEncryption, NwcMethod, PublicKey, UnixTimestamp, WakeLedger, WakePolicy,
};

use crate::{NwcBudgetInterval, NwcConnection, NwcPermission};

// Existing Rebel Wallet connection URIs and clients advertise NIP-04. A future
// NIP-44 migration must update info-event advertisement, new registry entries,
// and persisted-connection migration together before changing this policy.
pub(crate) const NWC_ENCRYPTION: NwcEncryption = NwcEncryption::LegacyNip04;

pub(crate) struct MigrationResult {
    pub(crate) revoked_client_pubkeys: Vec<String>,
}

struct MigrationClock(UnixTimestamp);

impl Clock for MigrationClock {
    fn now(&self) -> UnixTimestamp {
        self.0
    }
}

pub(crate) fn migrate_connections(
    ledger: &WakeLedger,
    connections: &mut Vec<NwcConnection>,
    now: u64,
) -> anyhow::Result<MigrationResult> {
    let clock = MigrationClock(UnixTimestamp::from_secs(now));
    let manager = ConnectionManager::new(ledger, &clock);
    let imports = connections
        .iter()
        .map(|connection| {
            Ok(LegacyConnectionImport::new(
                new_connection(connection)?,
                UnixTimestamp::from_secs(connection.created_at),
                connection.spent_sat,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let migration = manager
        .migrate_legacy_batch(imports)
        .context("could not migrate the NWC connection registry")?;
    let revoked_ids = migration
        .revoked_connection_ids()
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect::<Vec<_>>();

    connections.retain(|connection| !revoked_ids.contains(&connection.id));
    Ok(MigrationResult {
        revoked_client_pubkeys: migration
            .revoked_client_pubkeys()
            .iter()
            .map(PublicKey::to_hex)
            .collect(),
    })
}

pub(crate) fn hydrate_connection_usage(
    ledger: &WakeLedger,
    connections: &mut [NwcConnection],
) -> anyhow::Result<()> {
    let usage = connections
        .iter()
        .map(|connection| {
            let id =
                ConnectionId::parse(connection.id.clone()).context("invalid NWC connection id")?;
            ledger
                .last_completed_event_at(&id)
                .context("could not read NWC connection usage")
                .map(|timestamp| timestamp.map(UnixTimestamp::as_secs))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (connection, last_used_at) in connections.iter_mut().zip(usage) {
        connection.last_used_at = last_used_at;
    }
    Ok(())
}

pub(crate) fn insert_connection(
    ledger: &WakeLedger,
    connection: &NwcConnection,
    now: u64,
) -> anyhow::Result<ActiveConnection> {
    if connection.spent_sat != 0 {
        return Err(anyhow!(
            "new NWC connection contains legacy accounting state"
        ));
    }
    let clock = MigrationClock(UnixTimestamp::from_secs(now));
    ConnectionManager::new(ledger, &clock)
        .import_legacy(
            new_connection(connection)?,
            UnixTimestamp::from_secs(connection.created_at),
            0,
        )
        .context("could not persist the NWC authorization")
}

pub(crate) fn approve_nwa_connection(
    ledger: &WakeLedger,
    request: NwaRequest,
    connection: &NwcConnection,
    lud16: Option<&str>,
    now: u64,
) -> anyhow::Result<ApprovedNwaConnection> {
    if connection.spent_sat != 0 {
        return Err(anyhow!("new NWA connection contains accounting state"));
    }
    let clock = MigrationClock(UnixTimestamp::from_secs(now));
    ConnectionManager::new(ledger, &clock)
        .approve_nwa_connection(
            request,
            new_connection(connection)?,
            lud16.map(str::to_owned),
        )
        .context("could not persist the NWA authorization")
}

pub(crate) fn tombstone_connection(
    ledger: &WakeLedger,
    connection: &NwcConnection,
    now: u64,
) -> anyhow::Result<()> {
    let clock = MigrationClock(UnixTimestamp::from_secs(now));
    ConnectionManager::new(ledger, &clock)
        .revoke_host_connection(&connection.id)
        .context("could not revoke the NWC authorization")?;
    Ok(())
}

fn new_connection(connection: &NwcConnection) -> anyhow::Result<NewConnection> {
    let relays = connection
        .relay
        .split(|character: char| character.is_whitespace() || character == ',')
        .map(str::trim)
        .filter(|relay| !relay.is_empty())
        .map(str::to_owned)
        .collect();
    let methods = connection
        .enabled_permissions()
        .into_iter()
        .filter_map(permission_method);
    let policy = ConnectionPolicy::new(
        methods,
        BudgetPolicy::new(
            connection.budget_sat,
            budget_interval(connection.budget_interval),
            FeePolicy::CountTowardBudget {
                maximum_fee_sat: maximum_nwc_fee_sat(connection.budget_sat),
            },
        ),
    );
    NewConnection::from_host_strings(
        connection.id.clone(),
        &connection.client_pubkey,
        &connection.service_pubkey,
        relays,
        policy,
        NWC_ENCRYPTION,
        WakePolicy::default(),
    )
    .map(|parsed| parsed.with_expiration(connection.expires_at.map(UnixTimestamp::from_secs)))
    .context("invalid NWC connection authorization")
}

pub(crate) fn permission_method(permission: NwcPermission) -> Option<NwcMethod> {
    match permission {
        NwcPermission::GetInfo => Some(NwcMethod::GetInfo),
        NwcPermission::GetBalance => Some(NwcMethod::GetBalance),
        NwcPermission::MakeInvoice => Some(NwcMethod::MakeInvoice),
        NwcPermission::PayInvoice => Some(NwcMethod::PayInvoice),
        NwcPermission::LookupInvoice => Some(NwcMethod::LookupInvoice),
        NwcPermission::ListTransactions => Some(NwcMethod::ListTransactions),
        NwcPermission::PayKeysend
        | NwcPermission::MakeHoldInvoice
        | NwcPermission::CancelHoldInvoice
        | NwcPermission::SettleHoldInvoice => None,
    }
}

const fn budget_interval(interval: NwcBudgetInterval) -> BudgetInterval {
    match interval {
        NwcBudgetInterval::Never => BudgetInterval::Never,
        NwcBudgetInterval::Hourly => BudgetInterval::Hourly,
        NwcBudgetInterval::Daily => BudgetInterval::Daily,
        NwcBudgetInterval::Weekly => BudgetInterval::Weekly,
        NwcBudgetInterval::Monthly => BudgetInterval::Monthly,
        NwcBudgetInterval::Yearly => BudgetInterval::Yearly,
    }
}

fn maximum_nwc_fee_sat(budget_sat: u64) -> u64 {
    if budget_sat == 0 {
        return 0;
    }
    let proportional = budget_sat / 20;
    proportional.clamp(10, 1_000).min(budget_sat)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nwc_mobile::{ClaimOutcome, EventId, NwaParsePolicy, TerminalKind};

    use super::*;

    const CLIENT: &str = "687dd8ece211539364549b1f32c63eceec1e0661009ba65cf8ff2e73ba000746";
    const WALLET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn connection(id: &str) -> NwcConnection {
        NwcConnection {
            id: id.to_string(),
            name: "Test".to_string(),
            icon_url: None,
            icon_display_url: None,
            relay: "wss://relay.example.com/nwc".to_string(),
            uri: String::new(),
            wallet_managed_secret: true,
            service_pubkey: WALLET.to_string(),
            client_pubkey: CLIENT.to_string(),
            budget_sat: 1_000,
            spent_sat: 400,
            budget_display: "1,000 sats".to_string(),
            spent_display: "400 sats".to_string(),
            budget_interval: NwcBudgetInterval::Daily,
            budget_interval_display: "Daily".to_string(),
            permissions: vec![NwcPermission::GetInfo, NwcPermission::PayInvoice],
            permissions_configured: true,
            allow_get_balance: false,
            allow_pay_invoice: true,
            created_at: 100,
            last_used_at: None,
            expires_at: Some(300),
            budget_period_started_at: 100,
            pending_info_event_relays: Vec::new(),
        }
    }

    #[test]
    fn migration_is_idempotent_and_preserves_existing_budget_usage() {
        let directory = tempfile::tempdir().expect("directory");
        let ledger = WakeLedger::open(directory.path().join("mobile.sqlite3")).expect("ledger");
        let mut connections = vec![connection("nwc-test")];

        migrate_connections(&ledger, &mut connections, 200).expect("first migration");
        migrate_connections(&ledger, &mut connections, 200).expect("repeat migration");

        let active = ledger
            .load_active_connection(&ConnectionId::parse("nwc-test").expect("id"))
            .expect("load")
            .expect("active");
        assert_eq!(active.created_at(), UnixTimestamp::from_secs(100));
        assert_eq!(active.expires_at(), Some(UnixTimestamp::from_secs(300)));
        assert!(active.policy().allows(NwcMethod::PayInvoice));
        assert_eq!(
            active.policy().budget().fee_policy(),
            FeePolicy::CountTowardBudget {
                maximum_fee_sat: 50
            }
        );
        assert_eq!(connections.len(), 1);
    }

    #[test]
    fn nwa_approval_uses_shared_authority_validation_and_callback() {
        let directory = tempfile::tempdir().expect("directory");
        let ledger = WakeLedger::open(directory.path().join("mobile.sqlite3")).expect("ledger");
        let request = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com%2Fnwc&max_amount=1000000&budget_renewal=daily&request_methods=get_info+pay_invoice&return_to=https%3A%2F%2Fapp.example.com%2Fnwa&state=0123456789abcdef0123456789abcdef"
            ),
            UnixTimestamp::from_secs(100),
            &NwaParsePolicy::default(),
        )
        .expect("request");
        let mut connection = connection("nwc-nwa");
        connection.spent_sat = 0;
        connection.expires_at = None;

        let approved =
            approve_nwa_connection(&ledger, request, &connection, Some("name@example.com"), 200)
                .expect("approval");

        assert_eq!(approved.connection().id().as_str(), "nwc-nwa");
        let callback = approved.callback_url().expect("callback");
        assert!(callback.starts_with("https://app.example.com/nwa#"));
        assert!(callback.contains("status=approved"));
        assert!(!callback.contains("secret="));
    }

    #[test]
    fn migration_removes_a_permanently_revoked_legacy_record() {
        let directory = tempfile::tempdir().expect("directory");
        let ledger = WakeLedger::open(directory.path().join("mobile.sqlite3")).expect("ledger");
        let mut original = connection("nwc-revoked");
        original.spent_sat = 0;
        let active = insert_connection(&ledger, &original, 100).expect("insert");
        ledger
            .tombstone_connection(
                active.id(),
                active.revision(),
                UnixTimestamp::from_secs(101),
            )
            .expect("tombstone");
        let mut connections = vec![original];

        let result = migrate_connections(&ledger, &mut connections, 200).expect("migration");

        assert!(connections.is_empty());
        assert_eq!(result.revoked_client_pubkeys, vec![CLIENT.to_string()]);
    }

    #[test]
    fn connection_usage_is_hydrated_from_successful_durable_events() {
        let directory = tempfile::tempdir().expect("directory");
        let ledger = WakeLedger::open(directory.path().join("mobile.sqlite3")).expect("ledger");
        let mut original = connection("nwc-used");
        original.spent_sat = 0;
        let active = insert_connection(&ledger, &original, 100).expect("insert");
        let event_id = EventId::from_hex(CLIENT).expect("event id");
        let ClaimOutcome::Acquired(lease) = ledger
            .claim_event(
                &event_id,
                active.id(),
                active.revision(),
                UnixTimestamp::from_secs(200),
                Duration::from_secs(10),
            )
            .expect("claim")
        else {
            panic!("claim not acquired");
        };
        ledger
            .complete_event(
                &lease,
                TerminalKind::Completed,
                Some("encrypted-response"),
                UnixTimestamp::from_secs(205),
            )
            .expect("complete");
        let mut connections = vec![original];

        hydrate_connection_usage(&ledger, &mut connections).expect("hydrate usage");

        assert_eq!(connections[0].last_used_at, Some(205));
    }

    #[test]
    fn insecure_legacy_relays_fail_closed() {
        let mut legacy = connection("nwc-insecure");
        legacy.relay = "ws://relay.example.com".to_string();
        assert!(new_connection(&legacy).is_err());
    }

    #[test]
    fn relay_path_trailing_slashes_remain_part_of_the_allowlist() {
        let mut legacy = connection("nwc-relay-path");
        legacy.relay = "wss://relay.example.com/nwc/".to_string();
        let parsed = new_connection(&legacy).expect("connection");
        assert_eq!(parsed.relays()[0].as_str(), legacy.relay);
    }

    #[test]
    fn fee_cap_is_bounded_and_never_exceeds_the_connection_budget() {
        assert_eq!(maximum_nwc_fee_sat(0), 0);
        assert_eq!(maximum_nwc_fee_sat(5), 5);
        assert_eq!(maximum_nwc_fee_sat(1_000), 50);
        assert_eq!(maximum_nwc_fee_sat(100_000), 1_000);
    }
}
