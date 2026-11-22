# ZeroLink Device Pairing

**Implementation status:** v3 pairing flow is implemented in the shared Rust core, Windows client, and both Android transports. An API 36 TCP run completed first pairing and Windows-daemon restart/reconnect, demonstrating that the saved secret could be reloaded and used. The isolated Windows verifier also completed matched-code pairing and confirmed a 320-byte DPAPI-protected secret before deleting it; its non-secret audit remains under `.codex-target\verification-20261120-102323\logs`. API 36 instrumentation now verifies Android Keystore save/reload/clear and fail-closed behavior when the wrapping key is lost. Cross-device edge cases and physical BLE remain open.

## How trust works

ZeroLink derives a shared 32-byte pairing secret from the first X25519 session. Both devices display the same six-digit code derived from the ordered `client_pubkey || salt || server_pubkey` transcript. Windows uses a native approval prompt; Android shows a dialog in the foreground or a notification that opens the app to the dialog. Both users must approve and compare the codes.

After Windows verifies Android's reciprocal key-confirmation frame, Android stores the secret under an Android Keystore AES-GCM wrapping key and sends an encrypted pairing acknowledgement (`Signaling` payload `50 01`). Windows stores the same secret with current-user DPAPI only after receiving that acknowledgement. Feature packets remain gated until Windows has stored the acknowledgement. Subsequent sessions send role-labeled HMAC proofs in both handshake messages. A missing or incorrect proof fails closed; there is no unpaired fallback.

The persistent secret acts as a shared, trust-on-first-use device credential. ZeroLink does not currently maintain a separate signing identity key or support multiple paired peers per app installation. BLE and the TCP bridge share the same saved pairing record.

## Forgetting a device

When connected, Android and Windows can request pairing removal over an encrypted `Signaling` frame (`50 02`); the other side removes its secret and returns `50 03`. When offline, each side must be forgotten locally on its own device before pairing again. The Android and Windows interfaces explain this constraint.

## Protocol and security limits

- The code is a human comparison string for the current handshake, not a permanent identity or a manufacturer credential.
- Users must compare both codes before approving. Approving mismatched codes can accept a man-in-the-middle.
- v3 is wire-incompatible with older handshakes. Both apps must use v3; downgrade is not attempted.
- A transport interruption during initial secret persistence can leave one device with saved state. The mismatch is rejected on reconnect; use Forget on both devices and pair again.
- A lost forget acknowledgement can similarly leave one saved secret. Reconnect and retry the online forget flow; if the devices are offline, forget on both sides before pairing again.
- Android foreground notification permission and an unlocked user session may be needed to reach the approval dialog when the app is backgrounded.

## Verification still required

The API 36 run covers matched-code first pairing and same-secret TCP reconnect. The isolated verifier's `pairing-audit.txt` assertion passed: Windows persisted a 320-byte protected secret before cleanup, and the secret was absent afterward. Android instrumentation verifies Keystore save/reload/clear, fail-closed behavior after losing the wrapping key, the shared TCP/BLE proof gate against wrong secrets, role labels, transcript changes, malformed tag lengths, and unpaired state, timeout rejection, and the production dialog's Reject action. Still to exercise: visibly mismatched codes, cross-device one-sided stored state, forget while connected/offline, and BLE L2CAP and TCP on physical devices.
