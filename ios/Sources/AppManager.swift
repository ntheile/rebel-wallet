import Foundation
import Observation
import UIKit
import UserNotifications

@MainActor
@Observable
final class AppManager: AppReconciler {
    let rust: FfiApp
    var state: AppState
    var nwcWakeDebugEntries: [NwcWakeDebugEntry]
    var nwcConnectionExport: NwcConnectionExport?
    private var lastRevApplied: UInt64
    private var lastNwcWakeStatusLogged: String
    private var lastReceiveNotificationKey: String?
    private var receiveBackgroundTask: UIBackgroundTaskIdentifier = .invalid
    private var notificationObservers: [NSObjectProtocol] = []

    init(storagePaths: AppStoragePaths) {
        let dataDir = storagePaths.dataDir
        let cacheDir = storagePaths.cacheDir
        let rust = FfiApp(dataDir: dataDir, cacheDir: cacheDir, secretStore: KeychainSecretStore())
        self.rust = rust

        let initial = rust.state()
        state = initial
        nwcWakeDebugEntries = NwcWakeInbox.debugEntries()
        lastRevApplied = initial.rev
        lastNwcWakeStatusLogged = initial.nwc.lastWakeStatus
        lastReceiveNotificationKey = Self.receiveNotificationKey(initial.receive)

        rust.listenForUpdates(reconciler: self)
        observePushNotificationRegistration()
        observeNwcWakeInbox()
        rust.dispatch(action: .bootstrap)
        if let deviceToken = NwcPushPlatformContext.cachedDeviceToken {
            syncPushNotificationRegistration(status: "Registered", deviceToken: deviceToken)
        }
        NwcWakeInbox.removeLegacySnapshot()
        drainQueuedNwcWakeRequests()
    }

    convenience init() {
        self.init(storagePaths: AppStoragePreparer.prepareSynchronously())
    }

    nonisolated static func prepareStorage() async -> AppStoragePaths {
        await AppStoragePreparer.prepare()
    }

    nonisolated static func prepareStorageSynchronously() -> AppStoragePaths {
        AppStoragePreparer.prepareSynchronously()
    }

    nonisolated func reconcile(update: AppUpdate) {
        Task { @MainActor [weak self] in
            self?.apply(update: update)
        }
    }

    private func apply(update: AppUpdate) {
        switch update {
        case let .fullState(s):
            if s.rev <= lastRevApplied { return }
            recordNwcWakeDebugChanges(nextState: s)
            notifyIfReceiveCompleted(nextState: s)
            lastRevApplied = s.rev
            state = s
            // If a Lightning receive completed (e.g. while backgrounded), release the
            // background-execution assertion now that the core no longer needs to run.
            if !isAwaitingLightningReceive {
                endReceiveBackgroundTask()
            }
        case let .haptic(feedback):
            Haptics.play(feedback)
        case let .openUrl(_, url):
            Task {
                let opened: Bool
                if let target = URL(string: url) {
                    opened = await NwaCallbackOpener.open(target)
                } else {
                    opened = false
                }
                dispatch(.completeNwaCallbackOpen(opened: opened))
            }
        case let .nwcConnectionExportReady(_, connectionId, name, uri, copyToClipboard, presentQr):
            if copyToClipboard {
                UIPasteboard.general.string = uri
                Haptics.play(.impactLight)
            }
            if presentQr {
                nwcConnectionExport = NwcConnectionExport(id: connectionId, name: name, uri: uri)
            }
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

    private func notifyIfReceiveCompleted(nextState: AppState) {
        guard let key = Self.receiveNotificationKey(nextState.receive) else { return }
        guard key != lastReceiveNotificationKey else { return }

        lastReceiveNotificationKey = key
        schedulePaymentReceivedNotification(receive: nextState.receive)
    }

    private static func receiveNotificationKey(_ receive: ReceiveState) -> String? {
        guard receive.phase == .success else { return nil }

        if let paymentHash = receive.lightningPaymentHash, receive.lightningPaid {
            return "lightning:\(paymentHash)"
        }
        if let arkAddress = receive.arkAddress {
            return "ark:\(arkAddress):\(receive.amountSat)"
        }
        return nil
    }

    private func schedulePaymentReceivedNotification(receive: ReceiveState) {
        let content = UNMutableNotificationContent()
        content.title = "Payment received"
        if receive.amountSat > 0 {
            content.body = "Received \(receive.amountSat.formatted()) sats"
        } else {
            content.body = "Received payment"
        }
        content.sound = .default
        content.threadIdentifier = "wallet-receive"
        content.userInfo = [
            "type": "payment_received",
        ]

        let request = UNNotificationRequest(
            identifier: "payment-received-\(UUID().uuidString)",
            content: content,
            trigger: nil
        )

        UNUserNotificationCenter.current().add(request) { error in
            if let error {
                NSLog("RebelWallet failed to schedule receive notification: %@", String(describing: error))
            }
        }
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

    func handleOpenURL(_ url: URL) {
        guard url.scheme?.hasPrefix("nostr+walletauth") == true else { return }
        dispatch(.openNwaRequest(uri: url.absoluteString))
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
                self?.syncPushNotificationRegistration(
                    status: status ?? "Unknown",
                    deviceToken: deviceToken
                )
            }
        }
        notificationObservers.append(observer)
    }

    private func syncPushNotificationRegistration(status: String, deviceToken: String?) {
        dispatch(.setPushNotificationRegistration(
            apnsDeviceToken: deviceToken,
            registrationStatus: status,
            wakeServerUrl: NwcPushPlatformContext.serverURL,
            appId: Bundle.main.bundleIdentifier ?? "com.rebelwallet.app",
            environment: NwcPushPlatformContext.apnsEnvironment,
            installId: NwcPushPlatformContext.installId
        ))
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
        let timeout = Date().addingTimeInterval(100)
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

struct AppStoragePaths {
    let dataDir: String
    let cacheDir: String
}

private enum AppStoragePreparer {
    static func prepare() async -> AppStoragePaths {
        await Task.detached(priority: .userInitiated) {
            prepareSynchronously()
        }.value
    }

    static func prepareSynchronously() -> AppStoragePaths {
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

        return AppStoragePaths(dataDir: dataDir, cacheDir: cacheDir)
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
}
