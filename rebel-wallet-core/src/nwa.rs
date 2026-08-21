use std::fmt;

use anyhow::Context;
use nwc_mobile::{
    BudgetInterval, NwaParsePolicy, NwaRequest as MobileNwaRequest, NwcMethod, PublicKey,
    UnixTimestamp,
};

use crate::{NwaRequestState, NwcBudgetInterval, NwcPermission};

#[derive(Clone)]
pub(crate) struct NwaRequest {
    pub(crate) state: NwaRequestState,
    inner: MobileNwaRequest,
}

impl NwaRequest {
    pub(crate) fn parse(input: &str, now: u64) -> anyhow::Result<Self> {
        let inner = MobileNwaRequest::parse(
            input,
            UnixTimestamp::from_secs(now),
            &NwaParsePolicy::default(),
        )
        .context("Nostr Wallet Auth request rejected")?;
        let requested_policy = inner.requested_policy();
        let budget = requested_policy.budget();
        let callback = inner.callback();
        let state = NwaRequestState {
            id: inner.id().to_hex(),
            client_pubkey: inner.client_pubkey().to_hex(),
            display_name: inner.display_name().to_string(),
            icon_url: inner.icon_url().map(ToString::to_string),
            icon_display_url: None,
            requesting_app_description: callback
                .and_then(|callback| callback.url().host_str().map(str::to_string)),
            callback_target_description: callback
                .map(|callback| callback.target_description())
                .unwrap_or_else(|| "none".to_string()),
            relay: inner.relays().join("\n"),
            budget_sat: budget.limit_sat(),
            budget_interval: budget_interval(budget.interval()),
            permissions: requested_policy.methods().filter_map(permission).collect(),
            expires_at: inner.expires_at().map(|timestamp| timestamp.as_secs()),
        };
        Ok(Self { state, inner })
    }

    pub(crate) fn approved_callback(
        &self,
        wallet_pubkey: &str,
        relays: &[String],
        lud16: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let Some(callback) = self.inner.callback() else {
            return Ok(None);
        };
        let wallet_pubkey =
            PublicKey::from_hex(wallet_pubkey).context("invalid NWC wallet-service public key")?;
        callback
            .approved_url(&wallet_pubkey, relays, lud16)
            .map(|url| Some(url.to_string()))
            .context("could not build NWA approval callback")
    }

    pub(crate) fn cancelled_callback(&self) -> anyhow::Result<Option<String>> {
        self.inner
            .callback()
            .map(|callback| callback.cancelled_url().map(|url| url.to_string()))
            .transpose()
            .context("could not build NWA cancellation callback")
    }
}

impl fmt::Debug for NwaRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NwaRequest")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

const fn budget_interval(interval: BudgetInterval) -> NwcBudgetInterval {
    match interval {
        BudgetInterval::Never => NwcBudgetInterval::Never,
        BudgetInterval::Hourly => NwcBudgetInterval::Hourly,
        BudgetInterval::Daily => NwcBudgetInterval::Daily,
        BudgetInterval::Weekly => NwcBudgetInterval::Weekly,
        BudgetInterval::Monthly => NwcBudgetInterval::Monthly,
        BudgetInterval::Yearly => NwcBudgetInterval::Yearly,
        _ => NwcBudgetInterval::Never,
    }
}

const fn permission(method: NwcMethod) -> Option<NwcPermission> {
    match method {
        NwcMethod::GetInfo => Some(NwcPermission::GetInfo),
        NwcMethod::GetBalance => Some(NwcPermission::GetBalance),
        NwcMethod::MakeInvoice => Some(NwcPermission::MakeInvoice),
        NwcMethod::PayInvoice => Some(NwcPermission::PayInvoice),
        NwcMethod::LookupInvoice => Some(NwcPermission::LookupInvoice),
        NwcMethod::ListTransactions => Some(NwcPermission::ListTransactions),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: &str = "687dd8ece211539364549b1f32c63eceec1e0661009ba65cf8ff2e73ba000746";
    const WALLET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const STATE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn maps_validated_mobile_request_to_rebel_state() {
        let request = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.getalby.com%2Fnwc%2F&max_amount=500000000&budget_renewal=monthly&request_methods=get_info+pay_invoice&name=Alby+Go&icon=https%3A%2F%2Fexample.com%2Falby.png"
            ),
            100,
        )
        .expect("request");

        assert_eq!(request.state.client_pubkey, CLIENT);
        assert_eq!(request.state.display_name, "Alby Go");
        assert_eq!(request.state.budget_sat, 500_000);
        assert_eq!(request.state.budget_interval, NwcBudgetInterval::Monthly);
        assert_eq!(
            request.state.permissions,
            vec![NwcPermission::GetInfo, NwcPermission::PayInvoice]
        );
        assert_eq!(request.state.relay, "wss://relay.getalby.com/nwc/");
        assert_eq!(request.state.id.len(), 32);
    }

    #[test]
    fn omitted_authority_is_read_only_and_zero_budget() {
        let request = NwaRequest::parse(
            &format!("nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com"),
            100,
        )
        .expect("request");
        assert_eq!(request.state.budget_sat, 0);
        assert_eq!(request.state.permissions, vec![NwcPermission::GetInfo]);
    }

    #[test]
    fn only_https_app_link_callbacks_are_accepted() {
        let custom_scheme = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&return_to=example%3A%2F%2Fnwa&state={STATE}"
            ),
            100,
        );
        assert!(custom_scheme.is_err());

        let request = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&return_to=https%3A%2F%2Fapp.example.com%2Fnwa&state={STATE}"
            ),
            100,
        )
        .expect("https callback");
        let callback = request
            .approved_callback(
                WALLET,
                &["wss://relay.example.com".to_string()],
                Some("name@example.com"),
            )
            .expect("callback")
            .expect("callback URL");
        assert!(callback.starts_with("https://app.example.com/nwa#"));
        assert!(callback.contains("status=approved"));
        assert!(!callback.contains("secret="));
    }

    #[test]
    fn rejects_fractional_satoshi_unknown_methods_and_excess_lifetime() {
        for query in [
            "max_amount=1001",
            "request_methods=get_info+unknown_method",
            "expires_at=3000000",
        ] {
            let result = NwaRequest::parse(
                &format!("nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&{query}"),
                100,
            );
            assert!(result.is_err(), "accepted {query}");
        }
    }
}
