import UserNotifications

final class NotificationService: UNNotificationServiceExtension {
    private var contentHandler: ((UNNotificationContent) -> Void)?
    private var bestAttemptContent: UNMutableNotificationContent?

    override func didReceive(
        _ request: UNNotificationRequest,
        withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
    ) {
        self.contentHandler = contentHandler
        let content = (request.content.mutableCopy() as? UNMutableNotificationContent)
            ?? UNMutableNotificationContent()
        bestAttemptContent = content

        if let wake = NwcWakePayload(userInfo: request.content.userInfo) {
            logWakePayload(wake)
            content.title = content.title.isEmpty ? "Payment request pending" : content.title
            content.body = content.body.isEmpty ? "Open Rebel Wallet to continue." : content.body
            content.userInfo = wake.normalizedUserInfo
        }

        contentHandler(content)
    }

    override func serviceExtensionTimeWillExpire() {
        if let bestAttemptContent {
            contentHandler?(bestAttemptContent)
        }
    }

    private func logWakePayload(_ payload: NwcWakePayload) {
        NSLog(
            "RebelWallet NSE received nwc_wake event_id=%@ relay=%@ wallet_service_pubkey=%@",
            payload.eventId,
            payload.relay,
            payload.walletServicePubkey
        )
    }
}

private struct NwcWakePayload {
    let relay: String
    let eventId: String
    let walletServicePubkey: String

    init?(userInfo: [AnyHashable: Any]) {
        guard
            (userInfo["protocol"] as? String) == "nwc_wake",
            let relay = userInfo["relay"] as? String,
            let eventId = userInfo["event_id"] as? String,
            let walletServicePubkey = userInfo["wallet_service_pubkey"] as? String
        else {
            return nil
        }

        self.relay = relay
        self.eventId = eventId
        self.walletServicePubkey = walletServicePubkey
    }

    var normalizedUserInfo: [AnyHashable: Any] {
        [
            "protocol": "nwc_wake",
            "version": "v1",
            "relay": relay,
            "event_id": eventId,
            "wallet_service_pubkey": walletServicePubkey
        ]
    }
}
