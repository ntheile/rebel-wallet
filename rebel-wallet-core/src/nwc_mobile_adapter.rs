use std::path::Path;
use std::sync::Arc;

use nostr_sdk::prelude::SecretKey;
use nwc_mobile::{
    ConnectionId, HostError, HostErrorKind, NwcSecretKey, SecretProvider, WakeLedger,
};
pub(crate) use nwc_mobile_nostr::NostrRelayTransport;
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
