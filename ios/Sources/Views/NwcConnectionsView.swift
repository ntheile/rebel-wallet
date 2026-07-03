import SwiftUI
import UIKit

struct NwcConnectionsView: View {
    @Bindable var manager: AppManager
    @Environment(\.walletAccent) private var walletAccent
    @State private var name = ""
    @State private var relay = "wss://relay.getalby.com/v1"
    @State private var budgetText = "10000"
    @State private var budgetInterval: NwcBudgetInterval = .daily
    @State private var selectedPermissions = Set<NwcPermission>([.getInfo, .getBalance])
    @State private var copiedConnectionId: String?
    @State private var deleteConnectionId: String?

    private var connections: [NwcConnection] {
        manager.state.nwc.connections
    }

    private var parsedBudget: UInt64? {
        let cleaned = budgetText
            .replacingOccurrences(of: ",", with: "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if cleaned.isEmpty {
            return 0
        }
        return UInt64(cleaned)
    }

    private var canCreate: Bool {
        parsedBudget != nil
            && !relay.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                websocketSection
                createSection
                connectionsSection
                NwcWakeDebugCard(manager: manager)
            }
            .padding(16)
        }
        .navigationTitle("NWC")
        .background(pageBackground)
        .foregroundStyle(primaryText)
        .onAppear {
            relay = manager.state.nwc.defaultRelay
        }
        .alert("Delete NWC string?", isPresented: Binding(
            get: { deleteConnectionId != nil },
            set: { if !$0 { deleteConnectionId = nil } }
        )) {
            Button("Delete", role: .destructive) {
                if let deleteConnectionId {
                    manager.dispatch(.deleteNwcConnection(id: deleteConnectionId))
                }
                deleteConnectionId = nil
            }
            Button("Cancel", role: .cancel) {
                deleteConnectionId = nil
            }
        }
    }

    private var websocketSection: some View {
        SettingsCard(title: "NWC Websocket") {
            VStack(alignment: .leading, spacing: 14) {
                HStack(spacing: 12) {
                    NwcOnlineIndicator(
                        enabled: manager.state.nwc.websocketEnabled,
                        online: manager.state.nwc.websocketOnline
                    )

                    VStack(alignment: .leading, spacing: 3) {
                        Text(websocketTitle)
                            .font(.headline)
                        Text(manager.state.nwc.websocketStatus)
                            .font(.caption)
                            .foregroundStyle(mutedText)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }

                    Spacer()

                    Toggle("Websocket", isOn: Binding(
                        get: { manager.state.nwc.websocketEnabled },
                        set: { manager.dispatch(.setNwcWebsocketEnabled(enabled: $0)) }
                    ))
                    .labelsHidden()
                    .tint(rebelGreen)
                }
                .padding(14)
                .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
                .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
            }
            .padding(14)
        }
    }

    private var websocketTitle: String {
        if manager.state.nwc.websocketOnline {
            return "Online"
        }
        if manager.state.nwc.websocketEnabled {
            return "Connecting"
        }
        return "Offline"
    }

    private var createSection: some View {
        SettingsCard(title: "New NWC String") {
            VStack(alignment: .leading, spacing: 14) {
                TextField("Name", text: $name)
                    .textInputAutocapitalization(.words)
                    .profileField()

                TextField("Relay", text: $relay)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.URL)
                    .profileField()

                VStack(alignment: .leading, spacing: 10) {
                    Text("Budget")
                        .font(.caption.bold())
                        .foregroundStyle(mutedText)

                    HStack(spacing: 10) {
                        TextField("Sats", text: $budgetText)
                            .keyboardType(.numberPad)
                            .profileField()

                        ForEach([1_000, 10_000, 50_000], id: \.self) { amount in
                            Button {
                                budgetText = "\(amount)"
                                manager.requestHaptic(.selection)
                            } label: {
                                Text(compactSats(amount))
                                    .font(.caption.bold())
                                    .frame(minWidth: 44)
                            }
                            .buttonStyle(SecondaryButtonStyle())
                        }
                    }
                }

                VStack(alignment: .leading, spacing: 10) {
                    Text("Interval")
                        .font(.caption.bold())
                        .foregroundStyle(mutedText)

                    Picker("Interval", selection: $budgetInterval) {
                        ForEach(NwcBudgetInterval.createOptions, id: \.self) { interval in
                            Text(interval.title)
                                .tag(interval)
                        }
                    }
                    .pickerStyle(.segmented)
                }

                permissionsSection

                Button {
                    createConnection()
                } label: {
                    Label("Create NWC string", systemImage: "plus.circle.fill")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(PrimaryButtonStyle(color: walletAccent))
                .disabled(!canCreate)
            }
            .padding(14)
        }
    }

    private var connectionsSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Saved Strings")
                    .font(.caption.bold())
                    .foregroundStyle(mutedText)
                Spacer()
                Text("\(connections.count)")
                    .font(.caption.bold())
                    .foregroundStyle(connections.isEmpty ? mutedText : rebelGreen)
            }
            .padding(.horizontal, 4)

            if connections.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    Image(systemName: "link.badge.plus")
                        .font(.title2)
                        .foregroundStyle(walletAccent)
                    Text("No NWC strings")
                        .font(.headline)
                    Text("Create one above to authorize a Nostr Wallet Connect client.")
                        .font(.caption)
                        .foregroundStyle(mutedText)
                }
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(surfaceBackground, in: RoundedRectangle(cornerRadius: 8))
                .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
            } else {
                ForEach(connections, id: \.id) { connection in
                    NwcConnectionCard(
                        connection: connection,
                        copied: copiedConnectionId == connection.id,
                        copy: { copy(connection) },
                        delete: { deleteConnectionId = connection.id }
                    )
                }
            }
        }
    }

    private func createConnection() {
        guard let parsedBudget else { return }
        manager.dispatch(.createNwcConnection(
            name: name,
            relay: relay,
            budgetSat: parsedBudget,
            budgetInterval: budgetInterval,
            permissions: selectedPermissions.sortedForDisplay
        ))
        name = ""
    }

    private func copy(_ connection: NwcConnection) {
        UIPasteboard.general.string = connection.uri
        copiedConnectionId = connection.id
        manager.requestHaptic(.impactLight)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            if copiedConnectionId == connection.id {
                copiedConnectionId = nil
            }
        }
    }

    private var permissionsSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Permissions")
                    .font(.caption.bold())
                    .foregroundStyle(mutedText)

                Spacer()

                Button("All") {
                    selectedPermissions = Set(NwcPermission.createOptions)
                    manager.requestHaptic(.selection)
                }
                .font(.caption.bold())
                .foregroundStyle(rebelGreen)

                Button("None") {
                    selectedPermissions.removeAll()
                    manager.requestHaptic(.selection)
                }
                .font(.caption.bold())
                .foregroundStyle(mutedText)
            }

            VStack(spacing: 10) {
                ForEach(NwcPermission.createOptions, id: \.self) { permission in
                    NwcPermissionToggleRow(
                        icon: permission.icon,
                        title: permission.title,
                        caption: permission.methodName,
                        color: permission.color(walletAccent: walletAccent),
                        isOn: Binding(
                            get: { selectedPermissions.contains(permission) },
                            set: { enabled in
                                if enabled {
                                    selectedPermissions.insert(permission)
                                } else {
                                    selectedPermissions.remove(permission)
                                }
                            }
                        )
                    )
                }
            }
        }
    }
}

private struct NwcOnlineIndicator: View {
    let enabled: Bool
    let online: Bool

    private var color: Color {
        if online {
            return rebelGreen
        }
        if enabled {
            return rebelBlue
        }
        return mutedText
    }

    var body: some View {
        ZStack {
            Circle()
                .fill(color.opacity(0.18))
            Circle()
                .fill(color)
                .frame(width: 13, height: 13)
                .shadow(color: online ? color.opacity(0.65) : .clear, radius: 6)
        }
        .frame(width: 38, height: 38)
        .accessibilityLabel(online ? "NWC websocket online" : "NWC websocket offline")
    }
}

private struct NwcPermissionToggleRow: View {
    let icon: String
    let title: String
    let caption: String
    let color: Color
    @Binding var isOn: Bool

    var body: some View {
        Toggle(isOn: $isOn) {
            HStack(spacing: 12) {
                ZStack {
                    RoundedRectangle(cornerRadius: 8)
                        .fill(color.opacity(0.18))
                    Image(systemName: icon)
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(color)
                }
                .frame(width: 36, height: 36)

                VStack(alignment: .leading, spacing: 3) {
                    Text(title)
                        .font(.subheadline.bold())
                    Text(caption)
                        .font(.caption)
                        .foregroundStyle(mutedText)
                }
            }
        }
        .tint(color)
        .padding(12)
        .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
    }
}

private struct NwcConnectionCard: View {
    let connection: NwcConnection
    let copied: Bool
    let copy: () -> Void
    let delete: () -> Void

    private var lastUsedText: String {
        guard let lastUsedAt = connection.lastUsedAt else {
            return "Never used"
        }
        return "Last used \(shortDate(lastUsedAt))"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 12) {
                ZStack {
                    RoundedRectangle(cornerRadius: 8)
                        .fill(rebelBlue.opacity(0.2))
                    Image(systemName: "link")
                        .font(.headline)
                        .foregroundStyle(rebelBlue)
                }
                .frame(width: 42, height: 42)

                VStack(alignment: .leading, spacing: 4) {
                    Text(connection.name)
                        .font(.headline)
                    Text(connection.relay)
                        .font(.caption)
                        .foregroundStyle(mutedText)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }

                Spacer()

                Button(action: delete) {
                    Image(systemName: "trash")
                        .font(.headline)
                        .foregroundStyle(rebelRed)
                        .frame(width: 36, height: 36)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Delete NWC string")
            }

            VStack(alignment: .leading, spacing: 6) {
                Text(truncateMiddle(connection.uri, maxLength: 80, prefixCount: 28))
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(primaryText)
                    .textSelection(.enabled)
                    .lineLimit(3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))

            HStack(spacing: 8) {
                NwcPolicyPill(text: connection.budgetDisplay, color: primaryText)
                NwcPolicyPill(text: connection.budgetIntervalDisplay, color: primaryText)
            }

            LazyVGrid(columns: [GridItem(.adaptive(minimum: 108), spacing: 8)], alignment: .leading, spacing: 8) {
                ForEach(connection.enabledPermissionsForDisplay, id: \.self) { permission in
                    NwcPolicyPill(text: permission.shortTitle, color: permission.color(walletAccent: rebelBlue))
                }
            }

            HStack(spacing: 10) {
                Button(action: copy) {
                    Label(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(SecondaryButtonStyle())

                ShareLink(item: connection.uri) {
                    Label("Share", systemImage: "square.and.arrow.up")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(SecondaryButtonStyle())
            }

            HStack(spacing: 8) {
                Text("Created \(shortDate(connection.createdAt))")
                Text(lastUsedText)
            }
            .font(.caption2)
            .foregroundStyle(mutedText)
        }
        .padding(16)
        .background(surfaceBackground, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
    }
}

private struct NwcPolicyPill: View {
    let text: String
    let color: Color

    var body: some View {
        Text(text)
            .font(.caption.bold())
            .lineLimit(1)
            .minimumScaleFactor(0.8)
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .foregroundStyle(color)
            .background(color.opacity(0.14), in: Capsule())
    }
}

private extension NwcConnection {
    var enabledPermissionsForDisplay: [NwcPermission] {
        if permissionsConfigured {
            return permissions.sortedForDisplay
        }

        var legacy: [NwcPermission] = [.getInfo]
        if allowGetBalance {
            legacy.append(.getBalance)
        }
        if allowPayInvoice {
            legacy.append(.payInvoice)
        }
        return legacy.sortedForDisplay
    }
}

private extension Set where Element == NwcPermission {
    var sortedForDisplay: [NwcPermission] {
        NwcPermission.createOptions.filter { contains($0) }
    }
}

private extension Array where Element == NwcPermission {
    var sortedForDisplay: [NwcPermission] {
        NwcPermission.createOptions.filter { contains($0) }
    }
}

private extension NwcPermission {
    static var createOptions: [NwcPermission] {
        [
            .getInfo,
            .getBalance,
            .payInvoice,
            .payKeysend,
            .makeInvoice,
            .lookupInvoice,
            .listTransactions,
            .makeHoldInvoice,
            .cancelHoldInvoice,
            .settleHoldInvoice
        ]
    }

    var title: String {
        switch self {
        case .getInfo: return "Get info"
        case .getBalance: return "Get balance"
        case .payInvoice: return "Pay invoices"
        case .payKeysend: return "Pay keysend"
        case .makeInvoice: return "Make invoices"
        case .lookupInvoice: return "Lookup invoices"
        case .listTransactions: return "List transactions"
        case .makeHoldInvoice: return "Make hold invoices"
        case .cancelHoldInvoice: return "Cancel hold invoices"
        case .settleHoldInvoice: return "Settle hold invoices"
        }
    }

    var shortTitle: String {
        switch self {
        case .getInfo: return "Info"
        case .getBalance: return "Balance"
        case .payInvoice: return "Pay"
        case .payKeysend: return "Keysend"
        case .makeInvoice: return "Invoice"
        case .lookupInvoice: return "Lookup"
        case .listTransactions: return "History"
        case .makeHoldInvoice: return "Hold"
        case .cancelHoldInvoice: return "Cancel hold"
        case .settleHoldInvoice: return "Settle hold"
        }
    }

    var methodName: String {
        switch self {
        case .getInfo: return "get_info"
        case .getBalance: return "get_balance"
        case .payInvoice: return "pay_invoice"
        case .payKeysend: return "pay_keysend"
        case .makeInvoice: return "make_invoice"
        case .lookupInvoice: return "lookup_invoice"
        case .listTransactions: return "list_transactions"
        case .makeHoldInvoice: return "make_hold_invoice"
        case .cancelHoldInvoice: return "cancel_hold_invoice"
        case .settleHoldInvoice: return "settle_hold_invoice"
        }
    }

    var icon: String {
        switch self {
        case .getInfo: return "info.circle"
        case .getBalance: return "eye"
        case .payInvoice: return "bolt.fill"
        case .payKeysend: return "paperplane.fill"
        case .makeInvoice: return "plus.app"
        case .lookupInvoice: return "magnifyingglass"
        case .listTransactions: return "list.bullet.rectangle"
        case .makeHoldInvoice: return "lock.doc"
        case .cancelHoldInvoice: return "xmark.circle"
        case .settleHoldInvoice: return "checkmark.seal"
        }
    }

    func color(walletAccent: Color) -> Color {
        switch self {
        case .getInfo: return rebelBlue
        case .getBalance: return rebelGreen
        case .payInvoice: return walletAccent
        case .payKeysend: return rebelRed
        case .makeInvoice: return Color.cyan
        case .lookupInvoice: return Color.orange
        case .listTransactions: return Color.indigo
        case .makeHoldInvoice: return Color.yellow
        case .cancelHoldInvoice: return mutedText
        case .settleHoldInvoice: return Color.mint
        }
    }
}

private extension NwcBudgetInterval {
    static var createOptions: [NwcBudgetInterval] {
        [.hourly, .daily, .weekly, .monthly]
    }

    var title: String {
        switch self {
        case .hourly: return "Hourly"
        case .daily: return "Daily"
        case .weekly: return "Weekly"
        case .monthly: return "Monthly"
        }
    }
}

private func compactSats(_ amount: Int) -> String {
    if amount >= 1_000 {
        return "\(amount / 1_000)k"
    }
    return "\(amount)"
}

private func shortDate(_ unix: UInt64) -> String {
    let date = Date(timeIntervalSince1970: TimeInterval(unix))
    return date.formatted(date: .abbreviated, time: .shortened)
}
