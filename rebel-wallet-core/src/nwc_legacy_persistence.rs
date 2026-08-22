use serde::{Deserialize, Serialize};

use crate::{NwcBudgetInterval, NwcConnection, NwcPermission};

/// Compatibility-only representation of Rebel's pre-`nwc-mobile` registry.
///
/// This is deliberately app-owned: it mirrors Rebel's historical JSON schema
/// solely so existing installs can migrate into the `nwc-mobile` ledger.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedNwcConnection {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) icon_url: Option<String>,
    #[serde(default, skip_serializing)]
    pub(crate) icon_display_url: Option<String>,
    pub(crate) relay: String,
    #[serde(default, skip_serializing)]
    pub(crate) uri: String,
    #[serde(default)]
    pub(crate) wallet_managed_secret: bool,
    pub(crate) service_pubkey: String,
    pub(crate) client_pubkey: String,
    pub(crate) budget_sat: u64,
    pub(crate) spent_sat: u64,
    pub(crate) budget_display: String,
    pub(crate) spent_display: String,
    #[serde(default)]
    pub(crate) budget_interval: PersistedNwcBudgetInterval,
    #[serde(default)]
    pub(crate) budget_interval_display: String,
    #[serde(default)]
    pub(crate) permissions: Vec<PersistedNwcPermission>,
    #[serde(default)]
    pub(crate) permissions_configured: bool,
    pub(crate) allow_get_balance: bool,
    pub(crate) allow_pay_invoice: bool,
    pub(crate) created_at: u64,
    #[serde(default, skip)]
    pub(crate) last_used_at: Option<u64>,
    #[serde(default)]
    pub(crate) expires_at: Option<u64>,
    #[serde(default)]
    pub(crate) budget_period_started_at: u64,
    #[serde(default)]
    pub(crate) pending_info_event_relays: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) enum PersistedNwcBudgetInterval {
    #[default]
    Never,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum PersistedNwcPermission {
    PayInvoice,
    PayKeysend,
    MakeInvoice,
    LookupInvoice,
    ListTransactions,
    GetBalance,
    GetInfo,
    MakeHoldInvoice,
    CancelHoldInvoice,
    SettleHoldInvoice,
}

impl PersistedNwcConnection {
    pub(crate) fn into_view(self) -> NwcConnection {
        let permissions = if self.permissions_configured {
            self.permissions
                .into_iter()
                .filter_map(permission_from_persisted)
                .collect()
        } else {
            let mut permissions = vec![NwcPermission::GetInfo];
            if self.allow_get_balance {
                permissions.push(NwcPermission::GetBalance);
            }
            if self.allow_pay_invoice {
                permissions.push(NwcPermission::PayInvoice);
            }
            permissions
        };
        NwcConnection {
            id: self.id,
            name: self.name,
            icon_url: self.icon_url,
            icon_display_url: self.icon_display_url,
            relay: self.relay,
            wallet_managed_secret: self.wallet_managed_secret,
            service_pubkey: self.service_pubkey,
            client_pubkey: self.client_pubkey,
            budget_sat: self.budget_sat,
            spent_sat: self.spent_sat,
            budget_display: self.budget_display,
            spent_display: self.spent_display,
            budget_interval: self.budget_interval.into(),
            budget_interval_display: self.budget_interval_display,
            permissions,
            created_at: self.created_at,
            last_used_at: self.last_used_at,
            expires_at: self.expires_at,
            budget_period_started_at: self.budget_period_started_at,
            pending_info_event_relays: self.pending_info_event_relays,
        }
    }
}

impl From<PersistedNwcBudgetInterval> for NwcBudgetInterval {
    fn from(interval: PersistedNwcBudgetInterval) -> Self {
        match interval {
            PersistedNwcBudgetInterval::Never => Self::Never,
            PersistedNwcBudgetInterval::Hourly => Self::Hourly,
            PersistedNwcBudgetInterval::Daily => Self::Daily,
            PersistedNwcBudgetInterval::Weekly => Self::Weekly,
            PersistedNwcBudgetInterval::Monthly => Self::Monthly,
            PersistedNwcBudgetInterval::Yearly => Self::Yearly,
        }
    }
}

impl From<NwcBudgetInterval> for PersistedNwcBudgetInterval {
    fn from(interval: NwcBudgetInterval) -> Self {
        match interval {
            NwcBudgetInterval::Never => Self::Never,
            NwcBudgetInterval::Hourly => Self::Hourly,
            NwcBudgetInterval::Daily => Self::Daily,
            NwcBudgetInterval::Weekly => Self::Weekly,
            NwcBudgetInterval::Monthly => Self::Monthly,
            NwcBudgetInterval::Yearly => Self::Yearly,
        }
    }
}

impl From<NwcPermission> for PersistedNwcPermission {
    fn from(permission: NwcPermission) -> Self {
        match permission {
            NwcPermission::PayInvoice => Self::PayInvoice,
            NwcPermission::MakeInvoice => Self::MakeInvoice,
            NwcPermission::LookupInvoice => Self::LookupInvoice,
            NwcPermission::ListTransactions => Self::ListTransactions,
            NwcPermission::GetBalance => Self::GetBalance,
            NwcPermission::GetInfo => Self::GetInfo,
        }
    }
}

fn permission_from_persisted(permission: PersistedNwcPermission) -> Option<NwcPermission> {
    Some(match permission {
        PersistedNwcPermission::PayInvoice => NwcPermission::PayInvoice,
        PersistedNwcPermission::MakeInvoice => NwcPermission::MakeInvoice,
        PersistedNwcPermission::LookupInvoice => NwcPermission::LookupInvoice,
        PersistedNwcPermission::ListTransactions => NwcPermission::ListTransactions,
        PersistedNwcPermission::GetBalance => NwcPermission::GetBalance,
        PersistedNwcPermission::GetInfo => NwcPermission::GetInfo,
        PersistedNwcPermission::PayKeysend
        | PersistedNwcPermission::MakeHoldInvoice
        | PersistedNwcPermission::CancelHoldInvoice
        | PersistedNwcPermission::SettleHoldInvoice => return None,
    })
}

impl From<&NwcConnection> for PersistedNwcConnection {
    fn from(connection: &NwcConnection) -> Self {
        Self {
            id: connection.id.clone(),
            name: connection.name.clone(),
            icon_url: connection.icon_url.clone(),
            icon_display_url: None,
            relay: connection.relay.clone(),
            uri: String::new(),
            wallet_managed_secret: connection.wallet_managed_secret,
            service_pubkey: connection.service_pubkey.clone(),
            client_pubkey: connection.client_pubkey.clone(),
            budget_sat: connection.budget_sat,
            spent_sat: connection.spent_sat,
            budget_display: connection.budget_display.clone(),
            spent_display: connection.spent_display.clone(),
            budget_interval: connection.budget_interval.into(),
            budget_interval_display: connection.budget_interval_display.clone(),
            permissions: connection
                .permissions
                .iter()
                .copied()
                .map(Into::into)
                .collect(),
            permissions_configured: true,
            allow_get_balance: connection.permissions.contains(&NwcPermission::GetBalance),
            allow_pay_invoice: connection.permissions.contains(&NwcPermission::PayInvoice),
            created_at: connection.created_at,
            last_used_at: None,
            expires_at: connection.expires_at,
            budget_period_started_at: connection.budget_period_started_at,
            pending_info_event_relays: connection.pending_info_event_relays.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_migration_only_and_legacy_budgets_are_lifetime() {
        let connection = NwcConnection {
            id: "nwc-client".to_string(),
            name: "Client".to_string(),
            icon_url: None,
            icon_display_url: None,
            relay: "wss://relay.example.com".to_string(),
            wallet_managed_secret: false,
            service_pubkey: "service".to_string(),
            client_pubkey: "client".to_string(),
            budget_sat: 1_000,
            spent_sat: 100,
            budget_display: "1,000 sats".to_string(),
            spent_display: "100 sats".to_string(),
            budget_interval: NwcBudgetInterval::Daily,
            budget_interval_display: "Daily".to_string(),
            permissions: vec![NwcPermission::GetInfo],
            created_at: 1,
            last_used_at: None,
            expires_at: None,
            budget_period_started_at: 1,
            pending_info_event_relays: Vec::new(),
        };

        let persisted_connection = PersistedNwcConnection::from(&connection);
        let mut persisted =
            serde_json::to_value(&persisted_connection).expect("serialize connection");
        assert!(persisted.get("uri").is_none());

        let object = persisted.as_object_mut().expect("connection object");
        object.insert(
            "uri".to_string(),
            serde_json::Value::String("nostr+walletconnect://legacy-secret".to_string()),
        );
        object.remove("budget_interval");
        let legacy: PersistedNwcConnection =
            serde_json::from_value(persisted).expect("deserialize legacy connection");

        assert_eq!(
            legacy.uri, "nostr+walletconnect://legacy-secret",
            "legacy URI must remain available for one-time Keychain migration"
        );
        let migrated = legacy.into_view();
        assert_eq!(migrated.budget_interval, NwcBudgetInterval::Never);
    }
}
