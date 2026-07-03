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

        if let wake = StoredNwcWakeRequest(userInfo: request.content.userInfo) {
            logWakePayload(wake)
            NwcWakeInbox.appendDebug(
                source: "NSE",
                message: "Parsed nwc_wake event_id=\(wake.eventId) relay=\(wake.relay)"
            )
            if respondToWakeIfPossible(wake) {
                content.title = content.title.isEmpty ? "Payment request handled" : content.title
                content.body = content.body.isEmpty ? "Rebel Wallet responded to a wallet request." : content.body
            } else {
                NwcWakeInbox.enqueue(wake)
                content.title = content.title.isEmpty ? "Payment request pending" : content.title
                content.body = content.body.isEmpty ? "Open Rebel Wallet to continue." : content.body
            }
            content.userInfo = wake.normalizedUserInfo
        } else {
            let message = StoredNwcWakeRequest.parseFailureMessage(userInfo: request.content.userInfo)
            NwcWakeInbox.appendDebug(source: "NSE", message: message)
            NSLog("RebelWallet NSE did not parse push: %@", message)
        }

        contentHandler(content)
    }

    override func serviceExtensionTimeWillExpire() {
        if let bestAttemptContent {
            contentHandler?(bestAttemptContent)
        }
    }

    private func logWakePayload(_ payload: StoredNwcWakeRequest) {
        NSLog(
            "RebelWallet NSE received nwc_wake event_id=%@ relay=%@ wallet_service_pubkey=%@",
            payload.eventId,
            payload.relay,
            payload.walletServicePubkey
        )
    }

    private func respondToWakeIfPossible(_ wake: StoredNwcWakeRequest) -> Bool {
        guard let snapshot = NwcWakeInbox.snapshot() else {
            NwcWakeInbox.appendDebug(source: "NSE", message: "No local NWC wake snapshot; queued for app")
            return false
        }

        let result: NwcExtensionWakeResult
        if let eventJson = wake.eventJson {
            result = processNwcEventFromSnapshot(
                snapshotJson: snapshot,
                relay: wake.relay,
                eventId: wake.eventId,
                walletServicePubkey: wake.walletServicePubkey,
                eventJson: eventJson
            )
        } else {
            result = processNwcWakeFromSnapshot(
                snapshotJson: snapshot,
                relay: wake.relay,
                eventId: wake.eventId,
                walletServicePubkey: wake.walletServicePubkey
            )
        }
        NwcWakeInbox.appendDebug(source: "NSE", message: result.message)
        if let updatedSnapshot = result.updatedSnapshotJson {
            NwcWakeInbox.saveSnapshot(updatedSnapshot)
        }
        NSLog("RebelWallet NSE wake response result: %@", result.message)
        return result.success
    }
}
