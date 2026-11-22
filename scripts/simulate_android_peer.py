"""
ZeroLink Android Peer Simulator
Emulates an Android device running ZeroLinkService connecting to the ZeroLink Windows Daemon.
Tests ECDH Handshake, AES-256-GCM Session Encryption, Notifications, Clipboard, File Catalog, and Heartbeat.
"""

import socket
import struct
import time
import zlib
import hmac
import hashlib
import os
import sys
from cryptography.hazmat.primitives.asymmetric import x25519
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

MAGIC = 0x5A4C
VERSION = 1
HEADER_SIZE = 20

# Packet Types
PKT_HANDSHAKE = 1
PKT_SIGNALING = 2
PKT_NOTIFICATION = 3
PKT_CLIPBOARD = 4
PKT_FILE_METADATA = 5
PKT_FILE_CHUNK = 6
PKT_WIFI_WAKE = 7
PKT_SCREEN_CAST = 8
PKT_HEARTBEAT = 9


def compute_crc(magic, version, pkt_type, payload_len, seq):
    buf = struct.pack(">HBBIQ", magic, version, pkt_type, payload_len, seq)
    # Header CRC32 is computed over the first 16 bytes of header (magic 2B + ver 1B + type 1B + len 4B + seq 8B)
    return zlib.crc32(buf[:16]) & 0xFFFFFFFF


def build_frame(pkt_type, seq, payload):
    crc = compute_crc(MAGIC, VERSION, pkt_type, len(payload), seq)
    header = struct.pack(">HBBIQI", MAGIC, VERSION, pkt_type, len(payload), seq, crc)
    return header + payload


def parse_frame(data):
    if len(data) < HEADER_SIZE:
        return None, data
    magic, version, pkt_type, payload_len, seq, crc = struct.unpack(">HBBIQI", data[:HEADER_SIZE])
    if magic != MAGIC:
        raise ValueError(f"Invalid magic: {hex(magic)}")
    if version != VERSION:
        raise ValueError(f"Unsupported protocol version: {version}")
    expected_crc = compute_crc(magic, version, pkt_type, payload_len, seq)
    if crc != expected_crc:
        raise ValueError(f"Invalid header CRC: expected {expected_crc:#x}, got {crc:#x}")
    total_len = HEADER_SIZE + payload_len
    if len(data) < total_len:
        return None, data
    payload = data[HEADER_SIZE:total_len]
    return (pkt_type, seq, payload), data[total_len:]


def derive_nonce(iv_base, seq):
    seq_bytes = seq.to_bytes(8, byteorder="little")
    nonce = bytearray(iv_base)
    for i in range(8):
        nonce[i] ^= seq_bytes[i]
    return bytes(nonce)


def derive_pairing_code(client_public_key, salt, server_public_key):
    digest = hashlib.sha256(
        b"ZeroLink-v3-pairing-code" + client_public_key + salt + server_public_key
    ).digest()
    return f"{int.from_bytes(digest[:4], byteorder='big') % 1_000_000:06d}"


def confirm_pairing_code(expected_code):
    entered_code = os.environ.get("ZEROLINK_SIMULATOR_APPROVED_CODE")
    if entered_code is None:
        print(f"[Pairing] Simulator code: {expected_code}", flush=True)
        if not sys.stdin.isatty():
            raise RuntimeError(
                "Pairing is gated. Compare the code in Windows, then rerun with "
                "ZEROLINK_SIMULATOR_APPROVED_CODE set to that exact code."
            )
        entered_code = input(
            "Compare the six-digit Windows code, then enter it here to approve this simulator pairing: "
        )
    if entered_code.strip() != expected_code:
        raise ValueError("Pairing code did not match; no Android approval was sent")
    print("[Pairing] Displayed code matched; simulator approval is authorized.", flush=True)


def serialize_notification(msg_id, pkg, app, title, content, is_ongoing=False):
    out = bytearray()
    ts = int(time.time() * 1000)
    out.extend(struct.pack(">QB", ts, 1 if is_ongoing else 0))
    for s in [msg_id, pkg, app, title, content]:
        b = s.encode("utf-8")
        out.extend(struct.pack(">H", len(b)))
        out.extend(b)
    return bytes(out)


def serialize_clipboard(text):
    out = bytearray()
    out.append(0)  # PlainText
    h = zlib.crc32(text.encode("utf-8")) & 0xFFFFFFFF
    ts = int(time.time() * 1000)
    b = text.encode("utf-8")
    out.extend(struct.pack(">QQI", h, ts, len(b)))
    out.extend(b)
    return bytes(out)


def serialize_heartbeat(battery=88, charging=True, wifi=True, port=8443):
    return struct.pack(">BBBHB", battery, 1 if charging else 0, 1 if wifi else 0, port, 0)


def serialize_file_catalog(sync_root, files):
    out = bytearray()
    root_b = sync_root.encode("utf-8")
    out.extend(struct.pack(">H", len(root_b)))
    out.extend(root_b)
    out.extend(struct.pack(">I", len(files)))

    for f in files:
        item = bytearray()
        item.extend(struct.pack(">QQB", f["size"], f["mtime"], 1 if f.get("is_dir") else 0))
        for key in ["id", "path", "mime"]:
            sb = f[key].encode("utf-8")
            item.extend(struct.pack(">H", len(sb)))
            item.extend(sb)
        out.extend(struct.pack(">I", len(item)))
        out.extend(item)
    return bytes(out)


def run_simulation(host="127.0.0.1", port=8989):
    print("=" * 65)
    print("   ZeroLink Android Peer Simulator")
    print("=" * 65)
    print(f"Connecting to Windows Daemon on {host}:{port}...")

    timeout_seconds = float(os.environ.get("ZEROLINK_SIMULATOR_TIMEOUT_SECONDS", "180"))
    if timeout_seconds <= 0:
        raise ValueError("ZEROLINK_SIMULATOR_TIMEOUT_SECONDS must be greater than zero")

    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(timeout_seconds)
    s.connect((host, port))
    # Pairing requires a human to compare the code and approve on Windows.
    print(f" Using a {timeout_seconds:g}-second socket timeout.")
    print(" Connected to ZeroLink Windows transport socket!")

    def recv_exact(count):
        data = bytearray()
        while len(data) < count:
            chunk = s.recv(count - len(data))
            if not chunk:
                raise ConnectionError("Windows daemon closed the connection")
            data.extend(chunk)
        return bytes(data)

    def recv_frame():
        header = recv_exact(HEADER_SIZE)
        _magic, _version, _packet_type, payload_len, _sequence, _crc = struct.unpack(">HBBIQI", header)
        parsed, remainder = parse_frame(header + recv_exact(payload_len))
        if parsed is None or remainder:
            raise ValueError("Expected exactly one complete ZeroLink frame")
        return parsed

    init_frame = recv_frame()
    if init_frame[0] != PKT_HANDSHAKE or init_frame[1] != 0:
        raise ValueError("Expected a valid Windows HandshakeInit frame")
    init_payload = init_frame[2]
    if len(init_payload) < 88:
        raise ValueError("HandshakeInit payload too short")
    client_pk = init_payload[:32]
    salt = init_payload[32:64]
    handshake_version = struct.unpack(">I", init_payload[64:68])[0]
    if handshake_version != 3:
        raise ValueError(f"Unsupported handshake version: {handshake_version}")
    name_len = struct.unpack(">I", init_payload[68:72])[0]
    if len(init_payload) != 72 + name_len + 16 or any(init_payload[72 + name_len:]):
        raise ValueError("Unexpected paired-secret proof in unpaired simulator")

    # 1. Derive session keys from the actual Windows ephemeral key.
    server_sk = x25519.X25519PrivateKey.generate()
    server_pk = server_sk.public_key().public_bytes_raw()
    pairing_code = derive_pairing_code(client_pk, salt, server_pk)
    print(
        f"Windows pairing code to compare: {pairing_code}",
        flush=True,
    )
    shared_key = server_sk.exchange(x25519.X25519PublicKey.from_public_bytes(client_pk))
    okm = HKDF(algorithm=hashes.SHA256(), length=152, salt=salt,
               info=b"ZeroLink-v3-AEAD-Session-Keys").derive(shared_key)
    responder_key, iv_base, confirmation_key = okm[44:76], okm[76:88], okm[88:120]
    transcript = b"ZeroLink-v3-server-key-confirmation" + client_pk + salt + server_pk
    auth_tag = hmac.new(confirmation_key, transcript, "sha256").digest()[:16]
    device_name = b"ZeroLink Android Peer Simulator"
    reply = server_pk + auth_tag + struct.pack(">HI", 0x1001, len(device_name)) + device_name + bytes(16)
    s.sendall(build_frame(PKT_HANDSHAKE, 0, reply))
    confirm_pairing_code(pairing_code)
    aesgcm = AESGCM(responder_key)

    # Windows confirms that it derived the same session key before enabling data.
    confirmation = recv_frame()
    confirmation_transcript = (
        b"ZeroLink-v3-client-key-confirmation" + client_pk + salt + server_pk
    )
    expected_client_tag = hmac.new(
        confirmation_key, confirmation_transcript, "sha256"
    ).digest()[:16]
    if (
        confirmation[0] != PKT_HANDSHAKE
        or confirmation[1] != 1
        or confirmation[2] != bytes([0x02]) + expected_client_tag
    ):
        raise ValueError("Windows client key confirmation did not match the handshake")
    print("\n[Step 1] ECDH and mutual transcript key confirmation verified.")

    def send_encrypted(packet_type, sequence, plaintext):
        ciphertext = aesgcm.encrypt(derive_nonce(iv_base, sequence), plaintext, bytes([packet_type]))
        s.sendall(build_frame(packet_type, sequence, ciphertext))

    # The daemon gates feature payloads until Android approves first pairing.
    send_encrypted(PKT_SIGNALING, 1, bytes([0x50, 0x01]))
    print("\n[Step 2] Sending encrypted first-pair approval to open the data gate.")

    if os.environ.get("ZEROLINK_SIMULATOR_PAIRING_ONLY") == "1":
        time.sleep(0.8)
        print("[Step 2] Pairing-only run complete; synthetic feature packets were skipped.")
        s.close()
        return

    # 3. Send Notification
    print("\n[Step 3] Sending Remote Notification (Android -> Windows Action Center)...")
    notif_data = serialize_notification(
        msg_id="sim_msg_001",
        pkg="com.whatsapp",
        app="WhatsApp",
        title="Alex Rivera",
        content="Hey! ZeroLink low-power sync is working on Windows!",
    )
    send_encrypted(PKT_NOTIFICATION, 2, notif_data)
    print("  Title: 'Alex Rivera' | Content: 'Hey! ZeroLink low-power sync is working on Windows!'")
    time.sleep(0.5)

    # 4. Send Clipboard Sync only when explicitly requested; this replaces host clipboard data.
    next_sequence = 3
    if os.environ.get("ZEROLINK_SIMULATOR_SEND_CLIPBOARD") == "1":
        print("\n[Step 4] Sending Remote Clipboard Update (Android -> Windows Clipboard)...")
        clip_text = "https://github.com/prasanth-047/ZeroLink"
        clip_data = serialize_clipboard(clip_text)
        send_encrypted(PKT_CLIPBOARD, next_sequence, clip_data)
        next_sequence += 1
        print(f"  Clipboard Text: '{clip_text}'")
        time.sleep(0.5)
    else:
        print("\n[Step 4] Skipped synthetic clipboard update (set ZEROLINK_SIMULATOR_SEND_CLIPBOARD=1 to opt in).")

    # 5. Send Heartbeat Status
    print("\n[Step 5] Sending Android Heartbeat Status (Battery: 89%, Charging: True)...")
    hb_data = serialize_heartbeat(battery=89, charging=True, wifi=True, port=8443)
    send_encrypted(PKT_HEARTBEAT, next_sequence, hb_data)
    next_sequence += 1
    print("  Battery: 89% | Charging: Yes | Wi-Fi: Ready")
    time.sleep(0.5)

    # 6. Send File Catalog for Virtual Drive
    print("\n[Step 6] Sending Phone Photos Catalog (Generating Virtual Placeholders in File Explorer)...")
    catalog_files = [
        {
            "id": "photo_sunset_001",
            "path": "Camera/IMG_20261103_Sunset.jpg",
            "size": 3_452_100,
            "mtime": int(time.time()),
            "mime": "image/jpeg",
        },
        {
            "id": "photo_workspace_002",
            "path": "Camera/IMG_20261103_ZeroLinkSetup.jpg",
            "size": 4_891_020,
            "mtime": int(time.time()),
            "mime": "image/jpeg",
        },
        {
            "id": "doc_presentation_003",
            "path": "Documents/ZeroLink_Architecture_Notes.pdf",
            "size": 842_310,
            "mtime": int(time.time()),
            "mime": "application/pdf",
        },
    ]
    catalog_data = serialize_file_catalog("Android_DCIM", catalog_files)
    send_encrypted(PKT_FILE_METADATA, next_sequence, catalog_data)
    print(f"  Submitted {len(catalog_files)} catalog items to 'ZeroLink Photos'.")

    print("\n" + "=" * 65)
    print("  Encrypted peer traffic sent. Check the Windows daemon log for event handling.")
    print("=" * 65)
    s.close()


if __name__ == "__main__":
    run_simulation()

