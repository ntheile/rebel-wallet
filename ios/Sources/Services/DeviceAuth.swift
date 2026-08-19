import LocalAuthentication

/// Gates secret reveals behind device owner authentication.
enum DeviceAuth {
    /// Prompts the user to authenticate with biometrics or the device
    /// passcode. Returns false when the user cancels or authentication fails.
    static func authenticate(localizedReason: String) async -> Bool {
        let context = LAContext()
        do {
            return try await context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: localizedReason)
        } catch {
            return false
        }
    }
}
