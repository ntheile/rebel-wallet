use serde::{Deserialize, Serialize};

use crate::{
    NostrState, NwcBudgetInterval, NwcConnection, NwcPermission, PriceCurrency, WalletNetwork,
    WalletState,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedAppData {
    pub(crate) nostr: NostrState,
    pub(crate) receive_amount_sat: u64,
    pub(crate) receive_memo: String,
    #[serde(default)]
    pub(crate) network: WalletNetwork,
    #[serde(default = "default_server_config")]
    pub(crate) servers: ServerConfig,
    #[serde(default = "default_price_currency")]
    pub(crate) price_currency: PersistedPriceCurrency,
    #[serde(default, skip_serializing)]
    pub(crate) lightning_address_ark_address: Option<String>,
    #[serde(default)]
    pub(crate) custom_lightning_address: Option<String>,
    #[serde(default)]
    pub(crate) custom_lightning_address_name: String,
    #[serde(default)]
    pub(crate) pending_custom_lightning_address: Option<PendingCustomLightningAddress>,
    #[serde(default)]
    pub(crate) payment_annotations: Vec<PaymentAnnotation>,
    #[serde(default)]
    pub(crate) zap_receipts: Vec<ZapReceiptRecord>,
    #[serde(default)]
    pub(crate) nwc_connections: Vec<PersistedNwcConnection>,
}

/// Compatibility-only representation of Rebel's pre-`nwc-mobile` registry.
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PendingCustomLightningAddress {
    pub(crate) name: String,
    pub(crate) lightning_address: String,
    pub(crate) ark_address: String,
    #[serde(default)]
    pub(crate) payment_ark_address: Option<String>,
    pub(crate) invoice: String,
    #[serde(alias = "payment_hash")]
    pub(crate) purchase_id: String,
    pub(crate) amount_msats: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PaymentAnnotation {
    pub(crate) contact_id: Option<String>,
    #[serde(default)]
    pub(crate) label: Option<String>,
    pub(crate) destination: String,
    pub(crate) invoice: Option<String>,
    pub(crate) payment_hash: Option<String>,
    pub(crate) amount_sat: i64,
    pub(crate) outbound: bool,
    pub(crate) zap: bool,
    pub(crate) created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ZapReceiptRecord {
    pub(crate) event_id: String,
    pub(crate) sender_pubkey: String,
    pub(crate) recipient_pubkey: String,
    pub(crate) invoice: Option<String>,
    pub(crate) payment_hash: Option<String>,
    pub(crate) amount_msat: Option<u64>,
    #[serde(default)]
    pub(crate) lnurl: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct PersistedPriceCurrency {
    #[serde(with = "price_currency_serde")]
    pub(crate) currency: PriceCurrency,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServerConfig {
    #[serde(default)]
    pub(crate) network: WalletNetwork,
    pub(crate) server_address: String,
    #[serde(skip)]
    pub(crate) server_access_token: Option<String>,
    pub(crate) esplora_address: String,
}

impl ServerConfig {
    pub(crate) fn for_network(network: WalletNetwork) -> Self {
        Self {
            network,
            server_address: network.server_address().to_string(),
            server_access_token: network.server_access_token().map(str::to_string),
            esplora_address: network.esplora_address().to_string(),
        }
    }

    pub(crate) fn from_wallet(wallet: &WalletState) -> Self {
        Self {
            network: wallet.network,
            server_address: wallet.server_address.clone(),
            server_access_token: wallet.network.server_access_token().map(str::to_string),
            esplora_address: wallet.esplora_address.clone(),
        }
    }
}

fn default_server_config() -> ServerConfig {
    ServerConfig::for_network(WalletNetwork::default())
}

fn default_price_currency() -> PersistedPriceCurrency {
    PersistedPriceCurrency {
        currency: PriceCurrency::BTC,
    }
}

mod price_currency_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::PriceCurrency;

    pub(super) fn serialize<S>(currency: &PriceCurrency, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(currency.code())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<PriceCurrency, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.to_ascii_uppercase().as_str() {
            "BTC" => Ok(PriceCurrency::BTC),
            "USD" => Ok(PriceCurrency::USD),
            "EUR" => Ok(PriceCurrency::EUR),
            "GBP" => Ok(PriceCurrency::GBP),
            _ => Ok(PriceCurrency::BTC),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_data_defaults_missing_arkzap_addresses() {
        let raw = r#"{
            "nostr": {
                "npub": null,
                "name": "Rebel",
                "about": "",
                "picture": "",
                "lud16": "",
                "nip05": "",
                "contacts": []
            },
            "receive_amount_sat": 0,
            "receive_memo": "",
            "servers": {
                "server_address": "https://ark.example.com",
                "esplora_address": "https://esplora.example.com"
            },
            "price_currency": "BTC"
        }"#;

        let data: PersistedAppData = serde_json::from_str(raw).unwrap();

        assert_eq!(data.network, WalletNetwork::Mainnet);
        assert_eq!(data.lightning_address_ark_address, None);
        assert_eq!(data.custom_lightning_address, None);
        assert_eq!(data.custom_lightning_address_name, "");
        assert!(data.pending_custom_lightning_address.is_none());
        assert!(data.payment_annotations.is_empty());
        assert!(data.zap_receipts.is_empty());
        assert!(data.nwc_connections.is_empty());
        assert!(!data.nostr.deleted);
    }

    #[test]
    fn app_data_defaults_network_and_servers_to_mainnet() {
        let raw = r#"{
            "nostr": {
                "npub": null,
                "name": "Rebel",
                "about": "",
                "picture": "",
                "lud16": "",
                "nip05": "",
                "contacts": []
            },
            "receive_amount_sat": 0,
            "receive_memo": "",
            "price_currency": "BTC"
        }"#;

        let data: PersistedAppData = serde_json::from_str(raw).unwrap();
        let mainnet = WalletNetwork::default();

        assert_eq!(mainnet, WalletNetwork::Mainnet);
        assert_eq!(data.network, mainnet);
        assert_eq!(data.servers, ServerConfig::for_network(mainnet));
    }
}
