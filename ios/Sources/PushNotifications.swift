import UIKit
import UserNotifications
import NwcMobileApple
import os.log

enum PushNotificationEvents {
    static let registrationDidChange = Notification.Name("RebelWalletPushRegistrationDidChange")
    static let deviceTokenKey = "deviceToken"
    static let statusKey = "status"
}

// Leave two seconds below iOS's approximate background-push execution window
// so Rust can publish the NIP-47 response and acknowledge the wake server.
private let nwcBackgroundWakeExecutionMilliseconds: UInt64 = 28_000

enum NwcPushPlatformContext {
    private static let deviceTokenKey = "RebelWalletApnsDeviceToken"

    static var serverURL: String? {
        guard let value = Bundle.main.object(forInfoDictionaryKey: "RebelWalletNwcWakeServerURL") as? String else {
            return nil
        }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty || trimmed.hasPrefix("$(") ? nil : trimmed
    }

    static var apnsEnvironment: String {
        let value = Bundle.main.object(forInfoDictionaryKey: "RebelWalletApnsEnvironment") as? String
        return value == "production" ? "production" : "sandbox"
    }

    static var installId: String {
        let key = "RebelWalletNwcWakeInstallId"
        if let value = UserDefaults.standard.string(forKey: key), !value.isEmpty {
            return value
        }
        let value = UUID().uuidString
        UserDefaults.standard.set(value, forKey: key)
        return value
    }

    static var cachedDeviceToken: String? {
        get {
            UserDefaults.standard.string(forKey: deviceTokenKey)
        }
        set {
            UserDefaults.standard.set(newValue, forKey: deviceTokenKey)
        }
    }
}

final class RebelWalletAppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions _: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        requestPushAuthorization(application)
        return true
    }

    func application(
        _: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        let token = deviceToken.map { String(format: "%02x", $0) }.joined()
        NwcPushPlatformContext.cachedDeviceToken = token
        postRegistrationStatus("Registered", deviceToken: token)
        os_log("RebelWallet APNs device token: %{private}@", log: .default, type: .debug, token)
    }

    func application(
        _: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        postRegistrationStatus("Registration failed", deviceToken: nil)
        NSLog("RebelWallet failed to register for remote notifications: %@", String(describing: error))
    }

    func application(
        _: UIApplication,
        didReceiveRemoteNotification userInfo: [AnyHashable: Any],
        fetchCompletionHandler completionHandler: @escaping (UIBackgroundFetchResult) -> Void
    ) {
        guard userInfo["settlement_check"] as? Bool == true else {
            completionHandler(.noData)
            return
        }

        Task {
            completionHandler(await processSettlementWake(userInfo: userInfo))
        }
    }

    func userNotificationCenter(
        _: UNUserNotificationCenter,
        willPresent _: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }

    func userNotificationCenter(
        _: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        if let wake = NwcQueuedWakeRequest(
            validatedUserInfo: response.notification.request.content.userInfo
        ) {
            NwcWakeInbox.appendDebug(
                source: "App",
                message: "Notification tapped; queued NWC wake request"
            )
            NwcWakeInbox.enqueue(wake)
            os_log("RebelWallet opened NWC wake notification", log: .default, type: .debug)
        } else {
            NwcWakeInbox.appendDebug(
                source: "App",
                message: NwcQueuedWakeRequest.parseFailureMessage
            )
        }
    }

    private func requestPushAuthorization(_ application: UIApplication) {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .badge, .sound]) { granted, error in
            if let error {
                NSLog("RebelWallet notification authorization error: %@", String(describing: error))
                return
            }

            guard granted else {
                self.postRegistrationStatus("Permission denied", deviceToken: nil)
                NSLog("RebelWallet notification authorization denied")
                return
            }

            self.postRegistrationStatus("Permission granted", deviceToken: nil)
            DispatchQueue.main.async {
                application.registerForRemoteNotifications()
            }
        }
    }

    private func processSettlementWake(
        userInfo: [AnyHashable: Any]
    ) async -> UIBackgroundFetchResult {
        NwcWakeInbox.removeLegacySnapshot()
        guard let wake = NwcQueuedWakeRequest(validatedUserInfo: userInfo) else {
            NwcWakeInbox.appendDebug(
                source: "App background",
                message: NwcQueuedWakeRequest.parseFailureMessage
            )
            return .noData
        }
        guard let dataDirectory = NwcWakeInbox.extensionDataDirectoryPath() else {
            NwcWakeInbox.appendDebug(
                source: "App background",
                message: "Shared storage is unavailable"
            )
            NwcWakeInbox.enqueue(wake)
            return .failed
        }

        let settlementMonitor = NwcWakeInbox.settlementMonitorConfiguration()
        let engine = NwcExtensionEngine(
            dataDir: dataDirectory,
            secretStore: KeychainSecretStore(),
            wakeServerUrl: settlementMonitor?.serverURL,
            installId: settlementMonitor?.installID ?? ""
        )
        NwcWakeInbox.appendDebug(
            source: "App background",
            message: "Started silent NWC settlement processing"
        )
        let execution = await engine.executeWake(
            request: MobileWakeEnvelope(
                relayUrl: wake.payload.relayURL,
                eventIdHex: wake.payload.eventIDHex,
                walletServicePublicKeyHex: wake.payload.walletServicePublicKeyHex,
                embeddedEventJson: wake.payload.embeddedEventJSON,
                receivedAtSeconds: wake.receivedAtSeconds
            ),
            executionMilliseconds: nwcBackgroundWakeExecutionMilliseconds,
            cancellation: MobileCancellation()
        )

        if !execution.diagnosticCodes.isEmpty {
            NwcWakeInbox.appendDebug(
                source: "App background",
                message: "NWC diagnostics: \(execution.diagnosticCodes.joined(separator: ", "))"
            )
        }

        switch execution.disposition {
        case .completed, .alreadyProcessed, .rejected:
            NwcWakeInbox.appendDebug(
                source: "App background",
                message: "Finished silent NWC settlement processing"
            )
            return .newData
        case .queuedForApplication, .retryAfter:
            NwcWakeInbox.enqueue(wake)
            NwcWakeInbox.appendDebug(
                source: "App background",
                message: "Queued silent NWC settlement processing for retry"
            )
            return .failed
        }
    }

    private func postRegistrationStatus(_ status: String, deviceToken: String?) {
        var userInfo: [String: Any] = [
            PushNotificationEvents.statusKey: status,
        ]
        if let deviceToken {
            userInfo[PushNotificationEvents.deviceTokenKey] = deviceToken
        }
        NotificationCenter.default.post(
            name: PushNotificationEvents.registrationDidChange,
            object: nil,
            userInfo: userInfo
        )
    }
}
