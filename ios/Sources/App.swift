import SwiftUI

@main
struct RebelWalletApp: App {
    @UIApplicationDelegateAdaptor(RebelWalletAppDelegate.self) private var appDelegate
    @State private var manager: AppManager?
    @State private var pendingOpenURL: URL?
    @State private var easterEgg = WalletEasterEgg()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            Group {
                if let manager {
                    ContentView(manager: manager)
                } else {
                    LaunchSplashView()
                }
            }
                .environment(\.walletAccent, easterEgg.accentColor)
                .environment(\.walletUsesDellLogo, easterEgg.isDellMode)
                .preferredColorScheme(.dark)
                .task {
                    await loadManagerIfNeeded()
                }
                .onOpenURL { url in
                    if let manager {
                        manager.handleOpenURL(url)
                    } else {
                        pendingOpenURL = url
                    }
                }
                .onAppear {
                    easterEgg.start()
                    manager?.dispatch(.foregrounded)
                }
                .onDisappear {
                    easterEgg.stop()
                }
                .onChange(of: scenePhase) { _, phase in
                    guard let manager else { return }
                    switch phase {
                    case .active:
                        manager.drainQueuedNwcWakeRequests()
                        manager.dispatch(.foregrounded)
                        // Re-attempt claiming an in-flight Lightning receive in case
                        // the payment landed while the app was suspended.
                        manager.dispatch(.resumeReceiveMonitor)
                        // Sweep for any Lightning receive whose HTLCs arrived while we
                        // were away, independent of the receive screen, so it can't get
                        // stuck in "claimable".
                        manager.dispatch(.claimPendingLightningReceives)
                        manager.endReceiveBackgroundTask()
                    case .background:
                        manager.dispatch(.backgrounded)
                        // Keep the core running briefly so an in-flight Lightning
                        // receive can still be claimed while backgrounded.
                        manager.beginReceiveBackgroundTaskIfNeeded()
                    default:
                        break
                    }
                }
        }
    }

    @MainActor
    private func loadManagerIfNeeded() async {
        guard manager == nil else { return }
        let storagePaths = await AppManager.prepareStorage()
        let loadedManager = AppManager(storagePaths: storagePaths)
        manager = loadedManager
        if let pendingOpenURL {
            loadedManager.handleOpenURL(pendingOpenURL)
            self.pendingOpenURL = nil
        }
    }
}
