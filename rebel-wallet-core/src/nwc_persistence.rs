use anyhow::Context;
use nwc_mobile::ConnectionPresentation;
use serde::{Deserialize, Serialize};

use crate::{NwcBudgetInterval, NwcConnection, NwcPermission};

/// Rebel-owned presentation metadata that is not part of NWC authorization.
///
/// Public keys, relays, permissions, budgets, and lifecycle timestamps are
/// rebuilt from the authoritative `nwc-mobile` ledger on every launch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedNwcMetadata {
    pub(crate) connection_id: String,
    pub(crate) name: String,
    pub(crate) icon_url: Option<String>,
    pub(crate) spent_sat: u64,
    pub(crate) budget_period_started_at: u64,
    pub(crate) pending_info_event_relays: Vec<String>,
}

impl From<&NwcConnection> for PersistedNwcMetadata {
    fn from(connection: &NwcConnection) -> Self {
        Self {
            connection_id: connection.id.clone(),
            name: connection.name.clone(),
            icon_url: connection.icon_url.clone(),
            spent_sat: connection.spent_sat,
            budget_period_started_at: connection.budget_period_started_at,
            pending_info_event_relays: connection.pending_info_event_relays.clone(),
        }
    }
}

pub(crate) fn connection_view(
    presentation: ConnectionPresentation,
    metadata: Option<PersistedNwcMetadata>,
    fallback_name: String,
    wallet_managed_secret: bool,
) -> anyhow::Result<NwcConnection> {
    let created_at = presentation.created_at().as_secs();
    let relay_urls = presentation.relay_urls().to_vec();
    let (name, icon_url, spent_sat, budget_period_started_at, pending_info_event_relays) = metadata
        .map_or_else(
            || (fallback_name, None, 0, created_at, relay_urls.clone()),
            |metadata| {
                (
                    metadata.name,
                    metadata.icon_url,
                    metadata.spent_sat,
                    metadata.budget_period_started_at,
                    metadata.pending_info_event_relays,
                )
            },
        );
    let budget_interval = NwcBudgetInterval::try_from(presentation.budget_interval())
        .context("unsupported NWC budget interval")?;
    let permissions = presentation
        .methods()
        .iter()
        .copied()
        .map(NwcPermission::try_from)
        .collect::<Result<Vec<_>, _>>()
        .context("unsupported NWC permission")?;

    Ok(NwcConnection {
        id: presentation.id().to_owned(),
        name,
        icon_url,
        icon_display_url: None,
        relay: presentation.relay_storage(),
        wallet_managed_secret,
        service_pubkey: presentation.wallet_service_pubkey_hex().to_owned(),
        client_pubkey: presentation.client_pubkey_hex().to_owned(),
        budget_sat: presentation.budget_limit_sat(),
        spent_sat,
        budget_display: String::new(),
        spent_display: String::new(),
        budget_interval,
        budget_interval_display: String::new(),
        permissions,
        created_at,
        last_used_at: presentation
            .last_used_at()
            .map(|timestamp| timestamp.as_secs()),
        expires_at: presentation
            .expires_at()
            .map(|timestamp| timestamp.as_secs()),
        budget_period_started_at,
        pending_info_event_relays,
    })
}
