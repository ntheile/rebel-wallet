import SwiftUI
import UIKit

struct NwcConnectionsView: View {
    @Bindable var manager: AppManager
    @Environment(\.walletAccent) private var walletAccent
    @State private var copiedConnectionId: String?
    @State private var deleteConnection: NwcConnection?

    private var connections: [NwcConnection] {
        manager.state.nwc.connections
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                createLink
                connectionsSection
                websocketSection
                NwcWakeDebugCard(manager: manager)
            }
            .padding(16)
        }
        .navigationTitle("NWC")
        .background(pageBackground)
        .foregroundStyle(primaryText)
        .alert("Delete NWC string?", isPresented: Binding(
            get: { deleteConnection != nil },
            set: { if !$0 { deleteConnection = nil } }
        )) {
            Button("Delete", role: .destructive) {
                if let deleteConnection {
                    manager.unregisterNwcWakeConnection(deleteConnection)
                    manager.dispatch(.deleteNwcConnection(id: deleteConnection.id))
                }
                deleteConnection = nil
            }
            Button("Cancel", role: .cancel) {
                deleteConnection = nil
            }
        }
    }

    private var createLink: some View {
        NavigationLink {
            NwcCreateConnectionView(manager: manager)
        } label: {
            NwcCreateHeroButton(connectionCount: connections.count)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Create NWC")
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

                Text("Optional. NWC Wake handles background delivery; keeping the websocket on can respond faster while Rebel Wallet is open and foregrounded.")
                    .font(.caption)
                    .foregroundStyle(mutedText)
                    .fixedSize(horizontal: false, vertical: true)
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
                    Text("Create one to authorize a Nostr Wallet Connect client.")
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
                        uri: nwcUri(connection),
                        copied: copiedConnectionId == connection.id,
                        copy: { copy(connection) },
                        delete: { deleteConnection = connection }
                    )
                }
            }
        }
    }

    private func copy(_ connection: NwcConnection) {
        UIPasteboard.general.string = nwcUri(connection)
        copiedConnectionId = connection.id
        manager.requestHaptic(.impactLight)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            if copiedConnectionId == connection.id {
                copiedConnectionId = nil
            }
        }
    }

    private func nwcUri(_ connection: NwcConnection) -> String {
        NwcWakeRegistrationService.uriWithWake(connection.uri)
    }
}

private struct NwcCreateConnectionView: View {
    @Bindable var manager: AppManager
    @Environment(\.walletAccent) private var walletAccent
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var relay = "wss://relay.getalby.com/v1"
    @State private var budgetText = "10000"
    @State private var budgetInterval: NwcBudgetInterval = .daily
    @State private var permissionPreset: NwcPermissionPreset = .fullAccess
    @State private var selectedPermissions = Set<NwcPermission>(NwcPermissionPreset.fullAccess.permissions)
    @State private var pendingCreatedConnectionCopyAfterId: String?

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

    private var permissionsForCreate: [NwcPermission] {
        switch permissionPreset {
        case .fullAccess, .readOnly:
            return permissionPreset.permissions
        case .custom:
            return selectedPermissions.sortedForDisplay
        }
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                NwcConnectionVisualization()
                    .frame(maxWidth: .infinity)

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
                            Label("Create NWC", systemImage: "plus.circle.fill")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(PrimaryButtonStyle(color: walletAccent))
                        .disabled(!canCreate)
                    }
                    .padding(14)
                }
            }
            .padding(16)
        }
        .navigationTitle("Create NWC")
        .background(pageBackground)
        .foregroundStyle(primaryText)
        .onAppear {
            relay = manager.state.nwc.defaultRelay
        }
        .onChange(of: manager.state.nwc.connections) { _, newConnections in
            copyPendingCreatedConnection(from: newConnections)
        }
    }

    private var permissionsSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Permissions")
                .font(.caption.bold())
                .foregroundStyle(mutedText)

            NwcPermissionPresetMenu(
                selection: permissionPreset,
                select: selectPermissionPreset
            )

            if permissionPreset == .custom {
                HStack {
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

    private func createConnection() {
        guard let parsedBudget else { return }
        pendingCreatedConnectionCopyAfterId = connections.last?.id ?? ""
        manager.dispatch(.createNwcConnection(
            name: name,
            relay: relay,
            budgetSat: parsedBudget,
            budgetInterval: budgetInterval,
            permissions: permissionsForCreate
        ))
    }

    private func copyPendingCreatedConnection(from updatedConnections: [NwcConnection]) {
        guard let previousLastId = pendingCreatedConnectionCopyAfterId else { return }
        guard let newest = updatedConnections.last else { return }
        guard newest.id != previousLastId else { return }

        pendingCreatedConnectionCopyAfterId = nil
        UIPasteboard.general.string = NwcWakeRegistrationService.uriWithWake(newest.uri)
        manager.requestHaptic(.impactLight)
        dismiss()
    }

    private func selectPermissionPreset(_ preset: NwcPermissionPreset) {
        permissionPreset = preset
        if preset != .custom {
            selectedPermissions = Set(preset.permissions)
        }
        manager.requestHaptic(.selection)
    }
}

private struct NwcCreateHeroButton: View {
    let connectionCount: Int
    @Environment(\.walletAccent) private var walletAccent

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            NwcConnectionVisualization()

            Label("Create NWC", systemImage: "plus.circle.fill")
                .font(.headline)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 12)
                .foregroundStyle(primaryText)
                .background(walletAccent, in: RoundedRectangle(cornerRadius: 8))
        }
        .padding(16)
        .background(surfaceBackground, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
    }
}

private struct NwcConnectionVisualization: View {
    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 10) {
                RebelMark(size: 68)
                Text("Rebel Wallet")
                    .font(.caption.bold())
                    .foregroundStyle(primaryText)
                    .lineLimit(1)
                    .minimumScaleFactor(0.8)
            }
            .frame(maxWidth: .infinity)

            ZStack {
                Rectangle()
                    .fill(
                        LinearGradient(
                            colors: [.clear, mutedText.opacity(0.65), .clear],
                            startPoint: .leading,
                            endPoint: .trailing
                        )
                    )
                    .frame(height: 4)

                Image(systemName: "cable.connector.horizontal")
                    .font(.system(size: 34, weight: .medium))
                    .foregroundStyle(mutedText)
                    .padding(.horizontal, 8)
                    .background(Color.black, in: Capsule())
            }
            .frame(maxWidth: .infinity)

            VStack(spacing: 10) {
                ZStack {
                    RoundedRectangle(cornerRadius: 16)
                        .fill(Color.white.opacity(0.92))
                    Image(systemName: "square.grid.2x2")
                        .font(.system(size: 38, weight: .semibold))
                        .foregroundStyle(Color.black.opacity(0.55))
                }
                .frame(width: 68, height: 68)

                Text("External App")
                    .font(.caption.bold())
                    .foregroundStyle(primaryText)
                    .lineLimit(1)
                    .minimumScaleFactor(0.8)
            }
            .frame(maxWidth: .infinity)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 18)
        .frame(minHeight: 138)
        .background(Color.black, in: RoundedRectangle(cornerRadius: 8))
    }
}

private enum NwcPermissionPreset: String, CaseIterable, Identifiable {
    case fullAccess
    case readOnly
    case custom

    var id: String { rawValue }

    var title: String {
        switch self {
        case .fullAccess: return "Full Access"
        case .readOnly: return "Read Only"
        case .custom: return "Custom"
        }
    }

    var caption: String {
        switch self {
        case .fullAccess: return "Send and receive payments"
        case .readOnly: return "Receive payments and view history"
        case .custom: return "Define specific permissions"
        }
    }

    var icon: String {
        switch self {
        case .fullAccess: return "arrow.up.arrow.down"
        case .readOnly: return "arrow.down"
        case .custom: return "square.and.pencil"
        }
    }

    var color: Color {
        switch self {
        case .fullAccess: return rebelGreen
        case .readOnly: return rebelBlue
        case .custom: return mutedText
        }
    }

    var permissions: [NwcPermission] {
        switch self {
        case .fullAccess:
            return NwcPermission.createOptions
        case .readOnly:
            return [
                .getInfo,
                .getBalance,
                .makeInvoice,
                .lookupInvoice,
                .listTransactions
            ]
        case .custom:
            return []
        }
    }
}

private struct NwcPermissionPresetMenu: View {
    let selection: NwcPermissionPreset
    let select: (NwcPermissionPreset) -> Void

    var body: some View {
        Menu {
            ForEach(NwcPermissionPreset.allCases) { preset in
                Button {
                    select(preset)
                } label: {
                    Label(preset.title, systemImage: preset.icon)
                }
            }
        } label: {
            NwcPermissionPresetLabel(preset: selection)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("NWC permission preset")
    }
}

private struct NwcPermissionPresetLabel: View {
    let preset: NwcPermissionPreset

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: preset.icon)
                .font(.system(size: 19, weight: .medium))
                .foregroundStyle(mutedText)
                .frame(width: 28, height: 28)

            VStack(alignment: .leading, spacing: 4) {
                Text(preset.title)
                    .font(.subheadline.bold())
                    .foregroundStyle(primaryText)

                Text(preset.caption)
                    .font(.caption)
                    .foregroundStyle(mutedText)
            }

            Spacer()

            Image(systemName: "chevron.up.chevron.down")
                .font(.caption.weight(.semibold))
                .foregroundStyle(mutedText)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
        .contentShape(RoundedRectangle(cornerRadius: 8))
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
    let uri: String
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

            HStack(spacing: 8) {
                NwcPolicyPill(text: connection.budgetDisplay, color: primaryText)
                NwcPolicyPill(text: connection.budgetIntervalDisplay, color: primaryText)
                if let permissionPreset = connection.permissionPresetForDisplay {
                    NwcPolicyPill(text: permissionPreset.title, color: permissionPreset.color)
                }
            }

            if connection.permissionPresetForDisplay == nil {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 108), spacing: 8)], alignment: .leading, spacing: 8) {
                    ForEach(connection.enabledPermissionsForDisplay, id: \.self) { permission in
                        NwcPolicyPill(text: permission.shortTitle, color: permission.color(walletAccent: rebelBlue))
                    }
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text(truncateMiddle(uri, maxLength: 80, prefixCount: 28))
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

            HStack(spacing: 10) {
                Button(action: copy) {
                    Label(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(SecondaryButtonStyle())

                ShareLink(item: uri) {
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
    var permissionPresetForDisplay: NwcPermissionPreset? {
        let enabled = Set(enabledPermissionsForDisplay)
        if enabled == Set(NwcPermissionPreset.fullAccess.permissions) {
            return .fullAccess
        }
        if enabled == Set(NwcPermissionPreset.readOnly.permissions) {
            return .readOnly
        }
        return nil
    }

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
