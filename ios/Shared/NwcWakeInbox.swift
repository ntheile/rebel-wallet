import Foundation
import NwcMobileApple
import Security

enum NwcWakeInboxEvents {
    static let didChange = Notification.Name("RebelWalletNwcWakeInboxDidChange")
}

struct NwcWakeDebugEntry: Codable, Hashable, Identifiable {
    let id: String
    let timestamp: UInt64
    let source: String
    let message: String

    init(source: String, message: String, timestamp: UInt64 = UInt64(Date().timeIntervalSince1970)) {
        self.id = UUID().uuidString
        self.timestamp = timestamp
        self.source = source
        self.message = message
    }

    var timestampText: String {
        Date(timeIntervalSince1970: TimeInterval(timestamp)).formatted(date: .omitted, time: .standard)
    }
}

struct StoredNwcWakeRequest: Codable, Hashable {
    let relay: String
    let eventId: String
    let walletServicePubkey: String
    let eventJson: String?
    let receivedAt: UInt64

    init(
        relay: String,
        eventId: String,
        walletServicePubkey: String,
        eventJson: String? = nil,
        receivedAt: UInt64 = UInt64(Date().timeIntervalSince1970)
    ) {
        self.relay = relay
        self.eventId = eventId
        self.walletServicePubkey = walletServicePubkey
        self.eventJson = eventJson
        self.receivedAt = receivedAt
    }

    init?(userInfo: [AnyHashable: Any]) {
        guard
            let data = try? JSONSerialization.data(withJSONObject: userInfo),
            let payloadJson = String(data: data, encoding: .utf8),
            let request = parseNwcWakePayloadJson(
                payloadJson: payloadJson,
                receivedAtSeconds: UInt64(Date().timeIntervalSince1970)
            )
        else {
            return nil
        }
        self.init(
            relay: request.relayUrl,
            eventId: request.eventIdHex,
            walletServicePubkey: request.walletServicePublicKeyHex,
            eventJson: request.embeddedEventJson,
            receivedAt: request.receivedAtSeconds
        )
    }

    static func parseFailureMessage(userInfo _: [AnyHashable: Any]) -> String {
        "Ignored malformed or unrelated wake notification"
    }

    var normalizedUserInfo: [AnyHashable: Any] {
        var info: [AnyHashable: Any] = [
            "nwc_relay": relay,
            "nwc_event_id": eventId,
            "nwc_wallet_service_pubkey": walletServicePubkey,
        ]
        if let eventJson {
            info["nwc_event_json"] = eventJson
        }
        return info
    }

    init(_ queuedRequest: NwcQueuedWakeRequest) {
        relay = queuedRequest.payload.relayURL
        eventId = queuedRequest.payload.eventIDHex
        walletServicePubkey = queuedRequest.payload.walletServicePublicKeyHex
        eventJson = queuedRequest.payload.embeddedEventJSON
        receivedAt = queuedRequest.receivedAtSeconds
    }

    var queuedRequest: NwcQueuedWakeRequest {
        NwcQueuedWakeRequest(
            payload: NwcWakePayload(
                relayURL: relay,
                eventIDHex: eventId,
                walletServicePublicKeyHex: walletServicePubkey,
                embeddedEventJSON: eventJson
            ),
            receivedAtSeconds: receivedAt
        )
    }
}

enum NwcWakeInbox {
    private static let queueKey = "nwcWakeQueue"
    private static let queueDirectoryName = "NwcWakeInbox"
    private static let debugKey = "nwcWakeDebugLog"
    private static let snapshotKey = "nwcWakeSnapshot"
    private static let legacyProcessedEventIdsKey = "nwcWakeProcessedEventIds"
    private static let maxDebugEntries = 30

    static func enqueue(_ request: StoredNwcWakeRequest) {
        do {
            let inbox = try fileInbox()
            try migrateLegacyQueueIfNeeded(to: inbox)
            try inbox.enqueue(request.queuedRequest)
        } catch {
            NSLog("Could not persist nwc_wake request: %@", String(describing: error))
            return
        }
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func pendingRequests() -> [StoredNwcWakeRequest] {
        do {
            let inbox = try fileInbox()
            try migrateLegacyQueueIfNeeded(to: inbox)
            return try inbox.pendingRequests().map(StoredNwcWakeRequest.init)
        } catch {
            NSLog("Could not read nwc_wake requests: %@", String(describing: error))
            return []
        }
    }

    static func remove(eventIds: Set<String>) {
        guard !eventIds.isEmpty else { return }
        do {
            let changed = try fileInbox().remove(eventIDs: eventIds)
            if changed {
                NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
            }
        } catch {
            NSLog("Could not acknowledge nwc_wake requests: %@", String(describing: error))
        }
    }

    static func appendDebug(source: String, message: String) {
#if DEBUG
        guard let defaults = appGroupDefaults() else {
            NSLog("Could not open app group defaults for nwc_wake debug log")
            return
        }

        var entries = debugEntries(from: defaults)
        entries.append(NwcWakeDebugEntry(source: source, message: message))
        if entries.count > maxDebugEntries {
            entries.removeFirst(entries.count - maxDebugEntries)
        }
        saveDebugEntries(entries, to: defaults)
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
#endif
    }

    static func debugEntries() -> [NwcWakeDebugEntry] {
#if DEBUG
        guard let defaults = appGroupDefaults() else {
            return []
        }
        return debugEntries(from: defaults)
#else
        return []
#endif
    }

    static func clearDebugEntries() {
#if DEBUG
        guard let defaults = appGroupDefaults() else {
            return
        }
        defaults.removeObject(forKey: debugKey)
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
        guard
            let appGroupId = Bundle.main.object(
                forInfoDictionaryKey: "RebelWalletAppGroupIdentifier"
            ) as? String,
            !appGroupId.isEmpty,
            let root = FileManager.default.containerURL(
                forSecurityApplicationGroupIdentifier: appGroupId
            )
        else {
            return nil
        }
        let dataDirectory = root
            .appendingPathComponent("RustCore", isDirectory: true)
            .appendingPathComponent("ApplicationSupport", isDirectory: true)
        try? FileManager.default.createDirectory(
            at: dataDirectory,
            withIntermediateDirectories: true
        )
        return dataDirectory.path
    }

    private static func fileInbox() throws -> NwcWakeFileInbox {
        guard let root = appGroupRootURL() else {
            throw CocoaError(.fileNoSuchFile)
        }
        let directory = root.appendingPathComponent(queueDirectoryName, isDirectory: true)
        return NwcWakeFileInbox(directoryURL: directory)
    }

    private static func migrateLegacyQueueIfNeeded(to inbox: NwcWakeFileInbox) throws {
        guard
            let defaults = appGroupDefaults(),
            let legacyData = defaults.data(forKey: queueKey)
        else {
            return
        }
        let requests = try JSONDecoder().decode([StoredNwcWakeRequest].self, from: legacyData)
        for request in requests {
            try inbox.enqueue(request.queuedRequest)
        }
        defaults.removeObject(forKey: queueKey)
    }

    private static func debugEntries(from defaults: UserDefaults) -> [NwcWakeDebugEntry] {
        guard let data = defaults.data(forKey: debugKey) else {
            return []
        }

        return (try? JSONDecoder().decode([NwcWakeDebugEntry].self, from: data)) ?? []
    }

    private static func saveDebugEntries(_ entries: [NwcWakeDebugEntry], to defaults: UserDefaults) {
        guard let data = try? JSONEncoder().encode(entries) else {
            return
        }
        defaults.set(data, forKey: debugKey)
    }

    private static func appGroupDefaults() -> UserDefaults? {
        guard
            let appGroupId = Bundle.main.object(forInfoDictionaryKey: "RebelWalletAppGroupIdentifier") as? String,
            !appGroupId.isEmpty
        else {
            return nil
        }
        return UserDefaults(suiteName: appGroupId)
    }

    private static func appGroupRootURL() -> URL? {
        guard
            let appGroupId = Bundle.main.object(
                forInfoDictionaryKey: "RebelWalletAppGroupIdentifier"
            ) as? String,
            !appGroupId.isEmpty
        else {
            return nil
        }
        return FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: appGroupId
        )
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
