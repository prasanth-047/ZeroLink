# ZeroLink setup and current limits

## What ZeroLink is

ZeroLink is an Android and Windows companion project for clipboard sync, Android notifications and replies, media controls, photo catalog access, and screen mirroring. Rust checks and Android debug assembly pass. On an API 36 emulator, TCP pairing/reconnect, heartbeats, synthetic clipboard injection, synthetic notification delivery, and three synthetic photo placeholders were verified. The Android screen-capture consent and foreground-service promotion now work, but the emulator exposes no compatible HEVC surface or flexible-YUV configuration for the requested `1080x2209` capture or the lower sizes checked, so no video frames were emitted. Physical BLE and screen mirroring, notification replies, media controls, background clipboard behavior, and Doze remain unverified. The Windows daemon now requests a working-set trim five seconds after startup; three isolated no-peer idle snapshots from the repository benchmark measured 14.25–14.26 MB working set and 3.11–3.35 MB private memory. Connected-session memory and Android Doze behavior still need measurement.

## First pairing

1. Start the Windows daemon and ZeroLink Android service.
2. Keep both devices nearby. If the Android app is in the background, tap its ZeroLink pairing notification to open the approval dialog.
3. Compare the six-digit code shown on Android with the Windows approval prompt. Approve on both devices only when the codes match.
4. The pairing secret is stored with Android Keystore protection on Android and current-user DPAPI on Windows. Reconnects authenticate with that secret and do not show a new approval prompt.
5. Use **Forget Paired Device** on both sides before pairing again when the devices are offline. When connected, either side can request removal from the other.

If the devices lose connection during their first pairing, they may not both save the secret. A mismatched saved state fails closed; forget the pairing on both sides and start again.

## Permissions and connectivity

- Android 10 or later is required for the L2CAP CoC path used by this project. Grant the Bluetooth permissions requested by Android and keep Bluetooth enabled.
- Approve the Android notification permission so background pairing requests can open the app. Pairing still requires a person to review and approve the code.
- Photo catalog and file access request image permission only when you tap **Sync Photos Catalog to PC**. Android 10–12 use `READ_EXTERNAL_STORAGE`; Android 13+ use `READ_MEDIA_IMAGES`. Android 14+ lets you share selected images and reselect them later. Denying photo access does not disable pairing, clipboard, or notification features.
- Notification forwarding requires Android Notification Access. Accessibility-based clipboard and remote gesture features require the corresponding Android Accessibility service to be enabled. These permissions are optional to the rest of the app.
- The TCP bridge listens on port `8989`. For a LAN connection, enter the Windows PC's reachable address in Android and allow the Windows firewall to accept the connection on the selected network profile.
- Clipboard reads are subject to Android's background clipboard restrictions and the selected accessibility/manual action. Universal automatic clipboard capture is not guaranteed across Android versions or OEM policies.

## Build and verification

- Rebuild the Rust JNI libraries for both Android ABIs before packaging source changes: `powershell -ExecutionPolicy Bypass -File .\scripts\build_android_jni.ps1`. This requires the Android NDK and Rust targets `aarch64-linux-android` and `x86_64-linux-android`.
- Rust workspace: `cargo test --workspace`.
- Android: after rebuilding JNI, `:app:compileDebugKotlin` and `:app:assembleDebug` pass with AGP 8.5.0, Kotlin 1.9.24, Gradle 9.2.0, and Android SDK 34. The debug APK is written to `app/build/outputs/apk/debug/app-debug.apk`.
- Screen mirroring: Android prefers a MediaCodec HEVC surface encoder and can use flexible-YUV input when the device advertises it. Codec discovery runs off the UI thread. The API 36 emulator did not advertise a size/rate combination compatible with this capture path, so emitted frames and Windows playback remain unverified. Windows prefers an asynchronous D3D11-aware Media Foundation HEVC hardware decoder. Supported NV12 decoder surfaces are presented directly through a D3D11 video processor and swap chain; CPU readback/GDI and a synchronous software decoder are fallbacks. Android 14+ uses MediaProjection resize callbacks, while Android 10–13 use a default-display listener to resize capture during rotation.
- For a local-only TCP test, set `ZEROLINK_BIND_ADDR=127.0.0.1`; the default listener binds to all IPv4 interfaces. `ZEROLINK_PORT` still selects the port (default `8989`).

The performance figures in the architecture and execution-plan documents are project targets or prior repository claims; they have not been independently measured in this review. Sefirah is used as a product reference only; its GPL-3.0 source has not been copied into ZeroLink.
