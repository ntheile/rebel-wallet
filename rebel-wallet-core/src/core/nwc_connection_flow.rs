use super::*;

impl AppCore {
    pub(super) fn request_nwc_connection_export(&mut self, id: String, copy_to_clipboard: bool) {
        let Some(connection) = self
            .state
            .nwc
            .connections
            .iter()
            .find(|connection| connection.id == id)
        else {
            self.state.toast = Some("NWC connection was not found.".to_string());
            return;
        };
        let provider = RebelSecretProvider::new(self.secrets.clone());
        let uri = match self
            .nwc_service
            .as_ref()
            .expect("connection state requires the shared service")
            .export_wallet_connection_uri(
                &connection.id,
                self.state.lightning_address.address.clone(),
                &provider,
            ) {
            Ok(uri) => uri,
            Err(_) => {
                self.state.toast = Some(
                    "This client-created NWC connection cannot be exported by the wallet."
                        .to_string(),
                );
                self.request_haptic(HapticFeedback::NotificationWarning);
                return;
            }
        };
        self.pending_side_effects
            .push(AppUpdate::NwcConnectionExportReady {
                rev: self.rev + 1,
                connection_id: connection.id.clone(),
                name: connection.name.clone(),
                uri,
                copy_to_clipboard,
                present_qr: !copy_to_clipboard,
            });
    }
}
