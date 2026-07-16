import Foundation
import os.log
import UserNotifications

private let nseLog = OSLog(
    subsystem: Bundle.main.bundleIdentifier ?? "NWC.NotificationService",
    category: "NWCWake"
)

final class NotificationService: UNNotificationServiceExtension {
    private var contentHandler: ((UNNotificationContent) -> Void)?
    private var bestAttemptContent: UNMutableNotificationContent?
    private var currentWake: StoredNwcWakeRequest?

    override func didReceive(
        _ request: UNNotificationRequest,
        withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
    ) {
        self.contentHandler = contentHandler
        let content = (request.content.mutableCopy() as? UNMutableNotificationContent)
            ?? UNMutableNotificationContent()
        bestAttemptContent = content

        if let wake = StoredNwcWakeRequest(userInfo: request.content.userInfo) {
            currentWake = wake
            let startedAt = Date()
            logWakePayload(wake)
            NwcWakeInbox.appendDebug(
                source: "NSE",
                message: "NSE started event_id=\(wake.eventId) relay=\(wake.relay)"
            )
            if NwcWakeInbox.isProcessed(eventId: wake.eventId) {
                currentWake = nil
                NwcWakeInbox.appendDebug(
                    source: "NSE",
                    message: "Skipped already processed event_id=\(wake.eventId)"
                )
                setGenericWakeNotification(content, body: "Processing Request")
            } else {
                let outcome = respondToWakeIfPossible(wake, startedAt: startedAt)
                if outcome.handled {
                    currentWake = nil
                    setGenericWakeNotification(content, body: outcome.notificationBody)
                } else {
                    NwcWakeInbox.enqueue(wake)
                    currentWake = nil
                    setGenericWakeNotification(content, body: outcome.notificationBody)
                }
            }
            content.userInfo = wake.normalizedUserInfo
        } else {
            let message = StoredNwcWakeRequest.parseFailureMessage(userInfo: request.content.userInfo)
            NwcWakeInbox.appendDebug(source: "NSE", message: message)
            os_log(
                "NWC wake push did not parse: %{private}@",
                log: nseLog,
                type: .info,
                message
            )
        }

        contentHandler(content)
    }

    override func serviceExtensionTimeWillExpire() {
        if let currentWake {
            if !NwcWakeInbox.isProcessed(eventId: currentWake.eventId) {
                NwcWakeInbox.enqueue(currentWake)
                NwcWakeInbox.appendDebug(
                    source: "NSE",
                    message: "NSE time expired while processing \(currentWake.eventId); queued for app"
                )
            }
        }
        if let bestAttemptContent {
            setGenericWakeNotification(bestAttemptContent)
            contentHandler?(bestAttemptContent)
        }
    }

    private func logWakePayload(_ payload: StoredNwcWakeRequest) {
        os_log(
            "NWC wake received event_id=%{private}@ relay=%{private}@ wallet_service_pubkey=%{private}@",
            log: nseLog,
            type: .info,
            payload.eventId,
            payload.relay,
            payload.walletServicePubkey
        )
    }

    private struct WakeProcessingOutcome {
        let handled: Bool
        let notificationBody: String
    }

    private func respondToWakeIfPossible(
        _ wake: StoredNwcWakeRequest,
        startedAt: Date
    ) -> WakeProcessingOutcome {
        guard let snapshot = NwcWakeInbox.snapshot() else {
            NwcWakeInbox.appendDebug(source: "NSE", message: "No local NWC wake snapshot; queued for app")
            return WakeProcessingOutcome(handled: false, notificationBody: "Processing Request")
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
        let durationMs = Int(Date().timeIntervalSince(startedAt) * 1000)
        NwcWakeInbox.appendDebug(
            source: "NSE",
            message: "\(result.message) total_duration_ms=\(durationMs)"
        )
        if result.success {
            let processedEventIds = result.processedEventIds.isEmpty
                ? [wake.eventId]
                : result.processedEventIds
            NwcWakeInbox.markProcessed(eventIds: processedEventIds)
            if let updatedSnapshot = result.updatedSnapshotJson {
                NwcWakeInbox.saveSnapshot(updatedSnapshot)
            }
        }
        os_log(
            "NWC wake response result: %{private}@",
            log: nseLog,
            type: .info,
            result.message
        )
        return WakeProcessingOutcome(handled: result.success, notificationBody: result.notificationBody)
    }

    private func setGenericWakeNotification(
        _ content: UNMutableNotificationContent,
        body: String = "Processing Request"
    ) {
        content.title = "Nostr Connect"
        content.body = body
        content.sound = nil
        content.badge = nil
        content.interruptionLevel = .passive
    }
}
