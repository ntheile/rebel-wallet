use std::collections::{HashMap, HashSet};

use anyhow::Context;
use reqwest::Url;

use crate::{NwaRequestState, NwcBudgetInterval, NwcPermission};

const MAX_REQUEST_LENGTH: usize = 8192;
const MAX_CALLBACK_LENGTH: usize = 2048;
const MIN_STATE_LENGTH: usize = 32;
const MAX_STATE_LENGTH: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct NwaRequest {
    pub(crate) state: NwaRequestState,
    return_to: Option<Url>,
    callback_state: Option<String>,
}

impl NwaRequest {
    pub(crate) fn parse(input: &str, now: u64) -> anyhow::Result<Self> {
        if input.len() > MAX_REQUEST_LENGTH {
            anyhow::bail!("NWA request is too large");
        }

        let url = Url::parse(input).context("not a valid NWA URL")?;
        if !matches!(
            url.scheme(),
            "nostr+walletauth" | "nostr+walletauth+rebelwallet"
        ) {
            anyhow::bail!("not an NWA URL");
        }

        let client_pubkey = url
            .host_str()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if client_pubkey.len() != 64 || !client_pubkey.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("NWA requires a valid client public key in the URI authority");
        }

        let query = NwaQuery::new(&url);
        if query.has_duplicate_single_value_parameters(&["relay"]) {
            anyhow::bail!("duplicate NWA parameter");
        }
        if query.value("version").unwrap_or("1") != "1" {
            anyhow::bail!("unsupported NWA version");
        }
        if query.value("pubkey").is_some() {
            anyhow::bail!("NWA client public key must be in the URI authority");
        }
        if !query
            .value("secret_mode")
            .unwrap_or("client")
            .eq_ignore_ascii_case("client")
        {
            anyhow::bail!("only client-created secret mode is supported");
        }
        if !query
            .value("response_mode")
            .unwrap_or("relay")
            .eq_ignore_ascii_case("relay")
        {
            anyhow::bail!("only relay response mode is supported");
        }

        let expires_at = query
            .value("expires_at")
            .map(|raw| {
                let expires_at = raw
                    .parse::<u64>()
                    .context("expires_at must be an unsigned timestamp")?;
                if expires_at <= now {
                    anyhow::bail!("NWA request has expired");
                }
                Ok(expires_at)
            })
            .transpose()?;

        let relays = query.values("relay");
        if relays.is_empty() {
            anyhow::bail!("at least one relay is required");
        }
        let relay = relays.join("\n");

        let budget_sat = query
            .value("max_amount")
            .map(|raw| {
                raw.parse::<u64>()
                    .context("max_amount must be an unsigned millisatoshi amount")
                    .map(|amount_msat| amount_msat / 1000)
            })
            .transpose()?
            .unwrap_or(10_000);
        let budget_interval = parse_budget_interval(query.value("budget_renewal"));
        let permissions = parse_permissions(query.value("request_methods"));

        let (return_to, callback_state) =
            parse_callback(query.value("return_to"), query.value("state"));
        let name = query
            .value("name")
            .or_else(|| query.value("appname"))
            .unwrap_or_default()
            .trim();
        let display_name = if name.is_empty() {
            "External App".to_string()
        } else {
            name.to_string()
        };
        let requesting_app_description = return_to
            .as_ref()
            .and_then(|callback| callback.host_str().map(str::to_string));
        let callback_target_description = return_to
            .as_ref()
            .map(callback_target_description)
            .unwrap_or_else(|| "none".to_string());

        Ok(Self {
            state: NwaRequestState {
                id: format!("{client_pubkey}-{now}"),
                client_pubkey,
                display_name,
                requesting_app_description,
                callback_target_description,
                relay,
                budget_sat,
                budget_interval,
                permissions,
                expires_at,
            },
            return_to,
            callback_state,
        })
    }

    pub(crate) fn approved_callback(
        &self,
        wallet_pubkey: &str,
        relays: &[String],
        lud16: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let mut pairs = self.state_pairs();
        pairs.push(("status", "approved".to_string()));
        pairs.push(("wallet_pubkey", wallet_pubkey.to_string()));
        pairs.extend(relays.iter().cloned().map(|relay| ("relay", relay)));
        if let Some(lud16) = lud16.filter(|value| !value.is_empty()) {
            pairs.push(("lud16", lud16.to_string()));
        }
        self.callback_url(&pairs)
    }

    pub(crate) fn cancelled_callback(&self) -> anyhow::Result<Option<String>> {
        let mut pairs = self.state_pairs();
        pairs.push(("status", "cancelled".to_string()));
        self.callback_url(&pairs)
    }

    fn state_pairs(&self) -> Vec<(&'static str, String)> {
        self.callback_state
            .as_ref()
            .map(|state| vec![("state", state.clone())])
            .unwrap_or_default()
    }

    fn callback_url(&self, pairs: &[(&str, String)]) -> anyhow::Result<Option<String>> {
        let Some(mut callback) = self.return_to.clone() else {
            return Ok(None);
        };
        let mut fragment_builder = Url::parse("https://callback.invalid/")?;
        {
            let mut query = fragment_builder.query_pairs_mut();
            for (name, value) in pairs {
                query.append_pair(name, value);
            }
        }
        callback.set_fragment(fragment_builder.query());
        Ok(Some(callback.to_string()))
    }
}

struct NwaQuery {
    values: HashMap<String, Vec<String>>,
}

impl NwaQuery {
    fn new(url: &Url) -> Self {
        let mut values = HashMap::<String, Vec<String>>::new();
        for (name, value) in url.query_pairs() {
            values
                .entry(name.into_owned())
                .or_default()
                .push(value.into_owned());
        }
        Self { values }
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|values| values.first())
            .map(String::as_str)
    }

    fn values(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }

    fn has_duplicate_single_value_parameters(&self, repeatable: &[&str]) -> bool {
        let repeatable = repeatable.iter().copied().collect::<HashSet<_>>();
        self.values
            .iter()
            .any(|(name, values)| !repeatable.contains(name.as_str()) && values.len() > 1)
    }
}

fn parse_callback(return_to: Option<&str>, state: Option<&str>) -> (Option<Url>, Option<String>) {
    let Some(return_to) = return_to.filter(|value| value.len() <= MAX_CALLBACK_LENGTH) else {
        return (None, None);
    };
    let state = match state {
        Some(value) => {
            let value = value.trim();
            if value.len() < MIN_STATE_LENGTH || value.len() > MAX_STATE_LENGTH {
                return (None, None);
            }
            Some(value)
        }
        None => None,
    };
    let Ok(callback) = Url::parse(return_to) else {
        return (None, None);
    };
    if !is_allowed_callback(&callback) {
        return (None, None);
    }
    (Some(callback), state.map(str::to_string))
}

fn is_allowed_callback(callback: &Url) -> bool {
    if !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
    {
        return false;
    }

    if callback.scheme() == "https" {
        let Some(host) = callback.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        return is_public_domain(&host)
            && callback.port().map_or(true, |port| port == 443)
            && !callback.path().is_empty();
    }

    let blocked = [
        "http",
        "file",
        "data",
        "javascript",
        "about",
        "blob",
        "nostr+walletauth",
        "nostr+walletauth+rebelwallet",
    ];
    !blocked.contains(&callback.scheme())
        && callback.port().is_none()
        && (callback.host_str().is_some() || !callback.path().is_empty())
}

fn is_public_domain(host: &str) -> bool {
    if !host.contains('.') || host.ends_with(".local") || host == "localhost" || host.contains(':')
    {
        return false;
    }
    let parts = host.split('.').collect::<Vec<_>>();
    let is_ipv4 = parts.len() == 4 && parts.iter().all(|part| part.parse::<u8>().is_ok());
    !is_ipv4
}

fn callback_target_description(callback: &Url) -> String {
    let host = callback.host_str().unwrap_or_default();
    let port = callback
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!(
        "{}://{}{}{}",
        callback.scheme(),
        host,
        port,
        callback.path()
    )
}

fn parse_budget_interval(value: Option<&str>) -> NwcBudgetInterval {
    match value.unwrap_or_default().to_ascii_lowercase().as_str() {
        "hourly" => NwcBudgetInterval::Hourly,
        "daily" => NwcBudgetInterval::Daily,
        "weekly" => NwcBudgetInterval::Weekly,
        "monthly" => NwcBudgetInterval::Monthly,
        "yearly" => NwcBudgetInterval::Yearly,
        _ => NwcBudgetInterval::Never,
    }
}

fn parse_permissions(value: Option<&str>) -> Vec<NwcPermission> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return NwcPermission::IMPLEMENTED.to_vec();
    };
    let mut permissions = value
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter_map(|method| match method.to_ascii_lowercase().as_str() {
            "get_info" => Some(NwcPermission::GetInfo),
            "get_balance" => Some(NwcPermission::GetBalance),
            "pay_invoice" => Some(NwcPermission::PayInvoice),
            "make_invoice" => Some(NwcPermission::MakeInvoice),
            "lookup_invoice" => Some(NwcPermission::LookupInvoice),
            "list_transactions" => Some(NwcPermission::ListTransactions),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !permissions.contains(&NwcPermission::GetInfo) {
        permissions.push(NwcPermission::GetInfo);
    }
    permissions
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: &str = "687dd8ece211539364549b1f32c63eceec1e0661009ba65cf8ff2e73ba000746";

    #[test]
    fn parses_client_created_nwa_request_and_converts_msats() {
        let request = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.getalby.com&max_amount=500000000&budget_renewal=monthly&request_methods=get_info+pay_invoice&name=Alby+Go"
            ),
            100,
        )
        .unwrap();

        assert_eq!(request.state.client_pubkey, CLIENT);
        assert_eq!(request.state.display_name, "Alby Go");
        assert_eq!(request.state.budget_sat, 500_000);
        assert_eq!(request.state.budget_interval, NwcBudgetInterval::Monthly);
        assert_eq!(
            request.state.permissions,
            vec![NwcPermission::GetInfo, NwcPermission::PayInvoice]
        );
    }

    #[test]
    fn rejects_duplicate_security_parameters() {
        let result = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&max_amount=1000&max_amount=2000"
            ),
            100,
        );
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn builds_fragment_callback_without_exposing_a_secret() {
        let callback_state = "0123456789abcdef0123456789abcdef";
        let request = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&return_to=zapritep2p-dev%3A%2F%2Fnwa%2Fcallback&state={callback_state}"
            ),
            100,
        )
        .unwrap();
        let callback = request
            .approved_callback(
                "102b3ceebfe25f0b58c526de93433b96866aa4c58ec75b3843a3dfd9d2255b50",
                &["wss://relay.example.com".to_string()],
                Some("name@example.com"),
            )
            .unwrap()
            .unwrap();

        assert!(callback.starts_with("zapritep2p-dev://nwa/callback#"));
        assert!(callback.contains("status=approved"));
        assert!(callback.contains("wallet_pubkey="));
        assert!(!callback.contains("secret="));
    }

    #[test]
    fn rejects_expired_requests() {
        let result = NwaRequest::parse(
            &format!(
                "nostr+walletauth://{CLIENT}?relay=wss%3A%2F%2Frelay.example.com&expires_at=100"
            ),
            100,
        );
        assert!(result.unwrap_err().to_string().contains("expired"));
    }
}
