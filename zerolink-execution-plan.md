# ZeroLink — Execution Plan & Current Status

## Overview

ZeroLink is a cross-platform Android/Windows companion app with a shared Rust core, a Rust Windows client, and a Kotlin Android client. The repository includes clipboard, notifications, media controls, a photo catalog, and a source implementation for authenticated ranged file hydration. Pairing uses a v3 shared-secret proof with explicit approval on both sides. The Android debug app assembles. Android screen capture selects a supported HEVC surface input or flexible-YUV fallback off the UI thread. The API 36 emulator accepts the capture consent and foreground-service setup but exposes no usable HEVC size/rate combination, so frame delivery and Windows playback remain open. Windows prefers an asynchronous D3D11-aware Media Foundation HEVC decoder and presents its NV12 surfaces directly through a D3D11 video processor, with CPU/GDI fallback.

---

## 🛡️ Zero-Root Architecture Guarantee

ZeroLink is designed to operate on unrooted Android devices. Repository notes claim testing on a Samsung Galaxy SM-A075F running Android 14 / API 34, but that result was not independently reproduced in this review.

### How Background Clipboard Sync Operates Without Root:
On Android 10+ (API 29–34), background apps cannot directly query the system clipboard without window focus. ZeroLink implements a **Triple-Guard Zero-Root System** inspired by Sefirah and KDE Connect:

1. **Guard 1: Accessibility Service (`ZeroAccessibilityService`)**
   - Official Android Accessibility framework (`canRetrieveWindowContent="false"` for strict privacy).
   - Toggled via *Settings &rarr; Accessibility &rarr; Installed Apps &rarr; ZeroLink*.
   - When text selection, copy gestures, or app switching occur, it directly accesses `ClipboardManager` with the Accessibility Service's system identity and dispatches the update to Windows.
   - The same service enables `canPerformGestures="true"` for rootless `ScreenInput` injection (Phase 7); window content is still never retrieved.
2. **Guard 2: Status Bar Notification Action (`[ 📋 Sync Clipboard ]`)**
   - An instant 1-tap action button embedded directly inside ZeroLink's persistent foreground notification.
   - Pull down the notification shade from inside any app (Chrome, WhatsApp, Notes) and tap it to sync the current clipboard to Windows without switching apps.
3. **Guard 3: Quick Settings Tile (`ZeroClipboardTileService`)**
   - A dedicated quick tile in the Android notification panel (alongside Wi-Fi/Bluetooth) to sync clipboard in 1 tap.
4. **Anti-Echo Hashing:** SHA-256 / 64-bit SipHash tracking prevents remote sync feedback loops across devices.

---

## Architecture & Verification Matrix

```mermaid
flowchart LR
    subgraph Android["📱 Android (Samsung SM-A075F, Android 14)"]
        A1["Telemetry Manager<br/>(Battery & Charging 10s)"]
        A2["ZeroNotificationListener<br/>+ RemoteInput Extractor"]
        A3["Zero-Root Clipboard<br/>(A11y + Tile + Action)"]
        A4["Photo Catalog Sync<br/>(MediaStore Scanner)"]
        A5["H265ScreenEncoder<br/>(MediaProjection Hardware)"]
        A6["Media Card &<br/>Notification Controls"]
    end

    subgraph Fabric["⚡ Dual-Radio Fabric"]
        F1["Curve25519 ECDH + AES-256-GCM / ChaCha20"]
        F2["BLE L2CAP CoC + paired TCP bridge (:8989)<br/>On-demand AES-GCM bulk TCP (:8443)"]
    end

    subgraph Windows["🖥️ Windows Client (zerolink-win, Rust/Win32)"]
        W1["Live Dashboard<br/>(Device Name & Battery %)"]
        W2["Native WinRT Toast<br/>(Action Center + Quick Reply)"]
        W3["Win32 Clipboard Engine<br/>(HWND_MESSAGE Listener)"]
        W4["Virtual Cloud Filter Drive<br/>(cldapi.dll in Explorer)"]
        W5["HEVC Decoder + D3D11/GDI Viewer<br/>(Runtime validation pending)"]
        W6["GSMTC Media Session<br/>(Playback State & Commands)"]
    end

    A1 -->|Heartbeat (0x09)| Fabric --> W1
    A2 -->|Notification (0x03)| Fabric --> W2
    W2 -->|NotificationAction (0x0B)<br/>Inline Reply / Dismiss| Fabric --> A2
    A3 <-->|Clipboard (0x04)| Fabric <--> W3
    A4 -->|FileMetadata (0x05)| Fabric --> W4
    A5 -->|ScreenCast (0x08)| Fabric --> W5
    W5 -.->|ScreenInput (0x0D)| Fabric -.-> A5
    A6 <-->|MediaControl (0x0C) / MediaState (0x0E)| Fabric <--> W6
```

---

## Detailed Execution Status & Finished Steps

### Phase 1: Core Fabric, Transport & Security (INITIAL PAIRING AND PSK RECONNECT VERIFIED OVER TCP ON API 36 EMULATOR; PHYSICAL BLE VALIDATION PENDING)
- [x] **Zero-Copy Binary Framing (`framing.rs`):** 20-byte fixed header, CRC32 header integrity, magic bytes `0x5A4C` (`ZL`), 16 MB max frame boundary.
- [x] **Crypto Primitives (`crypto.rs`):** Curve25519 ECDH key exchange, HKDF key derivation, AES-256-GCM & ChaCha20-Poly1305 AEAD framing.
- [x] **Transport and pairing handshake:** X25519 ephemeral key exchange, directional AEAD keys, mutual transcript confirmation, first-pair code comparison/approval, and returning-peer PSK proofs. A matched first pairing and a reconnect after restarting Windows were verified over the emulator TCP bridge; reconnect completed without another approval prompt. Physical-device validation remains pending.
- [x] **Dual-Radio Fallback:** BLE L2CAP CoC channel (`UUID 0xFE2C`) uses Android's OS-assigned PSM, advertised to Windows and carried in the handshake reply; `0x1001` remains only a legacy discovery fallback. TCP pairing and PSK reconnect are verified. The current API 36 emulator reports BLE advertising with an OS-assigned PSM, but Windows host-side discovery and connection remain unverified; physical BLE testing is still open.
- [x] **Single active TCP session:** the Windows listener rejects overlapping peers while the shared session cipher is in use. Runtime verification rejected a second loopback socket while the paired Android connection continued heartbeating.

### Phase 2: Windows Native Integration (`zerolink-win`) (IMPLEMENTED; CATALOG AND ENCRYPTED MOCK-PEER HYDRATION VERIFIED; LIVE DEVICE ACTIONS PENDING)
- [x] **Lightweight Rust GUI (`ui.rs`):** Native Win32 window (`CreateWindowExW`), custom dark theme painting, high-DPI scaling, and no Electron/WebView overhead. The daemon uses two Tokio workers and requests one working-set trim five seconds after startup. The repository benchmark script's three isolated eight-second idle runs measured 14.25–14.26 MB working set and 3.11–3.35 MB private memory, meeting the `<15 MB` idle target. Connected-session memory remains unmeasured.
- [x] **WinRT Action Center Toasts (`notifications.rs`):** Native Windows toast notifications using `ToastNotificationManager` without requiring 3rd-party notification libraries.
- [x] **Win32 Message-Only Clipboard Listener (`clipboard.rs`):** `AddClipboardFormatListener` with `HWND_MESSAGE`; the listener waits for clipboard messages while idle.
- [x] **Windows Cloud Filter Virtual Drive (`cloud_filter.rs`):** Kernel-level `cldapi.dll` sync-root registration and native placeholder creation in `C:\Users\<User>\ZeroLink Photos`. FETCH_DATA validates the placeholder identity and range, retains Cloud Files request keys, and uses the API's optional contiguous range hint to fetch larger ranges (4 KiB aligned and capped at 16 MiB) before completing through `CfExecute`. Failed transfers preserve a valid positive range length as required by the API. A pairing-free local probe opened and read back a real 4 MiB placeholder in one range. The isolated encrypted Node peer then exercised the Android wire format through pairing, protected-secret persistence, and complete 4 MiB placeholder hydration/readback. The live Android catalog-token-to-Windows path and cancellation behavior remain unverified. See Microsoft's [FETCH_DATA range hints](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ns-cfapi-cf_callback_parameters) and [TRANSFER_DATA range rules](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ns-cfapi-cf_operation_parameters).
- [x] **Screen viewer (`screen_viewer.rs`, `hevc_decoder.rs`, `d3d_presenter.rs`):** D3D11/HEVC setup is deferred until the user opens the viewer. An asynchronous D3D11-aware Media Foundation hardware MFT is preferred when available; decoded NV12 surfaces go directly to a D3D11 video processor and swap chain. CPU NV12-to-BGRA conversion and aspect-fit GDI remain the fallback, along with a synchronous software MFT. Runtime codec availability and playback still need verification.

### Phase 3: Android System Integration (`zerolink-android`) (BUILD, API 36 SERVICE, ANDROID RANGE SERVER, AND ENCRYPTED MOCK-PEER HYDRATION VERIFIED; LIVE CATALOG DELIVERY, SCREEN FRAMES, AND PHYSICAL DEVICE VALIDATION PENDING)
- [x] **Foreground service (`ZeroLinkService.kt`):** Uses `connectedDevice` and `mediaProjection` service types. After the latest APK rebuild, the API 36 test emulator reinstalled and launched ZeroLink; the process remained alive and `dumpsys activity services` reported `ZeroLinkService` with `startForegroundCount=1` and `isForeground=true`. After Home moved the launcher to the resumed state, ZeroLink's process and foreground service remained alive. A fresh forced deep-idle run on this build reported `mForceIdle=true`, `mState=IDLE`, `mWakeLockSummary=0x0`, and `Wake Locks: size=0` while the service remained foreground. The emulator was returned to active idle (`mForceIdle=false`); physical-device Doze and battery measurements remain open.
- [x] **Telemetry & Battery Reporting:** Automatic 10-second heartbeat polling real `BatteryManager` levels and charging states.
- [x] **Notification Listener Service (`ZeroNotificationListener.kt`):** Captures incoming Android notifications and packages metadata for desktop toasts.
- [x] **H.265 Screen Streaming (`H265ScreenEncoder.kt`):** Uses Android `MediaProjection` and capability-aware `MediaCodec` selection, preferring surface input and falling back to flexible YUV. Codec discovery and startup run off the UI thread; encoded buffers carry timestamps, keyframe status, and frame dimensions. The emulator exposes no compatible size/rate configuration and emitted no frames, so live streaming remains open.
- [x] **MediaStore Photo Catalog:** The user-triggered catalog action lists the newest 25 MediaStore images. Image access is requested only on that action: Android 10–12 use `READ_EXTERNAL_STORAGE`, Android 13 uses `READ_MEDIA_IMAGES`, and Android 14+ also supports selected-photo access and reselection. Core pairing and sync permissions remain independent. API 36 instrumentation verifies the declared permissions and SDK-specific request set. Runtime permission UI and catalog delivery to Windows remain unverified.
- [x] **Encrypted photo-range server:** Android accepts bounded file-id/offset/length requests (up to 16 MiB per range) encrypted under per-connection AES-GCM keys derived from a one-time token and fresh server challenge. The token is delivered through the encrypted paired channel and is never sent over bulk TCP. API 36 instrumentation inserts a synthetic MediaStore image, requests an authenticated range crossing EOF through the actual socket server, verifies decrypted bytes and file size, confirms row cleanup, then flips a ciphertext bit on a second request and verifies rejection. Windows Cloud Files hydration through the encrypted Node peer using the Android wire format now passes; delivering a real Android catalog/token to Windows remains unverified.

### Phase 4: Zero-Root Background Clipboard Auto-Sync (IMPLEMENTED; FOREGROUND TEST SYNC VERIFIED; BACKGROUND DEVICE VALIDATION PENDING)
- [x] **`ZeroAccessibilityService` Implementation:** Completely non-root background clipboard reading using system accessibility context with `canRetrieveWindowContent="false"`.
- [x] **Ongoing Notification Quick Action:** Instant `[ 📋 Sync Clipboard ]` button in the persistent status bar notification.
- [x] **Android Quick Settings Tile:** `ZeroClipboardTileService` for single-tap quick settings pull-down sync.
- [x] **MainActivity Accessibility Deep-Link:** Status indicator and direct shortcut button into Android Accessibility settings.
- **Repository claim, not reproduced here:** clipboard updates were reported on Samsung SM-A075F; this review did not reproduce the device test.

---

## Phase 5: Actionable Notifications with Inline Quick Replies (IMPLEMENTED; SYNTHETIC DELIVERY AND ANDROID REPLY-CACHE VERIFIED; WINDOWS TOAST ACTIONS PENDING)

- **`PacketType::NotificationAction` (`0x0B`):** `NotificationActionPayload { notification_id, action_id, reply_text }`.
- **Windows toast retention:** `WindowsToastManager` keeps up to 32 live `ToastNotification` objects plus their `Activated` handlers, so WinRT can deliver activation events (the previous fire-and-forget code dropped the toast immediately).
- **Inline reply capture:** the activation handler parses `action=reply|dismiss&id=<key>`, reads `quickReply` from `ToastNotificationActivatedEventArgs.UserInput`, and forwards the payload over the fabric.
- **Android reply cache:** `ZeroNotificationListener` caches only free-form `RemoteInput` entries and the originating `PendingIntent` in a bounded 32-entry LRU (`NotificationActionCache`); `ZeroLinkService` delivers replies with `RemoteInput.addResultsToIntent()`. Notification keys and reply identifiers are omitted from Android action logs.
- **Dismissal:** the Windows Dismiss action invokes `NotificationListenerService.cancelNotification(key)` on Android.
- **Verification:** the API 36 emulator sent a synthetic notification packet to Windows, and the toast call returned without error. Instrumentation now sends a synthetic `RemoteInput` reply through the cached originating `PendingIntent` and verifies canceled intents are removed. Protocol tests and the Windows toast-argument parser test pass. Actual Windows toast activation and cross-device reply/dismiss behavior remain unverified.
- **Limitation:** activation is handled in-process while the resident daemon is running. True background activation (daemon stopped) additionally requires a COM `INotificationActivationCallback` registered with an AUMID Start Menu shortcut.

## Phase 6: Windows Media Playback Control & Volume Sync (IMPLEMENTED; ANDROID/DEVICE VALIDATION PENDING)

- **Windows GSMTC bridge (`media.rs`):** `GlobalSystemMediaTransportControlsSessionManager` supplies the active session; `query_state()` reads title, artist, album, source app, playback status, and timeline position/duration.
- **Commands (`0x0C`):** Play, Pause, Toggle, Next, Previous, Volume Up/Down. Volume uses multimedia `keybd_event` because GSMTC exposes no volume API.
- **State push (`0x0E`):** `MediaStatePayload` is sent on state change. Queries are piggybacked on the existing 10-second heartbeat and refreshed immediately after every command — no dedicated polling timer, preserving the 0.0% Windows idle CPU target.
- **Android UI:** MainActivity media card (title, artist, album, source app, prev/play-pause/next) and matching actions in the ongoing notification via `MediaControlReceiver`.
- **Verification:** Windows `cargo check`/`cargo test` and Android debug assembly pass. On-device media testing is pending.

## Phase 7: Rootless Gesture Injection for Screen Mirroring (IMPLEMENTED IN SOURCE; RUNTIME VALIDATION PENDING)

- **Protocol (`0x0D`):** `ScreenInputPayload { action: Down|Move|Up|Scroll, x, y, scroll_delta }` with normalized/clamped coordinates.
- **Android injection:** `ZeroAccessibilityService` implements `GestureDescription` + `dispatchGesture()` — `Down` starts a `Path`, `Move` extends it, `Up` dispatches the stroke, and `Scroll` dispatches a vertical swipe scaled from the wheel delta.
- **Privacy surface:** the accessibility config now enables `android:canPerformGestures="true"` while keeping `android:canRetrieveWindowContent="false"`.
- **Windows viewer:** an asynchronous D3D11-aware Media Foundation hardware MFT is preferred. When the D3D11 video processor is available, decoder-owned NV12 surfaces are presented directly through a D3D11 swap chain with aspect-fit scaling. Unsupported surfaces and unavailable presentation hardware fall back to CPU readback, NV12-to-BGRA conversion, and GDI; a synchronous software MFT remains the decoder fallback. The input window forwards click, drag, and wheel events as normalized `ScreenInput` packets. Frame dimensions set the view aspect ratio; the decoder restarts if the header dimensions change.
- **Runtime limits:** the hardware MFT and direct GPU presentation compile but have not been exercised on a Windows device. Android 14+ uses MediaProjection captured-content resize callbacks; Android 10–13 use the default-display listener. Both resize the existing virtual display and restart the encoder. Actual playback, rotation, and gesture delivery need Windows/Android device testing.

---

## Remaining Work & Known Gaps

### Sefirah-informed product decisions

Reference projects: [Sefirah for Windows](https://github.com/shrimqy/Sefirah) and [Sefirah for Android](https://github.com/shrimqy/Sefirah-Android). Use these as product and architecture references; do not copy code without a deliberate GPL-3.0 compatibility decision.

### What ZeroLink should learn from Sefirah

- **Explain permissions during onboarding.** Sefirah documents Android restricted settings and optional Shizuku/ADB routes for clipboard monitoring. ZeroLink should keep its non-root clipboard path easy to try, say when Android blocks background reads, and surface the existing notification and Quick Settings actions as fallbacks.
- **Make local-only data handling visible.** Sefirah's privacy note describes encrypted local-network transport and on-device storage. ZeroLink should plainly list what crosses the paired link, what stays local, and which permission each feature uses.
- **Treat Explorer storage as experimental until hydration is proven.** Sefirah's README calls its Android storage integration experimental and warns about destination selection. ZeroLink's local Cloud Files probe and encrypted Android-format mock peer both hydrated and read back a 4 MiB placeholder, with isolated secret/root cleanup. Cancellation and the live Android catalog/token path still need verification.
- **Keep the mirroring dependency trade-off explicit.** Sefirah uses scrcpy as an external screen-mirroring dependency. ZeroLink keeps its native HEVC path to avoid a second sender/bootstrap path; that choice still needs hardware, playback, rotation, and input validation.
- **Use Sefirah as a product reference, not a code dependency.** The desktop and Android repositories are GPL-3.0. No Sefirah code has been copied into ZeroLink.

Sources: [Sefirah desktop README](https://github.com/shrimqy/Sefirah), [Sefirah Android README](https://github.com/shrimqy/Sefirah-Android/blob/master/README.MD), [Sefirah Android privacy note](https://github.com/shrimqy/Sefirah-Android/blob/master/.github/Privacy.md), and [v3.1.0 desktop release notes](https://github.com/shrimqy/Sefirah/releases).

- [x] **Pairing UX:** v3 paired-secret handshake proofs, dual approval/code comparison, protected secret storage, data gating, background Android approval notification, and forget-device controls are implemented. Initial pairing and restart/reconnect were verified on the API 36 emulator. Android TCP and BLE now share a constant-time pairing-proof verifier; API 36 instrumentation covers wrong secrets, labels, transcripts, malformed tags, the unpaired zero tag, Keystore save/reload/clear, lost-key fail-closed/Forget recovery, timeout rejection, and the production approval dialog's Reject-button response. Visible code mismatch, cross-device forget, one-sided-state recovery, and physical-device behavior remain pending. See [PAIRING-DESIGN.md](PAIRING-DESIGN.md).
- [x] Add a shared six-digit pairing-code helper with a deterministic Rust test and JNI entry point.
- [x] **Clipboard backend decision:** retain the in-app, notification-action, Quick Settings, and Accessibility paths for now. Defer Shizuku/ADB integration until Android-version testing shows a gap that warrants its additional activation and support steps.
- [x] **Screen mirroring decision:** continue the native path. Android already emits HEVC through ZeroLink's protocol; scrcpy would add a second sender/bootstrap path and external tool dependency. D3D11 hardware decode and direct GPU presentation now have CPU/GDI and synchronous software fallbacks; device validation remains open.
- [x] **Onboarding and limitations:** documented first pairing, permissions, TCP port/firewall, Android clipboard limits, build requirements, and Windows HEVC decoder limits in [GETTING-STARTED.md](GETTING-STARTED.md).
- [x] **Screen-capture lifecycle:** promote the `mediaProjection` foreground-service type before retrieving the projection token; the API 36 consent and service-promotion path now completes without the earlier foreground-service failure.
- [ ] **Screen-mirror runtime:** capture a frame stream on a supported Android device, verify HEVC decode and playback in the Windows viewer, then check rotation and remote gestures. On the API 36 x86_64 emulator, ZeroLink requested 1080x2209 at up to 30 fps; Android reported no compatible HEVC encoder, and ZeroLink stopped the MediaProjection session before producing frames.

### Handshake and verification follow-up
- [x] Add a transcript-bound HMAC key-confirmation tag to Android replies and verify it on Windows before activating the session.
- [x] Derive separate initiator/responder encryption keys and nonce bases, add mutual transcript confirmation, and version the paired handshake as v3.
- [x] Replace the placeholder simulator flow with a peer that receives Windows HandshakeInit, derives matching keys, verifies the reciprocal key confirmation, sends the first-pair approval signal, and submits encrypted notification, clipboard, heartbeat, and catalog packets.
- [x] Add Android verification of the Windows key-confirmation frame and reject trailing bytes in handshake payloads. Rust workspace tests cover the Windows sender; Android debug compilation and assembly now pass.
- [x] Define and implement device pairing/authentication; Android build and device verification remain open.
- [x] Run the Node.js built-in-crypto simulator against an isolated Windows daemon. The earlier run verified mutual transcript confirmation and synthetic notification, clipboard, heartbeat/media-state, and catalog packet handling. Its test pairing key was removed afterward; that run predates the current code-entry gate.
- [x] Complete the isolated simulator code-match and mutual-key-confirmation flow. The latest pairing-only script exited successfully after the Node simulator accepted the matching code, verified Windows' reciprocal transcript tag, and sent the encrypted first-pair approval marker.
- [x] Capture the isolated verifier's pairing-secret persistence audit. On 2026-11-20, the displayed Windows and simulator codes matched (`966945`), mutual transcript confirmation succeeded, and the simulator sent encrypted first-pair approval plus synthetic notification, heartbeat, and catalog packets. Before cleanup, Windows had saved a 320-byte protected `paired-peer.dat`; `pairing-audit.txt` records only `PairingSecretPersisted=true` and `ProtectedSecretBytes=320`. The secret was absent after cleanup and the isolated daemon exited. This verifies Windows DPAPI persistence in the test profile; Android Keystore save/reload/clear is now verified by an isolated emulator instrumentation test.
- [x] `scripts/verify_zerolink_win.ps1` uses Node.js, a rebuilt release binary, dynamic loopback/bulk ports, and an isolated profile. The interactive simulator and daemon limits are both 3,600 seconds for action-time approval. Cleanup requests normal daemon shutdown before unregistering its temporary sync root, with a bounded forced stop as fallback. Both simulator implementations require a matching code before sending Android first-pair approval. Synthetic clipboard injection is opt-in so routine simulator runs do not replace the host clipboard.

### Windows HEVC viewer
- [x] Implement a Media Foundation HEVC decode path that prefers asynchronous D3D11-aware hardware MFTs and falls back to synchronous software MFTs. Present supported hardware decoder NV12 surfaces directly through a D3D11 video processor and swap chain; keep CPU/GDI fallback for unsupported surfaces or unavailable GPU presentation.
- [x] Create the screen input window, map `WM_LBUTTONDOWN`, `WM_LBUTTONUP`, `WM_MOUSEMOVE`, and `WM_MOUSEWHEEL` to normalized coordinates, and dispatch `PacketType::ScreenInput`.
- [x] Carry encoder dimensions in video headers and map viewer input to the remote aspect ratio.
- [x] Restart the Windows decoder if incoming frame dimensions change.
- [x] On Android 14+, resize the existing virtual display and recreate the encoder when MediaProjection reports a captured-content size change.
- [x] Add a default-display resize fallback for Android 10–13.
- [x] Add asynchronous D3D11-aware Media Foundation hardware-MFT selection and direct D3D11 NV12 presentation, retaining CPU/GDI and synchronous software fallbacks.
- [ ] Verify hardware decode, live playback, orientation changes, and remote gestures on Windows and Android devices. The last successful device check found no physical Android phone. On 2026-11-20, a reviewed `adb devices -l` invocation listed only emulator-5554 (API 36 x86_64), with no physical phone. The full-screen MediaProjection attempt requested 1080x2209 at 30 fps; Android exposed no compatible HEVC encoder, so no frames were produced. Windows host-side BLE discovery/connection and supported-device playback, rotation, and gestures remain unverified.
- [x] Verify encrypted peer byte-range hydration. The Node peer's encrypted bulk server opened a 4 MiB Cloud Files placeholder; Windows fetched the full range through `FETCH_DATA`/`CfExecute` and the verifier compared all 4,194,304 bytes against the synthetic media. The retained audit is `.codex-target\verification-20261120-164732\logs\hydration-audit.txt`; its trace records one successful 4 MiB transfer without secrets. The same run observed a 320-byte protected pairing secret before cleanup removed it and unregistered the isolated sync root. Android's bulk server independently passes an API 36 synthetic MediaStore range test through EOF and rejects modified request ciphertext. Still needed: exercise cancellation and verify the actual Android catalog path over a paired device. Physical Android pairing remains unavailable in this environment.

### Verification
- On-device test matrix on the SM-A075F: inline reply into WhatsApp/SMS, dismiss, media card + notification controls against Spotify/browser, gesture injection through the accessibility service.
- Latest Android `:app:compileDebugKotlin` and `:app:assembleDebug` pass with Gradle 9.2.0, AGP 8.5.0, Android SDK 34, and JDK 23. The debug APK is produced; device-level behavior remains unverified.
- Historical workspace verification checkpoint (2026-11-06): `cargo test --workspace` passed 14 core and 10 Windows tests; `cargo build --release -p zerolink-win` succeeded; `cargo clippy --workspace --all-targets` passed without code warnings. The new loopback test exercises the encrypted range client against the Android wire format and an EOF-ending range, but real MediaStore/Cloud Files hydration remains unverified. The Android Kotlin compile and debug APK assembly passed after the encrypted bulk-transfer changes. The updated pairing verifier's PowerShell parser check passed. Formatting differences remained at that checkpoint.
- Current workspace verification (2026-11-20): `cargo fmt --all -- --check` passes; the latest `cargo test --workspace` passes 14 core and 20 Windows tests, with one Cloud Files integration probe ignored by default and run explicitly below; the Windows suite includes isolated DPAPI storage tests. `cargo clippy --workspace --all-targets -- -D warnings` passes, and the latest release build succeeds. Android `:app:compileDebugKotlin` and `:app:assembleDebug` pass offline with JDK 23 after setting `JAVA_HOME` to the installed JDK directly; Kotlin reports two existing WifiInfo deprecation warnings, and Gradle reports deprecated features scheduled for removal in Gradle 10. Node simulator syntax, PowerShell verifier parsing, and `git diff --check` pass. Cargo emits filesystem incremental-cache hard-link fallback notices.
- Windows pairing-proof and Cloud Files regression verification (2026-11-20): tests prove a matching persisted secret completes the v3 handshake, a mismatched proof is rejected before session activation, optional Cloud Files range hints are safely used, and failed transfers retain a valid range length. The latest full workspace run passed 20 Windows tests with one Cloud Files integration probe ignored; the local Cloud Files probe passed separately and hydrated a 4 MiB placeholder in one range. The default sandbox initially denied loopback and temporary-directory operations; the reviewed full-suite rerun passed.
- Resumed Windows verifier (2026-11-20): the reviewed workspace run passed 14 core and 20 Windows tests, and the release binary rebuilt successfully under `.codex-target\verification-build` after Windows denied replacing the default target executable. The 3,600-second isolated run then completed after the user accepted the matching live code. It persisted a 320-byte protected pairing secret, hydrated and byte-verified the complete 4 MiB placeholder, removed the secret, and unregistered its test sync root. Audits and the secret-free transfer trace are retained under `.codex-target\verification-20261120-164732\logs`.
- Android Keystore probe (2026-11-20): `:app:assembleDebugAndroidTest --offline` succeeded. The targeted `PairingSecretStorePersistenceTest` passed on emulator-5554 (API 36 x86_64): it saved a synthetic 32-byte secret under a unique preference name and Keystore alias, reloaded it using a new store instance, then cleared both. The same APK also passed through direct `am instrument` invocation with Android's platform runner. The initial offline `connectedDebugAndroidTest` attempt could not resolve an uncached host-side UTP plugin; a subsequent normal `:app:connectedDebugAndroidTest` run completed successfully. Its generated XML report records 1 test, 0 failures, and 0 errors.
- Android photo permission verification (2026-11-20): the app now declares the legacy and granular image permissions and requests them on demand from Sync Photos, with Android 14 selected-photo and reselection handling. The runtime request set is separated from required service permissions; denying photo access does not block pairing or clipboard features. `:app:connectedDebugAndroidTest --offline` passed both the permission-manifest/request-policy test and Keystore persistence test on emulator-5554 (API 36 x86_64); XML report: 2 tests, 0 failures, 0 errors.
- Android pairing, permission, reply, range, and clipboard verification (2026-11-20): `:app:connectedDebugAndroidTest --offline` passed 10 tests on emulator-5554 (API 36 x86_64): photo permission policy; Keystore save/reload/clear; lost-wrapping-key fail-closed/Forget recovery; pairing proof validation for secret, role, transcript, tag length, and unpaired state; timed-out approval returning false; the production approval dialog's Reject-button response; synthetic notification `RemoteInput` delivery through the cached originating `PendingIntent`; canceled reply-action cleanup; encrypted range serving from a synthetic MediaStore row through EOF with verified cleanup and rejection of modified AES-GCM request ciphertext; and ignoring remote clipboard payloads whose format is not plain text. The TCP and BLE handshake paths now call the same constant-time proof verifier. The XML report records 10 tests, 0 failures, and 0 errors. This does not verify Windows toast activation, a live paired notification path, physical device behavior, or visible code mismatch between devices.
- Local Cloud Files API probe (2026-11-20): after adding optional-range support and valid failure lengths, `cargo test -p zerolink-win --bin zerolink-win cloud_files_callback_hydrates_a_local_placeholder -- --ignored --nocapture` passed again. The Windows callback transferred the local 4 MiB placeholder in one range through `CfExecute`, and the reader verified all 4,194,304 bytes; the test unregistered and removed its unique sync root. Five new range/error-path unit tests also pass, covering broad hints, `CF_EOF`, bounds, alignment, invalid hints, required-range preservation, and positive failure lengths. This validates the local callback path but does not substitute for the encrypted peer test.
- Isolated pairing verifier (2026-11-20): `scripts/verify_zerolink_win.ps1` completed successfully on loopback port `10913`. The pairing audit captured a 320-byte protected secret before cleanup; the secret was absent afterward, the daemon exited, and cleanup emitted no sync-root unregister warning. Logs and the non-secret audit remain under `.codex-target\verification-20261120-102323\logs`.
- Earlier isolated hydration attempts (2026-11-20): one matching-code prompt expired before approval, and another completed pairing but failed to hydrate after a 64 KiB transfer. These runs created no retained pairing secret; their temporary roots were cleaned up. The successful resumed verifier and its audit are recorded above.
- `scripts/measure_zerolink_memory.ps1 -Iterations 3 -MeasureAfterSeconds 8` passed with graceful daemon shutdown and sync-root cleanup. Its CSV is retained at `.codex-target\memory-benchmark-20261120-092149\measurements.csv`; all three no-peer runs met the idle working-set target.
- Android API 36 runtime (2026-11-20): a fresh isolated 2 GB AVD booted from the installed Google Play system image. The current debug APK installed, `MainActivity` remained alive, and `ZeroLinkService` entered foreground idle without a crash. The service UI reports BLE advertising, but Windows host-side BLE discovery has not been verified.
- Latest Android runtime recheck (2026-11-20): after the 16 MiB range-limit change compiled and assembled, the APK installed on the booted API 36 emulator. `org.zerolink` remained alive, and `dumpsys activity services org.zerolink` reported `ZeroLinkService` as foreground (`startForegroundCount=1`, `isForeground=true`). After sending Home, the emulator's resumed activity was the Pixel Launcher and the ZeroLink process/service remained alive. This validates startup and foreground-service survival on the emulator; BLE-to-Windows and real MediaStore hydration remain open.
- Latest Android Doze recheck (2026-11-20): forced the API 36 emulator into deep idle with the latest APK running in the background. `dumpsys deviceidle` reported `mForceIdle=true`, `mState=IDLE`; `dumpsys power` reported `mWakeLockSummary=0x0` and `Wake Locks: size=0`; `ZeroLinkService` remained foreground. `dumpsys deviceidle unforce` restored active idle and a follow-up read showed `mForceIdle=false`. Physical-device battery impact remains unmeasured.
- Android Doze snapshot (2026-11-20): forced the emulator into deep idle; `dumpsys deviceidle` reported `mState=IDLE`, and `dumpsys power` reported `mWakeLockSummary=0x0` and `Wake Locks: size=0`. The emulator was returned to normal idle afterward. Physical-device battery impact remains unmeasured.
- Pairing verification attempt (2026-11-19): with loopback access enabled, `cargo test --workspace` passed 14 core and 10 Windows tests, and the release Windows binary built successfully. The live isolated pairing prompt showed code `556222`, but action-time approval was not received. The simulator was given a deliberate mismatch and exited with “Pairing code did not match; no Android approval was sent.” The verifier stopped before the persistence check. Its isolated AppData directory contained no pairing secret after cleanup; no `pairing-audit.txt` was written. This is an aborted verification, not a pairing pass.
- Current device discovery: ADB lists the isolated API 36 emulator as online and `sys.boot_completed=1`. No physical Android device is connected. The original 6 GB test AVD could not fit on the project drive; a fresh 2 GB test profile boots from the installed image in an authorized temporary workspace.
- Reproduce Doze behavior on a physical Android device and measure Windows memory with an active peer and during transfers; the isolated no-peer idle run now meets the `<15 MB` working-set target.
- Exercise forget, notification reply/dismiss, and media commands on devices. The API 36 emulator has verified TCP pairing/reconnect, encrypted heartbeats, synthetic clipboard injection, notification packet delivery, and three catalog placeholders; physical BLE and screen-mirroring checks remain open.

### Build notes
- Android is configured for AGP 8.5.0 / Kotlin 1.9.24. Ten debug instrumentation tests pass on the API 36 emulator; `:app:assembleRelease` passes with release lint enabled after the Android diagnostic log-redaction and clipboard-format changes. The release output is `app/build/outputs/apk/release/app-release-unsigned.apk` because signing is not configured. The merged release manifest excludes the debug-only pairing-dialog test host and notification reply capture receiver. The existing wrapper files were preserved.
- Current Rust verification is recorded above: 14 core and 20 Windows tests pass, with one Cloud Files OS integration probe ignored by default and passed separately; the Windows release build succeeds, and workspace Clippy has no code warnings. Cargo emits filesystem incremental-cache hard-link fallback notices.

---

## Benchmarks & Verification Summary

| Metric | Target Constraint | Value / verification status |
| :--- | :--- | :--- |
| **Windows Daemon Working Set RAM** | `< 15.0 MB` | **Measured here:** 14.25–14.26 MB working set and 3.11–3.35 MB private across three isolated 8-second no-peer snapshots after the delayed working-set trim. Connected-session measurement remains open. |
| **Android Standby CPU Wake Locks** | `0` | **API 36 emulator snapshot:** forced deep idle reported `mWakeLockSummary=0x0` and `Wake Locks: size=0`. Physical-device battery impact remains unmeasured. |
| **Handshake & Session Setup** | `< 100 ms` | Repository note: **< 35 ms** for Curve25519 ECDH; not remeasured in this verification pass. |
| **Clipboard Sync Latency** | `< 100 ms` | Repository note: **~22 ms** for the local bridge / BLE L2CAP; not remeasured in this verification pass. |
| **Windows Media Query Overhead** | `0.0%` idle CPU | Queries are on demand and heartbeat-piggybacked; idle CPU impact was not benchmarked in this pass. |
| **Media Control Payload** | `< 16 bytes` | Protocol command is 1 byte; the packet also includes the 20-byte frame header. |
| **Quick Reply Payload** | `< 1 KB` typical | Length-prefixed notification ID, action, and text; payload size depends on text. |
| **Root Required** | **NO** | Designed for non-root Android; physical-device compatibility remains to be verified. |

> New-feature latency (inline reply round-trip, media command round-trip, gesture injection) still requires on-device measurement; the numbers above are protocol/overhead properties, not new benchmarks.
