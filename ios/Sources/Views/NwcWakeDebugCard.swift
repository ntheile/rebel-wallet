import SwiftUI
import UIKit

struct NwcWakeDebugCard: View {
    @Bindable var manager: AppManager
    @State private var detailsExpanded = false
    @State private var copiedDeviceToken = false

    private var apnsDeviceToken: String {
        manager.state.pushNotifications.apnsDeviceToken ?? ""
    }

    private var pushRegistrationStatus: String {
        manager.state.pushNotifications.registrationStatus
    }

    var body: some View {
        SettingsCard(title: "NWC Wake Debug") {
            VStack(alignment: .leading, spacing: 12) {
                VStack(alignment: .leading, spacing: 6) {
                    NwcDebugValueRow(title: "Status", value: manager.state.nwc.lastWakeStatus)
                    NwcDebugValueRow(title: "Pending", value: "\(manager.state.nwc.pendingWakeRequests.count)")
                    NwcDebugValueRow(title: "Processed", value: "\(manager.state.nwc.processedWakeRequests.count)")
                }

                DisclosureGroup(isExpanded: $detailsExpanded) {
                    VStack(alignment: .leading, spacing: 12) {
                        if let latest = manager.state.nwc.processedWakeRequests.last {
                            VStack(alignment: .leading, spacing: 6) {
                                NwcDebugValueRow(title: "Last method", value: latest.method)
                                NwcDebugValueRow(title: "Last event", value: latest.eventId)
                            }
                        }

                        VStack(alignment: .leading, spacing: 10) {
                            HStack(spacing: 8) {
                                Text("APNs")
                                    .font(.caption.bold())
                                    .foregroundStyle(mutedText)
                                Spacer()
                                Text(pushRegistrationStatus)
                                    .font(.caption.bold())
                                    .foregroundStyle(apnsDeviceToken.isEmpty ? mutedText : rebelGreen)
                            }

                            Text(apnsDeviceToken.isEmpty ? "No device token yet" : apnsDeviceToken)
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(apnsDeviceToken.isEmpty ? mutedText : primaryText)
                                .textSelection(.enabled)
                                .lineLimit(nil)
                                .fixedSize(horizontal: false, vertical: true)

                            Button {
                                UIPasteboard.general.string = apnsDeviceToken
                                copiedDeviceToken = true
                                DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
                                    copiedDeviceToken = false
                                }
                            } label: {
                                Label(copiedDeviceToken ? "Copied" : "Copy token", systemImage: copiedDeviceToken ? "checkmark" : "doc.on.doc")
                                    .font(.caption.bold())
                            }
                            .buttonStyle(.bordered)
                            .disabled(apnsDeviceToken.isEmpty)
                        }
                        .padding(12)
                        .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
                        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))

                        HStack(spacing: 8) {
                            Button {
                                manager.refreshNwcWakeDebugEntries()
                            } label: {
                                Label("Refresh", systemImage: "arrow.clockwise")
                                    .font(.caption.bold())
                            }
                            .buttonStyle(.bordered)

                            Button {
                                manager.clearNwcWakeDebugEntries()
                            } label: {
                                Label("Clear", systemImage: "trash")
                                    .font(.caption.bold())
                            }
                            .buttonStyle(.bordered)
                        }

                        VStack(alignment: .leading, spacing: 8) {
                            if manager.nwcWakeDebugEntries.isEmpty {
                                Text("No wake debug entries yet")
                                    .font(.caption)
                                    .foregroundStyle(mutedText)
                            } else {
                                ForEach(manager.nwcWakeDebugEntries.prefix(10)) { entry in
                                    VStack(alignment: .leading, spacing: 3) {
                                        HStack(spacing: 6) {
                                            Text(entry.source)
                                                .font(.caption.bold())
                                                .foregroundStyle(rebelGreen)
                                            Text(entry.timestampText)
                                                .font(.caption2)
                                                .foregroundStyle(mutedText)
                                        }
                                        Text(entry.message)
                                            .font(.system(.caption, design: .monospaced))
                                            .foregroundStyle(primaryText)
                                            .textSelection(.enabled)
                                            .lineLimit(nil)
                                            .fixedSize(horizontal: false, vertical: true)
                                    }
                                    .padding(.vertical, 4)
                                }
                            }
                        }
                    }
                    .padding(.top, 8)
                } label: {
                    Text("Details")
                        .font(.caption.bold())
                        .foregroundStyle(primaryText)
                }
                .tint(mutedText)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 12)
        }
    }
}

private struct NwcDebugValueRow: View {
    let title: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
                .font(.caption.bold())
                .foregroundStyle(mutedText)
            Text(value.isEmpty ? "None" : value)
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(primaryText)
                .textSelection(.enabled)
                .lineLimit(nil)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}
