# Sitewalk iOS

SwiftUI app running on the real `murmur-core` engine via the `crates/ffi`
UniFFI bridge (falls back to a scripted demo engine when no API key is
configured — see `GalleryApp.resolveEngine`).

## Build & run (real engine)

```sh
./build-ffi.sh       # from apps/ios or repo root — regenerates the gitignored
                     # MurmurCoreFFI xcframework + Swift bindings (slow first run)
./generate.sh        # generates SitewalkGallery.xcodeproj AND injects the API key
                     # (ANTHROPIC_API_KEY from the repo-root .env) as PPQ_API_KEY
xcodebuild -project SitewalkGallery.xcodeproj -scheme SitewalkGallery \
  -destination 'platform=iOS Simulator,name=iPhone 17' build
```

`./generate.sh` is the single command to run before building: it writes the
gitignored `project.local.yml` (from `project.local.yml.template`) with the key
pulled from `.env`, then runs `xcodegen`. The key flows only into that gitignored
spec and the generated (gitignored) `.xcodeproj`; xcodebuild expands
`$(PPQ_API_KEY)` into the built app's Info.plist at build time. No tracked file
ever holds the secret. With no key present the app builds against the demo engine.

Confirm which engine is live from the console (no key is ever logged):

```sh
xcrun simctl spawn booted log show --last 2m \
  --predicate 'subsystem == "com.damsac.sitewalk"' --info | grep engine=
# engine=real (murmur-core MurmurEngine, key len=...)   <- real core active
```

Design source of truth: `../../docs/design/BRIEF.md` (rationale) and
`../../docs/design/mockup.html` (visual reference, open in a browser).

Launch args: `autoflow=1` (scripted walk plays itself), `autopdf=1` (+ PDF),
`live=1` (real mic + on-device STT), `screen=<page>` (design gallery).
