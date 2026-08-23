import Foundation
import NwcMobileApple
import UserNotifications

private let nwcExtensionExecutionMilliseconds: UInt64 = 25_000
private let nwcNotificationCopy = NwcNotificationCopy(
    processingTitle: "Nostr Wallet Connect",
    processingBody: "Processing request",
    completedTitle: "Nostr Wallet Connect",
    completedBody: "Request completed",
    openApplicationTitle: "Nostr Wallet Connect",
    openApplicationBody: "Open Rebel Wallet to continue"
)
private let nwcNotificationPresenter = NwcNotificationPresenter(copy: nwcNotificationCopy)

final class NotificationService: UNNotificationServiceExtension {
    private var adapter: NwcNotificationServiceAdapter?

    override func didReceive(
        _ request: UNNotificationRequest,
        withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
    ) {
        NwcWakeInbox.removeLegacySnapshot()
        guard let wake = NwcQueuedWakeRequest(validatedUserInfo: request.content.userInfo) else {
            NwcWakeInbox.appendDebug(
                source: "NSE",
                message: NwcQueuedWakeRequest.parseFailureMessage
            )
            contentHandler(request.content)
            return
        }
        guard let dataDirectory = NwcWakeInbox.extensionDataDirectoryPath() else {
            NwcWakeInbox.appendDebug(source: "NSE", message: "Shared storage is unavailable")
            contentHandler(nwcNotificationPresenter.content(
                applying: .openApplication,
                to: request.content,
                userInfo: wake.payload.normalizedUserInfo
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
            copy: nwcNotificationCopy
        )
        self.adapter = adapter

        NwcWakeInbox.appendDebug(source: "NSE", message: "Started bounded NWC wake processing")
        adapter.didReceive(
            payload: wake.payload,
            content: request.content,
            contentHandler: contentHandler
        )
    }

    override func serviceExtensionTimeWillExpire() {
        adapter?.timeWillExpire()
    }

}

private final class RebelNwcWakeCancellation: NwcWakeCancellation, @unchecked Sendable {
    let rust = MobileCancellation()

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

        let execution = await engine.executeWake(
            request: MobileWakeEnvelope(
                relayUrl: payload.relayURL,
                eventIdHex: payload.eventIDHex,
                walletServicePublicKeyHex: payload.walletServicePublicKeyHex,
                embeddedEventJson: payload.embeddedEventJSON,
                receivedAtSeconds: UInt64(Date().timeIntervalSince1970)
            ),
            executionMilliseconds: executionMilliseconds,
            cancellation: cancellation.rust
        )
        let result = execution.disposition
        if !execution.diagnosticCodes.isEmpty {
            NwcWakeInbox.appendDebug(
                source: "NSE",
                message: "NWC diagnostics: \(execution.diagnosticCodes.joined(separator: ", "))"
            )
        }

        let notification: MobileNotificationHint
        switch result {
        case .completed(let hint), .alreadyProcessed(let hint), .rejected(_, let hint):
            notification = hint
        case .queuedForApplication(_, let hint), .retryAfter(_, _, let hint):
            notification = hint
            enqueue(payload)
        }
        NwcWakeInbox.appendDebug(
            source: "NSE",
            message: "Finished bounded NWC wake processing: \(result)"
        )

        switch notification {
        case .processing:
            return .processing
        case .completed:
            return .completed
        case .openApplication:
            return .openApplication
        }
    }

    private func enqueue(_ payload: NwcWakePayload) {
        NwcWakeInbox.enqueue(NwcQueuedWakeRequest(
            payload: payload,
            receivedAtSeconds: UInt64(Date().timeIntervalSince1970)
        ))
    }
}
