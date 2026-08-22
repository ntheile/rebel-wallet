import Foundation
import NwcMobileApple
import UserNotifications

private let nwcExtensionExecutionMilliseconds: UInt64 = 25_000

final class NotificationService: UNNotificationServiceExtension {
    private var adapter: NwcNotificationServiceAdapter?

    override func didReceive(
        _ request: UNNotificationRequest,
        withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
    ) {
        NwcWakeInbox.removeLegacySnapshot()
        guard let wake = StoredNwcWakeRequest(userInfo: request.content.userInfo) else {
            NwcWakeInbox.appendDebug(
                source: "NSE",
                message: StoredNwcWakeRequest.parseFailureMessage(userInfo: request.content.userInfo)
            )
            contentHandler(request.content)
            return
        }
        guard let dataDirectory = NwcWakeInbox.extensionDataDirectoryPath() else {
            NwcWakeInbox.appendDebug(source: "NSE", message: "Shared storage is unavailable")
            contentHandler(Self.openApplicationContent(
                from: request.content,
                userInfo: wake.normalizedUserInfo
            ))
            return
        }

        let engine = NwcExtensionEngine(
            dataDir: dataDirectory,
            secretStore: KeychainSecretStore()
        )
        let executor = RebelNwcWakeExecutor(engine: engine)
        let adapter = NwcNotificationServiceAdapter(
            executor: executor,
            cancellationFactory: { RebelNwcWakeCancellation() },
            executionMilliseconds: nwcExtensionExecutionMilliseconds,
            copy: NwcNotificationCopy(
                processingTitle: "Nostr Wallet Connect",
                processingBody: "Processing request",
                completedTitle: "Nostr Wallet Connect",
                completedBody: "Request completed",
                openApplicationTitle: "Nostr Wallet Connect",
                openApplicationBody: "Open Rebel Wallet to continue"
            )
        )
        self.adapter = adapter

        let normalizedContent = (request.content.mutableCopy() as? UNMutableNotificationContent)
            ?? UNMutableNotificationContent()
        normalizedContent.userInfo = wake.normalizedUserInfo
        let normalizedRequest = UNNotificationRequest(
            identifier: request.identifier,
            content: normalizedContent,
            trigger: request.trigger
        )
        NwcWakeInbox.appendDebug(source: "NSE", message: "Started bounded NWC wake processing")
        adapter.didReceive(normalizedRequest, contentHandler: contentHandler)
    }

    override func serviceExtensionTimeWillExpire() {
        adapter?.timeWillExpire()
    }

    private static func openApplicationContent(
        from content: UNNotificationContent,
        userInfo: [AnyHashable: Any]
    ) -> UNNotificationContent {
        let mutable = (content.mutableCopy() as? UNMutableNotificationContent)
            ?? UNMutableNotificationContent()
        mutable.title = "Nostr Wallet Connect"
        mutable.subtitle = ""
        mutable.body = "Open Rebel Wallet to continue"
        mutable.attachments = []
        mutable.categoryIdentifier = ""
        mutable.threadIdentifier = "nwc"
        mutable.badge = nil
        mutable.sound = nil
        mutable.interruptionLevel = .active
        mutable.userInfo = userInfo
        return mutable
    }
}

private final class RebelNwcWakeCancellation: NwcWakeCancellation, @unchecked Sendable {
    let rust = NwcExtensionCancellation()

    func cancel() {
        rust.cancel()
    }
}

private final class RebelNwcWakeExecutor: NwcWakeExecutor, @unchecked Sendable {
    private let engine: NwcExtensionEngine

    init(engine: NwcExtensionEngine) {
        self.engine = engine
    }

    func execute(
        payload: NwcWakePayload,
        executionMilliseconds: UInt64,
        cancellation: any NwcWakeCancellation
    ) async -> NwcWakePresentationHint {
        guard let cancellation = cancellation as? RebelNwcWakeCancellation else {
            enqueue(payload)
            return .openApplication
        }

        let result = await engine.executeWake(
            request: NwcExtensionWakeRequest(
                relayUrl: payload.relayURL,
                eventIdHex: payload.eventIDHex,
                walletServicePublicKeyHex: payload.walletServicePublicKeyHex,
                embeddedEventJson: payload.embeddedEventJSON,
                receivedAtSeconds: UInt64(Date().timeIntervalSince1970)
            ),
            executionMilliseconds: executionMilliseconds,
            cancellation: cancellation.rust
        )

        switch result.disposition {
        case .completed, .alreadyProcessed, .rejected:
            break
        case .queuedForApplication, .retryAfter:
            enqueue(payload)
        }
        NwcWakeInbox.appendDebug(
            source: "NSE",
            message: "Finished bounded NWC wake processing: \(result.disposition)"
        )

        switch result.notification {
        case .processing:
            return .processing
        case .completed:
            return .completed
        case .openApplication:
            return .openApplication
        }
    }

    private func enqueue(_ payload: NwcWakePayload) {
        NwcWakeInbox.enqueue(StoredNwcWakeRequest(
            relay: payload.relayURL,
            eventId: payload.eventIDHex,
            walletServicePubkey: payload.walletServicePublicKeyHex,
            eventJson: payload.embeddedEventJSON
        ))
    }
}
