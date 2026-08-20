use anyhow::{anyhow, Context};
use nwc_mobile::{
    ActiveConnection, BudgetInterval, BudgetPolicy, ConnectionId, ConnectionPolicy, FeePolicy,
    NewConnection, NwcEncryption, NwcMethod, PublicKey, SecureRelayUrl, StoredConnection,
    UnixTimestamp, WakeLedger, WakePolicy,
};

use crate::{NwcBudgetInterval, NwcConnection, NwcPermission};

pub(crate) struct MigrationResult {
    pub(crate) revoked_client_pubkeys: Vec<String>,
}

pub(crate) fn migrate_connections(
    ledger: &WakeLedger,
    connections: &mut Vec<NwcConnection>,
    now: u64,
) -> anyhow::Result<MigrationResult> {
    let mut revoked_ids = Vec::new();
    let mut revoked_client_pubkeys = Vec::new();

    for connection in connections.iter() {
        let specification = RegistryConnection::try_from(connection)?;
        match ledger
            .load_connection(&specification.id)
            .context("could not read the NWC connection registry")?
        {
            None => {
                ledger
                    .import_connection(
                        specification.new_connection()?,
                        UnixTimestamp::from_secs(connection.created_at),
                        connection.spent_sat,
                        UnixTimestamp::from_secs(now),
                    )
                    .context("could not import an existing NWC connection")?;
            }
            Some(StoredConnection::Active(active)) => {
                if !specification.matches(&active, connection.created_at) {
                    return Err(anyhow!(
                        "persisted NWC connection does not match its durable authorization"
                    ));
                }
            }
            Some(StoredConnection::Tombstoned(_)) => {
                revoked_ids.push(connection.id.clone());
                revoked_client_pubkeys.push(connection.client_pubkey.clone());
            }
            Some(_) => return Err(anyhow!("unsupported NWC authorization state")),
        }
    }

    connections.retain(|connection| !revoked_ids.contains(&connection.id));
    Ok(MigrationResult {
        revoked_client_pubkeys,
    })
}

pub(crate) fn insert_connection(
    ledger: &WakeLedger,
    connection: &NwcConnection,
    now: u64,
) -> anyhow::Result<ActiveConnection> {
    let specification = RegistryConnection::try_from(connection)?;
    ledger
        .import_connection(
            specification.new_connection()?,
            UnixTimestamp::from_secs(connection.created_at),
            connection.spent_sat,
            UnixTimestamp::from_secs(now),
        )
        .context("could not persist the NWC authorization")
}

pub(crate) fn tombstone_connection(
    ledger: &WakeLedger,
    connection: &NwcConnection,
    now: u64,
) -> anyhow::Result<()> {
    let id = ConnectionId::parse(connection.id.clone()).context("invalid NWC connection id")?;
    match ledger
        .load_connection(&id)
        .context("could not read the NWC connection registry")?
    {
        Some(StoredConnection::Active(active)) => {
            ledger
                .tombstone_connection(&id, active.revision(), UnixTimestamp::from_secs(now))
                .context("could not revoke the NWC authorization")?;
            Ok(())
        }
        Some(StoredConnection::Tombstoned(_)) => Ok(()),
        Some(_) => Err(anyhow!("unsupported NWC authorization state")),
        None => Err(anyhow!("NWC authorization was not found in the registry")),
    }
}

struct RegistryConnection {
    id: ConnectionId,
    client_pubkey: PublicKey,
    wallet_service_pubkey: PublicKey,
    relays: Vec<SecureRelayUrl>,
    policy: ConnectionPolicy,
    expires_at: Option<u64>,
}

impl RegistryConnection {
    fn new_connection(&self) -> anyhow::Result<NewConnection> {
        NewConnection::new(
            self.id.clone(),
            self.client_pubkey.clone(),
            self.wallet_service_pubkey.clone(),
            self.relays.clone(),
            self.policy.clone(),
            NwcEncryption::LegacyNip04,
            WakePolicy::default(),
        )
        .map(|connection| connection.with_expiration(self.expires_at.map(UnixTimestamp::from_secs)))
        .context("invalid NWC connection authorization")
    }

    fn matches(&self, active: &ActiveConnection, created_at: u64) -> bool {
        active.id() == &self.id
            && active.client_pubkey() == &self.client_pubkey
            && active.wallet_service_pubkey() == &self.wallet_service_pubkey
            && active.relays() == self.relays
            && active.policy() == &self.policy
            && active.encryption() == NwcEncryption::LegacyNip04
            && active.created_at() == UnixTimestamp::from_secs(created_at)
            && active.expires_at() == self.expires_at.map(UnixTimestamp::from_secs)
    }
}

impl TryFrom<&NwcConnection> for RegistryConnection {
    type Error = anyhow::Error;

    fn try_from(connection: &NwcConnection) -> Result<Self, Self::Error> {
        let id = ConnectionId::parse(connection.id.clone()).context("invalid NWC connection id")?;
        let client_pubkey = PublicKey::from_hex(&connection.client_pubkey)
            .context("invalid NWC client public key")?;
        let wallet_service_pubkey = PublicKey::from_hex(&connection.service_pubkey)
            .context("invalid NWC wallet-service public key")?;
        let relays = connection
            .relay
            .split(|character: char| character.is_whitespace() || character == ',')
            .map(str::trim)
            .filter(|relay| !relay.is_empty())
            .map(SecureRelayUrl::parse)
            .collect::<Result<Vec<_>, _>>()
            .context("invalid or insecure NWC relay")?;
        let methods = connection
            .enabled_permissions()
            .into_iter()
            .filter_map(permission_method)
            .collect::<Vec<_>>();
        let policy = ConnectionPolicy::new(
            methods,
            BudgetPolicy::new(
                connection.budget_sat,
                budget_interval(connection.budget_interval),
                FeePolicy::ExcludeForCompatibility,
            ),
        );
        Ok(Self {
            id,
            client_pubkey,
            wallet_service_pubkey,
            relays,
            policy,
            expires_at: connection.expires_at,
        })
    }
}

fn permission_method(permission: NwcPermission) -> Option<NwcMethod> {
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

#[cfg(test)]
mod tests {
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
        assert_eq!(connections.len(), 1);
    }

    #[test]
    fn migration_removes_a_permanently_revoked_legacy_record() {
        let directory = tempfile::tempdir().expect("directory");
        let ledger = WakeLedger::open(directory.path().join("mobile.sqlite3")).expect("ledger");
        let original = connection("nwc-revoked");
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
    fn insecure_legacy_relays_fail_closed() {
        let mut legacy = connection("nwc-insecure");
        legacy.relay = "ws://relay.example.com".to_string();
        assert!(RegistryConnection::try_from(&legacy).is_err());
    }

    #[test]
    fn relay_path_trailing_slashes_remain_part_of_the_allowlist() {
        let mut legacy = connection("nwc-relay-path");
        legacy.relay = "wss://relay.example.com/nwc/".to_string();
        let specification = RegistryConnection::try_from(&legacy).expect("connection");
        assert_eq!(specification.relays[0].as_str(), legacy.relay);
    }
}
