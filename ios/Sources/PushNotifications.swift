import UIKit
import UserNotifications

enum PushNotificationEvents {
    static let registrationDidChange = Notification.Name("RebelWalletPushRegistrationDidChange")
    static let deviceTokenKey = "deviceToken"
    static let statusKey = "status"
}

enum NwcPushPlatformContext {
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
        postRegistrationStatus("Registered", deviceToken: token)
        NSLog("RebelWallet APNs device token: %@", token)
    }

    func application(
        _: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        postRegistrationStatus("Registration failed", deviceToken: nil)
        NSLog("RebelWallet failed to register for remote notifications: %@", String(describing: error))
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
        if let wake = StoredNwcWakeRequest(userInfo: response.notification.request.content.userInfo) {
            NwcWakeInbox.appendDebug(
                source: "App",
                message: "Notification tapped event_id=\(wake.eventId) relay=\(wake.relay)"
            )
            NwcWakeInbox.enqueue(wake)
            NSLog("RebelWallet opened nwc_wake notification event_id=%@ relay=%@", wake.eventId, wake.relay)
        } else {
            NwcWakeInbox.appendDebug(
                source: "App",
                message: StoredNwcWakeRequest.parseFailureMessage(userInfo: response.notification.request.content.userInfo)
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
