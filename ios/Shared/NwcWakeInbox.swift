import Foundation
import NwcMobileApple
import Security

enum NwcWakeInboxEvents {
    static let didChange = Notification.Name("RebelWalletNwcWakeInboxDidChange")
}

extension NwcQueuedWakeRequest {
    init?(validatedUserInfo userInfo: [AnyHashable: Any]) {
        let receivedAt = UInt64(Date().timeIntervalSince1970)
        guard
            let data = try? JSONSerialization.data(withJSONObject: userInfo),
            let payloadJson = String(data: data, encoding: .utf8),
            let request = parseNwcWakePayloadJson(
                payloadJson: payloadJson,
                receivedAtSeconds: receivedAt
            )
        else {
            return nil
        }
        self.init(
            payload: NwcWakePayload(
                relayURL: request.relayUrl,
                eventIDHex: request.eventIdHex,
                walletServicePublicKeyHex: request.walletServicePublicKeyHex,
                embeddedEventJSON: request.embeddedEventJson
            ),
            receivedAtSeconds: request.receivedAtSeconds
        )
    }

    static let parseFailureMessage = "Ignored malformed or unrelated wake notification"
}

enum NwcWakeInbox {
    private static let queueKey = "nwcWakeQueue"
    private static let snapshotKey = "nwcWakeSnapshot"
    private static let legacyProcessedEventIdsKey = "nwcWakeProcessedEventIds"

    static func enqueue(_ request: NwcQueuedWakeRequest) {
        do {
            let inbox = try appGroupInbox()
            try migrateLegacyQueueIfNeeded(to: inbox)
            try inbox.enqueue(request)
        } catch {
            NSLog("Could not persist nwc_wake request: %@", String(describing: error))
            return
        }
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func pendingRequests() -> [NwcQueuedWakeRequest] {
        do {
            let inbox = try appGroupInbox()
            try migrateLegacyQueueIfNeeded(to: inbox)
            return try inbox.pendingRequests()
        } catch {
            NSLog("Could not read nwc_wake requests: %@", String(describing: error))
            return []
        }
    }

    static func remove(eventIds: Set<String>) {
        guard !eventIds.isEmpty else { return }
        do {
            let changed = try appGroupInbox().remove(eventIDs: eventIds)
            if changed {
                NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
            }
        } catch {
            NSLog("Could not acknowledge nwc_wake requests: %@", String(describing: error))
        }
    }

    static func appendDebug(source: String, message: String) {
#if DEBUG
        guard let log = debugLog() else {
            NSLog("Could not open app group defaults for nwc_wake debug log")
            return
        }
        do {
            try log.append(source: source, message: message)
        } catch {
            NSLog("Could not persist nwc_wake debug log: %@", String(describing: error))
            return
        }
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
#endif
    }

    static func debugEntries() -> [NwcWakeDebugEntry] {
#if DEBUG
        guard let log = debugLog() else {
            return []
        }
        do {
            return try log.entries()
        } catch {
            NSLog("Could not read nwc_wake debug log: %@", String(describing: error))
            return []
        }
#else
        return []
#endif
    }

    static func clearDebugEntries() {
#if DEBUG
        guard let log = debugLog() else {
            return
        }
        log.clear()
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
#endif
    }

    static func removeLegacySnapshot() {
        if let defaults = appGroupDefaults() {
            defaults.removeObject(forKey: snapshotKey)
            defaults.removeObject(forKey: legacyProcessedEventIdsKey)
        }
        NwcWakeKeychainStore.delete(key: snapshotKey)
    }

    static func extensionDataDirectoryPath() -> String? {
        try? appGroupInbox().dataDirectoryURL().path
    }

    private static func appGroupInbox() throws -> NwcAppGroupWakeInbox {
        guard
            let appGroupId = appGroupIdentifier(),
            let inbox = NwcAppGroupWakeInbox(appGroupIdentifier: appGroupId)
        else {
            throw CocoaError(.fileNoSuchFile)
        }
        return inbox
    }

    private static func migrateLegacyQueueIfNeeded(to inbox: NwcAppGroupWakeInbox) throws {
        if let defaults = appGroupDefaults() {
            try inbox.migrateLegacyFlatQueue(from: defaults, key: queueKey)
        }
    }

    private static func debugLog() -> NwcWakeDebugLog? {
        appGroupDefaults().map { NwcWakeDebugLog(defaults: $0) }
    }

    private static func appGroupDefaults() -> UserDefaults? {
        guard let appGroupId = appGroupIdentifier() else { return nil }
        return UserDefaults(suiteName: appGroupId)
    }

    private static func appGroupIdentifier() -> String? {
        guard
            let appGroupId = Bundle.main.object(
                forInfoDictionaryKey: "RebelWalletAppGroupIdentifier"
            ) as? String,
            !appGroupId.isEmpty
        else {
            return nil
        }
        return appGroupId
    }
}

private enum NwcWakeKeychainStore {
    private static let service = "com.rebelwallet.app.nwc-wake"

    static func delete(key: String) {
        SecItemDelete(baseQuery(key: key) as CFDictionary)
    }

    private static func baseQuery(key: String) -> [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key
        ]
        if let accessGroup = keychainAccessGroup {
            query[kSecAttrAccessGroup as String] = accessGroup
        }
        return query
    }

    private static var keychainAccessGroup: String? {
        guard let value = Bundle.main.object(forInfoDictionaryKey: "RebelWalletKeychainAccessGroup") as? String else {
            return nil
        }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !trimmed.hasPrefix("$(") else {
            return nil
        }
        return trimmed
    }
}
