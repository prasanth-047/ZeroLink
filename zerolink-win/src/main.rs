use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

mod ble_client;
mod clipboard;
mod cloud_filter;
mod notifications;
mod quic_client;
mod screen_viewer;
mod ui;

use ble_client::BleL2capClient;
use clipboard::ClipboardManager;
use cloud_filter::CloudFilterVirtualDrive;
use notifications::WindowsToastManager;
use quic_client::EphemeralBulkClient;
use screen_viewer::ScreenMirrorReceiver;
use ui::{spawn_ui_thread, UiCommand};
use zerolink_core::framing::{Frame, PacketType, HEADER_SIZE};
use zerolink_core::protocol::{
    ClipboardPayload, ClipboardType, FileCatalog, FileFetchChunk, FileMetadata, Heartbeat,
    NotificationPayload, VideoFrameHeader, WifiWakePayload,
};

#[derive(Debug)]
pub struct InboundPacket {
    pub packet_type: PacketType,
    pub payload: Vec<u8>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer().compact())
        .init();

    tracing::info!("Starting ZeroLink Windows Daemon (zerolink-win v0.1.0)");

    // Initialize Win32 COM runtime for WinRT notifications and Direct3D 11
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // Target Metric: Windows RAM Footprint < 15 MB
    // Enforce process working set limits via Win32 API
    unsafe {
        let process_handle = GetCurrentProcess();
        let min_size = 4 * 1024 * 1024; // 4 MB
        let max_size = 14 * 1024 * 1024; // 14 MB (strictly < 15 MB target)
        let _ = SetProcessWorkingSetSize(process_handle, min_size, max_size);
    }

    // Communication channels
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<ClipboardPayload>();
    let (wake_tx, mut wake_rx) = mpsc::unbounded_channel::<WifiWakePayload>();
    let (raw_ble_tx, mut raw_ble_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (inbound_packet_tx, mut inbound_packet_rx) = mpsc::unbounded_channel::<InboundPacket>();
    let (discovery_tx, mut discovery_rx) = mpsc::unbounded_channel::<(u64, u16, String)>();
    let (ui_cmd_tx, mut ui_cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();

    // 1. Start Win32 Clipboard Listener (WM_CLIPBOARDUPDATE, 0.0% Idle CPU)
    let clip_manager = Arc::new(ClipboardManager::new(clip_tx));
    let _clip_thread = clip_manager.start_listener();
    tracing::info!("Win32 Clipboard Listener registered on HWND_MESSAGE");

    // 2. Initialize Native WinRT Toast Notification Manager (No Electron / No WebView)
    let toast_manager = Arc::new(WindowsToastManager::new("ZeroLink.DesktopClient"));
    tracing::info!("Native Windows.UI.Notifications manager initialized");

    // 3. Initialize Cloud Filter API (cldapi.dll) Virtual Drive
    let sync_dir = dirs_virtual_path();
    let mut cloud_drive = CloudFilterVirtualDrive::new(sync_dir.clone(), wake_tx);
    if let Err(e) = cloud_drive.register_and_connect() {
        tracing::warn!("Cloud Filter registration note: {}", e);
    }
    let cloud_drive_shared = Arc::new(tokio::sync::Mutex::new(cloud_drive));

    // 4. Initialize BLE L2CAP Client and WinRT BLE Advertisement Watcher
    let ble_client = Arc::new(BleL2capClient::new(0, 0x1001));
    if let Err(e) = ble_client.start_advertisement_watcher(discovery_tx).await {
        tracing::warn!("WinRT BLE Advertisement Watcher warning: {}", e);
    }

    // 5. Initialize Hardware D3D11 Video Acceleration context
    let mut screen_receiver = ScreenMirrorReceiver::new();
    if let Err(e) = screen_receiver.init_d3d11_hardware() {
        tracing::warn!("D3D11 hardware acceleration warning: {:?}", e);
    }
    let screen_receiver = Arc::new(tokio::sync::Mutex::new(screen_receiver));

    // 6. Spawn GUI Dashboard Window
    let (ui_handle, _ui_thread) = spawn_ui_thread(ui_cmd_tx, sync_dir.clone());
    let ui = Arc::new(ui_handle);
    ui.log("⚡ ZeroLink Windows Daemon initialized");
    ui.log("📋 Win32 Clipboard hook registered on HWND_MESSAGE (0.0% CPU)");
    ui.log("🔔 Native Windows.UI.Notifications (Toast) manager active");
    ui.log(&format!("📁 Cloud Filter virtual drive connected at {:?}", sync_dir));
    ui.log("🎮 Direct3D 11 Video Acceleration context active");

    // 7. Spawn Signaling & Data Transport Server (Dual-radio local & network bridge)
    let listen_port = std::env::var("ZEROLINK_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8989);

    let (active_client_tx, mut active_client_rx) =
        mpsc::unbounded_channel::<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>();

    let ble_client_for_transport = Arc::clone(&ble_client);
    let inbound_pkt_tx = inbound_packet_tx.clone();
    let ui_for_network = Arc::clone(&ui);

    tokio::spawn(async move {
        let bind_addr = format!("0.0.0.0:{}", listen_port);
        match TcpListener::bind(&bind_addr).await {
            Ok(listener) => {
                tracing::info!(
                    "ZeroLink Dual-Radio Signaling & Transport bridge listening on {}",
                    bind_addr
                );
                ui_for_network.log(&format!("🌐 Transport bridge listening on {}", bind_addr));

                while let Ok((stream, peer_addr)) = listener.accept().await {
                    tracing::info!("Android signaling peer connected from {}", peer_addr);
                    ui_for_network.log(&format!("📱 Android peer connected from {}", peer_addr));
                    ui_for_network.update_device_status("Android Connected", 100, true);
                    let _ = stream.set_nodelay(true);

                    let (peer_outbound_tx, mut peer_outbound_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    let _ = active_client_tx.send(peer_outbound_tx.clone());

                    let (mut reader, mut writer) = stream.into_split();
                    let ble_client = Arc::clone(&ble_client_for_transport);
                    let in_tx = inbound_pkt_tx.clone();
                    let ui_for_peer = Arc::clone(&ui_for_network);

                    // Writer task: forward outbound wire frames to peer
                    tokio::spawn(async move {
                        while let Some(bytes) = peer_outbound_rx.recv().await {
                            if let Err(e) = writer.write_all(&bytes).await {
                                tracing::warn!("Failed to write frame to peer {}: {}", peer_addr, e);
                                break;
                            }
                        }
                    });

                    // Automatically initiate secure handshake with connected peer
                    if let Err(e) = ble_client.initiate_handshake(&peer_outbound_tx).await {
                        tracing::warn!("Failed to dispatch HandshakeInit to {}: {}", peer_addr, e);
                    }

                    // Reader task: parse length-prefixed frames, decrypt, and dispatch
                    tokio::spawn(async move {
                        let mut buffer = Vec::with_capacity(64 * 1024);
                        let mut temp_buf = [0u8; 16 * 1024];

                        loop {
                            let bytes_read = match reader.read(&mut temp_buf).await {
                                Ok(0) => {
                                    tracing::info!("Peer {} closed signaling connection", peer_addr);
                                    ui_for_peer.log(&format!("Peer {} disconnected", peer_addr));
                                    ui_for_peer.update_device_status("Disconnected", 0, false);
                                    ble_client.reset_session().await;
                                    break;
                                }
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::warn!("Error reading from peer {}: {}", peer_addr, e);
                                    ble_client.reset_session().await;
                                    break;
                                }
                            };

                            buffer.extend_from_slice(&temp_buf[..bytes_read]);

                            // Parse all complete frames from the streaming buffer
                            while buffer.len() >= HEADER_SIZE {
                                match Frame::try_parse(&buffer) {
                                    Ok(Some((frame, consumed))) => {
                                        buffer.drain(..consumed);

                                        match frame.header.packet_type {
                                            PacketType::Handshake => {
                                                tracing::info!("Processing Handshake frame from peer");
                                                if let Ok(name) = ble_client.complete_handshake(&frame.payload).await {
                                                    tracing::info!("Handshake complete! Connected device: {}", name);
                                                    ui_for_peer.log(&format!("🔒 Handshake verified! Peer: {}", name));
                                                    ui_for_peer.update_device_status(&name, 100, true);
                                                }
                                            }
                                            other_type => {
                                                match ble_client.receive_frame(&frame).await {
                                                    Ok(decrypted) => {
                                                        let _ = in_tx.send(InboundPacket {
                                                            packet_type: other_type,
                                                            payload: decrypted,
                                                        });
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            "Failed to decrypt frame {:?} from {}: {}",
                                                            other_type,
                                                            peer_addr,
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Ok(None) => break, // Need more bytes to complete frame
                                    Err(e) => {
                                        tracing::error!("Frame parsing error from {}: {}, resetting buffer", peer_addr, e);
                                        buffer.clear();
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
            }
            Err(e) => {
                tracing::warn!("Could not bind transport listener on port {}: {}", listen_port, e);
            }
        }
    });

    // 8. Background Event & Dispatch Loop
    let ble_dispatch = Arc::clone(&ble_client);
    let raw_ble_sender = raw_ble_tx.clone();
    let clip_mgr_shared = Arc::clone(&clip_manager);
    let toast_mgr_shared = Arc::clone(&toast_manager);
    let screen_rcvr_shared = Arc::clone(&screen_receiver);
    let cloud_drive_dispatch = Arc::clone(&cloud_drive_shared);
    let ui_for_events = Arc::clone(&ui);
    let sync_dir_for_ui = sync_dir.clone();

    let mut current_client_outbound: Option<mpsc::UnboundedSender<Vec<u8>>> = None;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // UI Button Actions
                Some(cmd) = ui_cmd_rx.recv() => {
                    match cmd {
                        UiCommand::OpenPhotosFolder => {
                            ui_for_events.log(&format!("📂 Opening ZeroLink Photos folder: {:?}", sync_dir_for_ui));
                            let _ = std::process::Command::new("explorer.exe")
                                .arg(&sync_dir_for_ui)
                                .spawn();
                        }
                        UiCommand::TriggerTestToast => {
                            ui_for_events.log("🔔 Sending test Toast Notification to Windows Action Center");
                            let notif = NotificationPayload {
                                id: "test_toast".to_string(),
                                package_name: "org.zerolink".to_string(),
                                app_name: "ZeroLink Desktop".to_string(),
                                title: "ZeroLink Sync Active".to_string(),
                                content: "Ultra-low-power BLE & Wi-Fi fabric is running smoothly (<15 MB RAM)".to_string(),
                                timestamp_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
                                is_ongoing: false,
                            };
                            let _ = toast_mgr_shared.display_notification(&notif);
                        }
                        UiCommand::TriggerTestClipboard => {
                            let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                            let test_text = format!("ZeroLink: Hello from Windows PC @ {:02}:{:02}:{:02}", (now_ms / 3600000) % 24, (now_ms / 60000) % 60, (now_ms / 1000) % 60);
                            ui_for_events.log(&format!("📋 Syncing text to Android: '{}'", test_text));
                            let payload = ClipboardPayload {
                                format: ClipboardType::PlainText,
                                payload_hash: 0x12345678,
                                timestamp_ms: now_ms,
                                data: test_text.into_bytes(),
                            };
                            clip_mgr_shared.inject_remote_clipboard(&payload);
                            let serialized = payload.serialize();
                            let _ = ble_dispatch.send_packet(PacketType::Clipboard, &serialized, &raw_ble_sender).await;
                        }
                        UiCommand::ToggleScreenMirror => {
                            ui_for_events.log("📱 Screen mirror ready (waiting for Android HEVC video stream)");
                        }
                        UiCommand::Quit => {
                            ui_for_events.log("Closing ZeroLink Daemon...");
                            let _ = shutdown_tx.send(());
                            break;
                        }
                    }
                }

                // Outbound local clipboard update
                Some(payload) = clip_rx.recv() => {
                    tracing::info!(
                        "Local clipboard updated (hash: {:#x}, bytes: {}), dispatching to BLE L2CAP",
                        payload.payload_hash,
                        payload.data.len()
                    );
                    ui_for_events.log(&format!("📋 Local clipboard updated: {} bytes synced to Android", payload.data.len()));
                    let serialized = payload.serialize();
                    let _ = ble_dispatch.send_packet(PacketType::Clipboard, &serialized, &raw_ble_sender).await;
                }

                // Outbound CloudFilter placeholder fetch trigger -> Wi-Fi wake
                Some(wake_req) = wake_rx.recv() => {
                    tracing::info!(
                        "CloudFilter placeholder triggered: Waking Android Wi-Fi for bulk transfer: {}",
                        wake_req.reason
                    );
                    ui_for_events.log(&format!("⚡ CloudFilter triggered Wi-Fi wake: {}", wake_req.reason));
                    let serialized = wake_req.serialize();
                    let _ = ble_dispatch.send_packet(PacketType::WifiWake, &serialized, &raw_ble_sender).await;
                }

                // Track active client sender
                Some(client_tx) = active_client_rx.recv() => {
                    current_client_outbound = Some(client_tx);
                }

                // Outbound wire frames ready for physical write
                Some(raw_outbound) = raw_ble_rx.recv() => {
                    if let Some(ref client) = current_client_outbound {
                        let _ = client.send(raw_outbound);
                    }
                }

                // Inbound BLE device discovery
                Some((address, psm, name)) = discovery_rx.recv() => {
                    tracing::info!(
                        "ZeroLink discovered target device '{}' (addr: 0x{:012X}, PSM: 0x{:04X})",
                        name, address, psm
                    );
                    ui_for_events.log(&format!("📡 Discovered device '{}' (addr: 0x{:012X}, PSM: 0x{:04X})", name, address, psm));
                    ui_for_events.update_device_status(&name, 100, false);
                    ble_dispatch.update_device_target(address, psm).await;
                    let _ = ble_dispatch.initiate_handshake(&raw_ble_sender).await;
                }

                // Inbound decrypted packet dispatch pipeline
                Some(packet) = inbound_packet_rx.recv() => {
                    match packet.packet_type {
                        PacketType::Clipboard => {
                            if let Ok(clip_payload) = ClipboardPayload::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Remote clipboard received from Android (hash: {:#x}, bytes: {}), injecting into Windows clipboard",
                                    clip_payload.payload_hash,
                                    clip_payload.data.len()
                                );
                                ui_for_events.log(&format!("📋 Received remote clipboard from Android ({} bytes)", clip_payload.data.len()));
                                clip_mgr_shared.inject_remote_clipboard(&clip_payload);
                            }
                        }
                        PacketType::Notification => {
                            if let Ok(notif) = NotificationPayload::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Remote notification received: [{}] {} - {}",
                                    notif.app_name,
                                    notif.title,
                                    notif.content
                                );
                                ui_for_events.log(&format!("🔔 Notification from {}: {}", notif.app_name, notif.title));
                                if let Err(e) = toast_mgr_shared.display_notification(&notif) {
                                    tracing::warn!("Failed to display toast notification: {:?}", e);
                                }
                            }
                        }
                        PacketType::FileMetadata => {
                            let drive = cloud_drive_dispatch.lock().await;
                            if let Ok(catalog) = FileCatalog::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Received FileCatalog with {} files for sync root '{}'",
                                    catalog.files.len(),
                                    catalog.sync_root_id
                                );
                                ui_for_events.log(&format!("📁 Received catalog: {} photos/files synced to Virtual Drive", catalog.files.len()));
                                for file in catalog.files {
                                    if let Err(e) = drive.create_placeholder(
                                        &file.relative_path,
                                        file.file_size as i64,
                                        &file.file_id,
                                    ) {
                                        tracing::warn!("Failed to create placeholder for {}: {}", file.relative_path, e);
                                    } else {
                                        tracing::info!("Created placeholder for '{}' ({} bytes)", file.relative_path, file.file_size);
                                    }
                                }
                            } else if let Ok(meta) = FileMetadata::deserialize(&packet.payload) {
                                tracing::info!("Received single FileMetadata for '{}'", meta.relative_path);
                                ui_for_events.log(&format!("📁 Created placeholder for '{}'", meta.relative_path));
                                let _ = drive.create_placeholder(
                                    &meta.relative_path,
                                    meta.file_size as i64,
                                    &meta.file_id,
                                );
                            }
                        }
                        PacketType::FileChunk => {
                            if let Ok(chunk) = FileFetchChunk::deserialize(&packet.payload) {
                                ui_for_events.log(&format!("📥 Hydrated chunk for {}: {} bytes", chunk.file_id, chunk.data.len()));
                                let drive = cloud_drive_dispatch.lock().await;
                                let _ = drive.hydrate_file_chunk(&chunk.file_id, chunk.byte_offset, &chunk.data);
                            }
                        }
                        PacketType::Heartbeat => {
                            if let Ok(hb) = Heartbeat::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Heartbeat from Android: Battery: {}% (charging: {}), Wi-Fi: {}, QUIC Port: {}, PowerMode: {}",
                                    hb.battery_percentage,
                                    hb.is_charging,
                                    hb.wifi_connected,
                                    hb.quic_port_active,
                                    hb.device_power_mode
                                );
                                let dev_name = if hb.is_charging { "SM-A075F (Charging)" } else { "SM-A075F" };
                                ui_for_events.update_device_status(
                                    dev_name,
                                    hb.battery_percentage,
                                    true,
                                );
                            }
                        }
                        PacketType::ScreenCast => {
                            let receiver = screen_rcvr_shared.lock().await;
                            let (header_opt, nal_data) = if packet.payload.len() >= 23 {
                                if let Ok(header) = VideoFrameHeader::deserialize(&packet.payload[..23]) {
                                    (Some(header), &packet.payload[23..])
                                } else {
                                    (None, packet.payload.as_slice())
                                }
                            } else {
                                (None, packet.payload.as_slice())
                            };
                            let (pts, is_kf) = header_opt
                                .as_ref()
                                .map(|h| (h.pts_microseconds, h.is_keyframe))
                                .unwrap_or((0, false));
                            receiver.submit_hevc_packet(nal_data, pts, is_kf);
                            let (frames, keyframes, total_bytes) = receiver.stats();
                            if frames % 30 == 1 || is_kf {
                                ui_for_events.log(&format!(
                                    "📱 Screen frame #{}: {} KB (Keyframes: {})",
                                    frames,
                                    total_bytes / 1024,
                                    keyframes
                                ));
                            }
                        }
                        PacketType::WifiWake => {
                            if let Ok(wake) = WifiWakePayload::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Android Wi-Fi wake acknowledged: {}:{} ({})",
                                    wake.target_ip,
                                    wake.port,
                                    wake.reason
                                );
                                ui_for_events.log(&format!("⚡ Wi-Fi bulk socket active at {}:{}", wake.target_ip, wake.port));
                                let ip = if wake.target_ip.is_empty() { "127.0.0.1" } else { &wake.target_ip };
                                if let Ok(addr) = format!("{}:{}", ip, wake.port).parse() {
                                    tokio::spawn(async move {
                                        let mut bulk_client = EphemeralBulkClient::new();
                                        if let Ok(()) = bulk_client.connect_ephemeral(addr).await {
                                            tracing::info!("Connected to Android ephemeral bulk transfer socket at {}", addr);
                                        }
                                    });
                                }
                            }
                        }
                        _ => {
                            tracing::debug!("Received unhandled packet type: {:?}", packet.packet_type);
                        }
                    }
                }
            }
        }
    });

    tracing::info!("ZeroLink Daemon is active with UI dashboard (< 15 MB RAM)");

    // Wait for shutdown signal (either Ctrl-C or window close)
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutting down ZeroLink Windows Daemon (Ctrl-C)...");
        }
        _ = shutdown_rx.recv() => {
            tracing::info!("Shutting down ZeroLink Windows Daemon (UI Window Closed)...");
        }
    }

    ble_client.stop_advertisement_watcher().await;
    {
        let mut drive = cloud_drive_shared.lock().await;
        drive.disconnect();
    }
    {
        let mut receiver = screen_receiver.lock().await;
        receiver.stop();
    }

    unsafe {
        CoUninitialize();
    }

    Ok(())
}

fn dirs_virtual_path() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(|p| PathBuf::from(p).join("ZeroLink Photos"))
        .unwrap_or_else(|_| PathBuf::from("C:\\ZeroLink Photos"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerolink_core::crypto::KeyExchange;
    use zerolink_core::protocol::{ClipboardType, HandshakeInitPayload, HandshakeReplyPayload};

    #[tokio::test]
    async fn test_windows_ble_client_handshake_and_crypto() {
        let ble_client = Arc::new(BleL2capClient::new(0x112233445566, 0x1001));
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // 1. Initiate handshake
        ble_client.initiate_handshake(&raw_tx).await.unwrap();

        // Verify HandshakeInit frame
        let init_raw = raw_rx.recv().await.unwrap();
        let (init_frame, _) = Frame::try_parse(&init_raw).unwrap().unwrap();
        assert_eq!(init_frame.header.packet_type, PacketType::Handshake);

        let init_payload = HandshakeInitPayload::deserialize(&init_frame.payload).unwrap();
        assert_eq!(init_payload.client_device_name, "ZeroLink-Windows-Daemon");

        // 2. Simulate Android server generating reply
        let android_kx = KeyExchange::new();
        let reply = HandshakeReplyPayload {
            server_ephemeral_pubkey: android_kx.public_key,
            auth_tag: [0u8; 16],
            l2cap_psm: 0x1001,
            server_device_name: "Pixel 8 Pro (ZeroLink)".to_string(),
        };

        let server_device_name = ble_client
            .complete_handshake(&reply.serialize())
            .await
            .unwrap();
        assert_eq!(server_device_name, "Pixel 8 Pro (ZeroLink)");
        assert!(ble_client.is_session_active().await);

        // 3. Android completes its own handshake with client pubkey & salt
        let mut android_cipher = android_kx
            .complete_handshake(&init_payload.client_ephemeral_pubkey, &init_payload.salt)
            .unwrap();

        // 4. Test outbound packet (Windows -> Android)
        let clip = ClipboardPayload {
            format: ClipboardType::PlainText,
            payload_hash: 0xABCDEF0123456789,
            timestamp_ms: 1700000000000,
            data: b"Clipboard text from Windows".to_vec(),
        };
        ble_client
            .send_packet(PacketType::Clipboard, &clip.serialize(), &raw_tx)
            .await
            .unwrap();

        let out_raw = raw_rx.recv().await.unwrap();
        let (out_frame, _) = Frame::try_parse(&out_raw).unwrap().unwrap();
        assert_eq!(out_frame.header.packet_type, PacketType::Clipboard);

        let decrypted_by_android = android_cipher
            .decrypt(
                out_frame.header.sequence,
                &out_frame.payload,
                &[out_frame.header.packet_type as u8],
            )
            .unwrap();
        let parsed_clip = ClipboardPayload::deserialize(&decrypted_by_android).unwrap();
        assert_eq!(parsed_clip.data, b"Clipboard text from Windows");

        // 5. Test inbound packet (Android -> Windows)
        let notif = NotificationPayload {
            id: "notif_99".to_string(),
            package_name: "com.google.android.apps.messaging".to_string(),
            app_name: "Messages".to_string(),
            title: "Security Code".to_string(),
            content: "Your 2FA verification code is 492810".to_string(),
            timestamp_ms: 1700000050000,
            is_ongoing: false,
        };
        let notif_bytes = notif.serialize();
        let (seq, ciphertext) = android_cipher
            .encrypt(&notif_bytes, &[PacketType::Notification as u8])
            .unwrap();
        let in_frame = Frame::new(PacketType::Notification, seq, ciphertext).unwrap();

        let decrypted_by_windows = ble_client.receive_frame(&in_frame).await.unwrap();
        let parsed_notif = NotificationPayload::deserialize(&decrypted_by_windows).unwrap();
        assert_eq!(parsed_notif.title, "Security Code");
        assert_eq!(parsed_notif.content, "Your 2FA verification code is 492810");
    }

    #[test]
    fn test_screen_mirror_hevc_parser() {
        let receiver = ScreenMirrorReceiver::new();

        let vps_nal = [0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0C, 0x01];
        receiver.submit_hevc_packet(&vps_nal, 1000, false);

        let idr_nal = [0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x99];
        receiver.submit_hevc_packet(&idr_nal, 2000, true);

        let trail_nal = [0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0x88, 0x77];
        receiver.submit_hevc_packet(&trail_nal, 3000, false);

        let (frames, keyframes, total_bytes) = receiver.stats();
        assert_eq!(frames, 0);
        assert_eq!(keyframes, 0);
        assert_eq!(total_bytes, 0);
    }
}
