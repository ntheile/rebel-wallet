import Foundation
import Observation
import Security
import UIKit

@MainActor
@Observable
final class AppManager: AppReconciler {
    let rust: FfiApp
    var state: AppState
    var nwcWakeDebugEntries: [NwcWakeDebugEntry]
    private var lastRevApplied: UInt64
    private var lastNwcWakeStatusLogged: String
    private var lastNwcWakeSnapshot: String?
    private var receiveBackgroundTask: UIBackgroundTaskIdentifier = .invalid
    private var notificationObservers: [NSObjectProtocol] = []
    private let nwcWakeRegistration = NwcWakeRegistrationService()

    init() {
        let fm = FileManager.default
        let legacyDataDirUrl = fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        let legacyCacheDirUrl = fm.urls(for: .cachesDirectory, in: .userDomainMask).first!.appendingPathComponent("RebelWallet")
        let appGroupId = Bundle.main.object(forInfoDictionaryKey: "RebelWalletAppGroupIdentifier") as? String
        let appGroupRootUrl = appGroupId.flatMap { fm.containerURL(forSecurityApplicationGroupIdentifier: $0) }
        let sharedRootUrl = appGroupRootUrl?.appendingPathComponent("RustCore", isDirectory: true)
        let dataDirUrl = sharedRootUrl?.appendingPathComponent("ApplicationSupport", isDirectory: true) ?? legacyDataDirUrl
        let cacheDirUrl = sharedRootUrl?.appendingPathComponent("Caches", isDirectory: true) ?? legacyCacheDirUrl
        let dataDir = dataDirUrl.path
        let cacheDir = cacheDirUrl.path
        try? fm.createDirectory(at: dataDirUrl, withIntermediateDirectories: true)
        try? fm.createDirectory(at: cacheDirUrl, withIntermediateDirectories: true)
        if dataDirUrl != legacyDataDirUrl {
            Self.migrateLegacyData(from: legacyDataDirUrl, to: dataDirUrl)
        }
        Self.removeLegacyProfileCache(from: dataDirUrl)

        let rust = FfiApp(dataDir: dataDir, cacheDir: cacheDir, secretStore: KeychainSecretStore())
        self.rust = rust

        let initial = rust.state()
        self.state = initial
        self.nwcWakeDebugEntries = NwcWakeInbox.debugEntries()
        self.lastRevApplied = initial.rev
        self.lastNwcWakeStatusLogged = initial.nwc.lastWakeStatus
        self.lastNwcWakeSnapshot = nil

        rust.listenForUpdates(reconciler: self)
        observePushNotificationRegistration()
        observeNwcWakeInbox()
        rust.dispatch(action: .bootstrap)
        syncNwcWakeSnapshot()
        drainQueuedNwcWakeRequests()
    }

    nonisolated func reconcile(update: AppUpdate) {
        Task { @MainActor [weak self] in
            self?.apply(update: update)
        }
    }

    private func apply(update: AppUpdate) {
        switch update {
        case .fullState(let s):
            if s.rev <= lastRevApplied { return }
            recordNwcWakeDebugChanges(nextState: s)
            lastRevApplied = s.rev
            state = s
            nwcWakeRegistration.sync(state: s)
            syncNwcWakeSnapshot()
            // If a Lightning receive completed (e.g. while backgrounded), release the
            // background-execution assertion now that the core no longer needs to run.
            if !isAwaitingLightningReceive {
                endReceiveBackgroundTask()
            }
        case .haptic(let feedback):
            Haptics.play(feedback)
        }
    }

    private func recordNwcWakeDebugChanges(nextState: AppState) {
        if state.setup != .ready, nextState.setup == .ready, !nextState.nwc.pendingWakeRequests.isEmpty {
            NwcWakeInbox.appendDebug(
                source: "Rust",
                message: "Wallet ready; retrying \(nextState.nwc.pendingWakeRequests.count) pending NWC wake request\(nextState.nwc.pendingWakeRequests.count == 1 ? "" : "s")"
            )
        }

        let status = nextState.nwc.lastWakeStatus
        if status != lastNwcWakeStatusLogged {
            lastNwcWakeStatusLogged = status
            NwcWakeInbox.appendDebug(source: "Rust", message: status)
        }

        refreshNwcWakeDebugEntries()
    }

    /// True while we are showing a receive request and still waiting for the
    /// Lightning payment to be claimed. The Rust core must keep polling to supply
    /// the preimage, so the process needs to stay alive if backgrounded.
    var isAwaitingLightningReceive: Bool {
        state.receive.phase == .showingRequest
            && state.receive.lightningInvoice != nil
            && !state.receive.lightningPaid
    }

    /// Request a background-execution assertion so the Rust core keeps polling and
    /// can claim an in-flight Lightning receive after the app is backgrounded.
    /// iOS grants a limited window (~30s); we release the assertion as soon as the
    /// payment is claimed, the app returns to the foreground, or the window expires.
    func beginReceiveBackgroundTaskIfNeeded() {
        guard isAwaitingLightningReceive else { return }
        guard receiveBackgroundTask == .invalid else { return }
        receiveBackgroundTask = UIApplication.shared.beginBackgroundTask(withName: "LightningReceive") { [weak self] in
            self?.endReceiveBackgroundTask()
        }
    }

    func endReceiveBackgroundTask() {
        guard receiveBackgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(receiveBackgroundTask)
        receiveBackgroundTask = .invalid
    }

    func dispatch(_ action: AppAction) {
        rust.dispatch(action: action)
    }

    func requestHaptic(_ feedback: HapticFeedback) {
        dispatch(.requestHaptic(feedback: feedback))
    }

    private func observePushNotificationRegistration() {
        let observer = NotificationCenter.default.addObserver(
            forName: PushNotificationEvents.registrationDidChange,
            object: nil,
            queue: .main
        ) { [weak self] notification in
            let status = notification.userInfo?[PushNotificationEvents.statusKey] as? String
            let deviceToken = notification.userInfo?[PushNotificationEvents.deviceTokenKey] as? String

            Task { @MainActor [weak self] in
                self?.dispatch(.setPushNotificationRegistration(
                    apnsDeviceToken: deviceToken,
                    registrationStatus: status ?? "Unknown"
                ))
            }
        }
        notificationObservers.append(observer)
    }

    private func observeNwcWakeInbox() {
        let observer = NotificationCenter.default.addObserver(
            forName: NwcWakeInboxEvents.didChange,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.refreshNwcWakeDebugEntries()
                self?.drainQueuedNwcWakeRequests()
            }
        }
        notificationObservers.append(observer)
    }

    func drainQueuedNwcWakeRequests() {
        let requests = NwcWakeInbox.drain()
        guard !requests.isEmpty else { return }

        NwcWakeInbox.appendDebug(
            source: "App",
            message: "Drained \(requests.count) queued nwc_wake request\(requests.count == 1 ? "" : "s")"
        )
        refreshNwcWakeDebugEntries()
        dispatch(.processNwcWakeRequests(requests: requests.map {
            NwcWakeRequest(
                relay: $0.relay,
                eventId: $0.eventId,
                walletServicePubkey: $0.walletServicePubkey,
                receivedAt: $0.receivedAt
            )
        }))
    }

    func refreshNwcWakeDebugEntries() {
        nwcWakeDebugEntries = Array(NwcWakeInbox.debugEntries().reversed())
    }

    func clearNwcWakeDebugEntries() {
        NwcWakeInbox.clearDebugEntries()
        refreshNwcWakeDebugEntries()
    }

    private func syncNwcWakeSnapshot() {
        let snapshot = rust.nwcWakeSnapshotJson()
        guard snapshot != lastNwcWakeSnapshot else { return }
        lastNwcWakeSnapshot = snapshot
        NwcWakeInbox.saveSnapshot(snapshot)
    }

    private static func removeLegacyProfileCache(from dataDirUrl: URL) {
        let fm = FileManager.default
        for fileName in ["profiles.sqlite3", "profiles.sqlite3-wal", "profiles.sqlite3-shm"] {
            try? fm.removeItem(at: dataDirUrl.appendingPathComponent(fileName))
        }
        try? fm.removeItem(at: dataDirUrl.appendingPathComponent("profile_pictures"))
    }

    private static func migrateLegacyData(from legacyDataDirUrl: URL, to dataDirUrl: URL) {
        let fm = FileManager.default
        guard let items = try? fm.contentsOfDirectory(
            at: legacyDataDirUrl,
            includingPropertiesForKeys: [.isRegularFileKey],
            options: [.skipsHiddenFiles]
        ) else {
            return
        }

        for sourceUrl in items {
            let destinationUrl = dataDirUrl.appendingPathComponent(sourceUrl.lastPathComponent)
            guard !fm.fileExists(atPath: destinationUrl.path) else { continue }
            try? fm.copyItem(at: sourceUrl, to: destinationUrl)
        }
    }

    func syncWalletForRefresh() async {
        if state.busy.syncingWallet {
            await waitForWalletSync()
            return
        }

        let startingRev = state.rev
        dispatch(.syncWallet)
        await waitForWalletSync(startingRev: startingRev)
    }

    private func waitForWalletSync(startingRev: UInt64? = nil) async {
        let timeout = Date().addingTimeInterval(60)
        var observedSync = state.busy.syncingWallet

        while Date() < timeout {
            if state.busy.syncingWallet {
                observedSync = true
            } else if observedSync {
                return
            } else if let startingRev, state.rev > startingRev {
                return
            }

            try? await Task.sleep(nanoseconds: 100_000_000)
        }
    }
}

final class KeychainSecretStore: SecretStore {
    private let service = "com.rebelwallet.app"

    func getSecret(key: String) -> String? {
        var query = baseQuery(key: key)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    func setSecret(key: String, value: String) -> Bool {
        let data = Data(value.utf8)
        var query = baseQuery(key: key)
        let update: [String: Any] = [kSecValueData as String: data]

        let status = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if status == errSecSuccess {
            return true
        }

        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        return SecItemAdd(query as CFDictionary, nil) == errSecSuccess
    }

    func deleteSecret(key: String) -> Bool {
        SecItemDelete(baseQuery(key: key) as CFDictionary) == errSecSuccess
    }

    private func baseQuery(key: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key
        ]
    }
}
