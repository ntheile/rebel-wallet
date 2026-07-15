Once you're done with a change, build it on to my phone with `just run-ios-phone` if my iPhone is visible to
`xcrun devicectl list devices`. A Wi-Fi paired phone may show as `available (paired)` instead of `connected`; that
still counts. We don't need to build the app for the simulator.

## Local iOS signing for external contributors

The checked-in `.env.sample` contains shared project defaults. External contributors must keep their Apple account,
team, and bundle identifiers in the ignored `.env.local` file instead of editing `.env.sample`, `ios/project.yml`, or
the Justfile. Start with:

```bash
cp .env.sample .env.local
```

Then replace the signing values with identifiers owned by the contributor's Apple Developer team:

```env
IOS_BUNDLE_ID=com.YOURORG.rebelwallet
IOS_DEVELOPMENT_TEAM=YOURTEAMID
IOS_CODE_SIGN_STYLE=Automatic
IOS_APS_ENVIRONMENT=development
IOS_APP_GROUP_ID=group.com.YOURORG.rebelwallet
NWC_WAKE_SERVER_URL=
```

`IOS_BUNDLE_ID` must be globally unique. XcodeGen derives the notification service extension bundle identifier as
`com.YOURORG.rebelwallet.NotificationService`. In the Apple Developer portal, register the main app identifier, that
notification service extension identifier, and the `group.com.YOURORG.rebelwallet` app group. Enable App Groups and
Keychain Sharing for both targets, and enable Push Notifications for the main app target. Add the contributor's Apple
account under Xcode Settings > Accounts so automatic signing can create or download development profiles.

After changing `.env.local`, regenerate the project and install it on a connected iPhone:

```bash
just ios-xcodeproj
just run-ios-phone com.YOURORG.rebelwallet
```

The bundle ID argument passed to `run-ios-phone` must match `IOS_BUNDLE_ID` in `.env.local`. Contributors can omit the
argument and run `just run-ios-phone` when they want the recipe to use `IOS_BUNDLE_ID` automatically.

Local generation can put personal signing values in `ios/App.xcodeproj/project.pbxproj`; do not commit those values.
The pre-commit hook verifies the committed project against `.env.sample` and regenerates shared defaults when needed.
`NWC_WAKE_SERVER_URL` may remain empty for basic local builds, but push-based NWC Wake testing requires a reachable
notification server.

Nostr profile metadata and profile pictures are cache-managed by the Rust core. When adding or changing a profile
fetch path, route kind-0 metadata through `profile_contact_from_metadata_json` / `FetchedProfileContact`, then through
the actor's fetched-profile cache helpers before updating UI state. Swift views should render Rust-provided cached
profile image URLs and should not fetch remote pfps directly except for explicit edit-preview UI.
