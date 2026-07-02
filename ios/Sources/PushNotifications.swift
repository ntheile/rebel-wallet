import UIKit
import UserNotifications

enum PushNotificationEvents {
    static let registrationDidChange = Notification.Name("RebelWalletPushRegistrationDidChange")
    static let deviceTokenKey = "deviceToken"
    static let statusKey = "status"
}

final class RebelWalletAppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        requestPushAuthorization(application)
        return true
    }

    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        let token = deviceToken.map { String(format: "%02x", $0) }.joined()
        postRegistrationStatus("Registered", deviceToken: token)
        NSLog("RebelWallet APNs device token: %@", token)
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        postRegistrationStatus("Registration failed", deviceToken: nil)
        NSLog("RebelWallet failed to register for remote notifications: %@", String(describing: error))
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        if let wake = NwcWakeNotification(userInfo: response.notification.request.content.userInfo) {
            NSLog("RebelWallet opened nwc_wake notification event_id=%@ relay=%@", wake.eventId, wake.relay)
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
            PushNotificationEvents.statusKey: status
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

private struct NwcWakeNotification {
    let relay: String
    let eventId: String

    init?(userInfo: [AnyHashable: Any]) {
        guard
            (userInfo["protocol"] as? String) == "nwc_wake",
            let relay = userInfo["relay"] as? String,
            let eventId = userInfo["event_id"] as? String
        else {
            return nil
        }

        self.relay = relay
        self.eventId = eventId
    }
}
