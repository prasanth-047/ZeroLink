use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::{timeout, Duration};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

mod ble_client;
mod bulk_client;
mod clipboard;
mod cloud_filter;
mod d3d_presenter;
mod hevc_decoder;
mod media;
mod notifications;
pub(crate) mod pairing_store;
mod screen_viewer;
mod ui;

use ble_client::BleL2capClient;
use bulk_client::EphemeralBulkClient;
use clipboard::ClipboardManager;
use cloud_filter::{
    CloudFetchCancellation, CloudFetchIdentity, CloudFetchRequest, CloudFilterEvent,
    CloudFilterVirtualDrive,
};
use notifications::WindowsToastManager;
use screen_viewer::ScreenMirrorReceiver;
use screen_viewer::ScreenViewerWindow;
use ui::{spawn_ui_thread, UiCommand};
use zerolink_core::crypto::generate_salt;
use zerolink_core::framing::{Frame, PacketType, HEADER_SIZE};
use zerolink_core::protocol::{
    ClipboardPayload, ClipboardType, FileCatalog, FileFetchChunk, FileMetadata, Heartbeat,
    MediaControlPayload, MediaStatePayload, NotificationActionPayload, NotificationPayload,
    ScreenInputPayload, VideoFrameHeader, WifiWakePayload,
};

#[derive(Debug)]
pub struct InboundPacket {
    pub packet_type: PacketType,
    pub payload: Vec<u8>,
}

enum BulkSessionEvent {
    Completed(String, CloudFetchIdentity),
    Cancelled(String, CloudFetchIdentity),
    TimedOut(String),
}

fn create_bulk_session_token() -> String {
    generate_salt()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn remove_fully_cancelled_fetches(
    pending: &mut VecDeque<CloudFetchRequest>,
    cancellation: &CloudFetchCancellation,
) -> Vec<CloudFetchIdentity> {
    let mut removed = Vec::new();
    pending.retain(|request| {
        if request.is_fully_cancelled_by(cancellation) {
            removed.push(request.identity());
            false
        } else {
            true
        }
    });
    removed
}

async fn send_bulk_wake(
    ble_client: &BleL2capClient,
    raw_sender: &mpsc::UnboundedSender<Vec<u8>>,
) -> zerolink_core::error::Result<String> {
    let token = create_bulk_session_token();
    let port = resolve_bulk_port(std::env::var("ZEROLINK_TEST_BULK_PORT").ok().as_deref());
    let wake = WifiWakePayload {
        target_ip: String::new(),
        port,
        session_token: token.clone(),
        reason: "wake".to_string(),
    };
    ble_client
        .send_packet(PacketType::WifiWake, &wake.serialize(), raw_sender)
        .await?;
    Ok(token)
}

async fn fetch_bulk_range_cancellable(
    address: SocketAddr,
    request: &CloudFetchRequest,
    token: &str,
    cancel_receiver: oneshot::Receiver<()>,
) -> Option<zerolink_core::error::Result<(u64, Vec<u8>)>> {
    let mut bulk_client = EphemeralBulkClient::new();
    let transfer = async {
        timeout(
            Duration::from_secs(8),
            bulk_client.connect_ephemeral(address),
        )
        .await
        .map_err(|_| {
            zerolink_core::error::ZeroLinkError::ConnectionError(
                "Bulk socket connect timed out".into(),
            )
        })??;
        timeout(
            Duration::from_secs(20),
            bulk_client.fetch_file_chunk(
                &request.file_id,
                token,
                request.offset as u64,
                request.length,
            ),
        )
        .await
        .map_err(|_| {
            zerolink_core::error::ZeroLinkError::ConnectionError(
                "Bulk file request timed out".into(),
            )
        })?
    };

    let result = tokio::select! {
        biased;
        _ = cancel_receiver => None,
        result = transfer => Some(result),
    };
    bulk_client.teardown();
    result
}

async fn start_next_bulk_wake(
    pending: &mut VecDeque<CloudFetchRequest>,
    active_token: &mut Option<String>,
    waiting_for_ack: &mut bool,
    waiting_request: &mut Option<CloudFetchIdentity>,
    ble_client: &Arc<BleL2capClient>,
    raw_sender: &mpsc::UnboundedSender<Vec<u8>>,
    event_sender: &mpsc::UnboundedSender<BulkSessionEvent>,
    cloud_drive: &Arc<tokio::sync::Mutex<CloudFilterVirtualDrive>>,
) {
    if active_token.is_some() || pending.is_empty() {
        return;
    }
    match send_bulk_wake(ble_client, raw_sender).await {
        Ok(token) => {
            *waiting_request = pending.front().map(CloudFetchRequest::identity);
            *active_token = Some(token.clone());
            *waiting_for_ack = true;
            let timeout_sender = event_sender.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(40)).await;
                let _ = timeout_sender.send(BulkSessionEvent::TimedOut(token));
            });
        }
        Err(error) => {
            *waiting_request = None;
            tracing::warn!("Could not request Android bulk transfer: {error}");
            let drive = cloud_drive.lock().await;
            while let Some(request) = pending.pop_front() {
                if let Err(error) = drive.fail_fetch(&request) {
                    tracing::warn!("Could not fail Cloud Files request: {error}");
                }
            }
        }
    }
}

// Keep the idle daemon's async pool proportional to its I/O workload instead of
// creating one worker for every logical CPU on the host.
#[tokio::main(worker_threads = 2)]
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

    // Communication channels
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<ClipboardPayload>();
    let (wake_tx, mut wake_rx) = mpsc::unbounded_channel::<CloudFilterEvent>();
    let (raw_ble_tx, mut raw_ble_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (inbound_packet_tx, mut inbound_packet_rx) = mpsc::unbounded_channel::<InboundPacket>();
    let (discovery_tx, mut discovery_rx) = mpsc::unbounded_channel::<(u64, u16, String)>();
    let (ui_cmd_tx, mut ui_cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
    let (notif_action_tx, mut notif_action_rx) =
        mpsc::unbounded_channel::<NotificationActionPayload>();
    let (screen_input_tx, mut screen_input_rx) = mpsc::unbounded_channel::<ScreenInputPayload>();
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
    let (bulk_event_tx, mut bulk_event_rx) = mpsc::unbounded_channel::<BulkSessionEvent>();

    // 1. Start Win32 Clipboard Listener (WM_CLIPBOARDUPDATE, 0.0% Idle CPU)
    let clip_manager = Arc::new(ClipboardManager::new(clip_tx));
    let _clip_thread = clip_manager.start_listener();
    tracing::info!("Win32 Clipboard Listener registered on HWND_MESSAGE");

    // 2. Initialize Native WinRT Toast Notification Manager (No Electron / No WebView)
    let toast_manager = Arc::new(WindowsToastManager::new(
        "ZeroLink.DesktopClient",
        notif_action_tx,
    ));
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

    // 5. Keep the screen decoder cold until the user opens screen mirroring.
    let screen_receiver = Arc::new(tokio::sync::Mutex::new(ScreenMirrorReceiver::new()));

    // 6. Spawn GUI Dashboard Window
    let (ui_handle, _ui_thread) = spawn_ui_thread(ui_cmd_tx, sync_dir.clone());
    let ui = Arc::new(ui_handle);
    ui.log("⚡ ZeroLink Windows Daemon initialized");
    ui.log("📋 Win32 clipboard listener registered");
    ui.log("🔔 Native Windows.UI.Notifications (Toast) manager active");
    ui.log(&format!(
        "📁 Cloud Filter virtual drive connected at {:?}",
        sync_dir
    ));
    ui.log("🖥️ Screen viewer initialized; Media Foundation prefers D3D11 decode with software fallback");

    // 7. Spawn Signaling & Data Transport Server (Dual-radio local & network bridge)
    let listen_port = std::env::var("ZEROLINK_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8989);

    let (active_client_tx, mut active_client_rx) =
        mpsc::unbounded_channel::<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>();
    let tcp_session_slot = Arc::new(Semaphore::new(1));

    let ble_client_for_transport = Arc::clone(&ble_client);
    let inbound_pkt_tx = inbound_packet_tx.clone();
    let ui_for_network = Arc::clone(&ui);

    let bind_ip = resolve_bind_ip(std::env::var("ZEROLINK_BIND_ADDR").ok().as_deref())?;
    let bind_addr = SocketAddr::new(bind_ip, listen_port);
    tokio::spawn(async move {
        match TcpListener::bind(&bind_addr).await {
            Ok(listener) => {
                tracing::info!(
                    "ZeroLink Dual-Radio Signaling & Transport bridge listening on {}",
                    bind_addr
                );
                ui_for_network.log(&format!("🌐 Transport bridge listening on {}", bind_addr));

                while let Ok((stream, _peer_addr)) = listener.accept().await {
                    let session_permit = match try_acquire_tcp_session(&tcp_session_slot) {
                        Some(permit) => permit,
                        None => {
                            tracing::warn!(
                                "Rejecting TCP peer because another signaling session is active"
                            );
                            continue;
                        }
                    };

                    tracing::info!("Android signaling peer connected");
                    ui_for_network.log("📱 Android peer connected");
                    ui_for_network.update_device_status("Android Connected", 100, true);
                    let _ = stream.set_nodelay(true);

                    let (peer_outbound_tx, mut peer_outbound_rx) =
                        mpsc::unbounded_channel::<Vec<u8>>();
                    let _ = active_client_tx.send(peer_outbound_tx.clone());

                    let (mut reader, mut writer) = stream.into_split();
                    let ble_client = Arc::clone(&ble_client_for_transport);
                    let in_tx = inbound_pkt_tx.clone();
                    let ui_for_peer = Arc::clone(&ui_for_network);

                    // Writer task: forward outbound wire frames to peer
                    tokio::spawn(async move {
                        while let Some(bytes) = peer_outbound_rx.recv().await {
                            let byte_count = bytes.len();
                            if let Err(e) = writer.write_all(&bytes).await {
                                tracing::warn!("Failed to write frame to peer: {}", e);
                                break;
                            }
                            tracing::debug!("Sent {} bytes to peer", byte_count);
                        }
                    });

                    // Automatically initiate secure handshake with connected peer
                    if let Err(e) = ble_client.initiate_handshake(&peer_outbound_tx).await {
                        tracing::warn!("Failed to dispatch HandshakeInit: {}", e);
                    }

                    // Reader task: parse length-prefixed frames, decrypt, and dispatch
                    tokio::spawn(async move {
                        let _session_permit: OwnedSemaphorePermit = session_permit;
                        let mut buffer = Vec::with_capacity(64 * 1024);
                        let mut temp_buf = [0u8; 16 * 1024];

                        loop {
                            let bytes_read = match reader.read(&mut temp_buf).await {
                                Ok(0) => {
                                    tracing::info!("Android peer closed the signaling connection");
                                    ui_for_peer.log("Android peer disconnected");
                                    ui_for_peer.update_device_status("Disconnected", 0, false);
                                    ble_client.reset_session().await;
                                    break;
                                }
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::warn!("Error reading from Android peer: {}", e);
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
                                                tracing::info!(
                                                    "Processing Handshake frame from peer"
                                                );
                                                if let Ok(name) = ble_client
                                                    .complete_handshake(
                                                        &frame.payload,
                                                        &peer_outbound_tx,
                                                    )
                                                    .await
                                                {
                                                    tracing::info!(
                                                        "Handshake complete and peer authenticated"
                                                    );
                                                    ui_for_peer.log("🔒 Handshake verified");
                                                    ui_for_peer.update_device_status(
                                                        &name,
                                                        100,
                                                        ble_client.is_paired().await,
                                                    );
                                                }
                                            }
                                            other_type => {
                                                match ble_client.receive_frame(&frame).await {
                                                    Ok(decrypted) => {
                                                        if other_type == PacketType::Signaling
                                                            && decrypted == [0x50, 0x01]
                                                            && !ble_client.is_paired().await
                                                        {
                                                            match ble_client
                                                                .session_pairing_secret()
                                                                .await
                                                            {
                                                                Ok(secret) => {
                                                                    if let Err(e) = ble_client
                                                                        .save_pairing_secret(secret)
                                                                        .await
                                                                    {
                                                                        tracing::error!("Could not save Android-approved pairing secret: {}", e);
                                                                        continue;
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    tracing::error!("Could not retrieve pairing secret after Android approval: {}", e);
                                                                    continue;
                                                                }
                                                            }
                                                        }
                                                        if !ble_client.is_paired().await
                                                            && other_type != PacketType::Signaling
                                                        {
                                                            tracing::warn!("Dropping {:?} from unpaired device until pairing is acknowledged", other_type);
                                                            continue;
                                                        }
                                                        let _ = in_tx.send(InboundPacket {
                                                            packet_type: other_type,
                                                            payload: decrypted,
                                                        });
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            "Failed to decrypt frame {:?}: {}",
                                                            other_type,
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Ok(None) => break, // Need more bytes to complete frame
                                    Err(e) => {
                                        tracing::error!(
                                            "Frame parsing error: {}, resetting buffer",
                                            e
                                        );
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
                tracing::warn!(
                    "Could not bind transport listener on port {}: {}",
                    listen_port,
                    e
                );
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
    let mut last_media_state: Option<MediaStatePayload> = None;
    let mut screen_viewer: Option<ScreenViewerWindow> = None;
    let mut pending_forget_ack = false;
    let mut pending_cloud_fetches = VecDeque::<CloudFetchRequest>::new();
    let mut active_bulk_token: Option<String> = None;
    let mut waiting_for_bulk_ack = false;
    let mut waiting_for_fetch: Option<CloudFetchIdentity> = None;
    let mut active_fetch_cancellations =
        HashMap::<CloudFetchIdentity, (CloudFetchRequest, oneshot::Sender<()>)>::new();

    // The isolated memory benchmark uses this opt-in timer to exercise the
    // normal Cloud Files disconnect path before unregistering its temporary root.
    if let Some(seconds) = std::env::var("ZEROLINK_TEST_SHUTDOWN_AFTER_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=1_200).contains(seconds))
    {
        let test_shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let _ = test_shutdown_tx.send(());
        });
    }

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Outbound toast quick-reply / dismiss actions from WinRT activation
                Some(action) = notif_action_rx.recv() => {
                    tracing::info!(
                        "Toast action '{}' received (reply bytes: {})",
                        action.action_id,
                        action.reply_text.len()
                    );
                    if action.action_id == "reply" && !action.reply_text.is_empty() {
                        ui_for_events.log(&format!("💬 Quick reply sent to Android ({} bytes)", action.reply_text.len()));
                    } else {
                        ui_for_events.log(&format!("🔕 Toast action '{}' dispatched to Android", action.action_id));
                    }
                    let serialized = action.serialize();
                    if let Err(e) = ble_dispatch
                        .send_packet(PacketType::NotificationAction, &serialized, &raw_ble_sender)
                        .await
                    {
                        tracing::warn!("Failed to forward toast action to Android: {}", e);
                    }
                }
                // Outbound normalized pointer input from the screen viewer window
                Some(input) = screen_input_rx.recv() => {
                    let serialized = input.serialize();
                    if let Err(e) = ble_dispatch
                        .send_packet(PacketType::ScreenInput, &serialized, &raw_ble_sender)
                        .await
                    {
                        tracing::warn!("Failed to forward screen input to Android: {}", e);
                    }
                }

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
                                content: "ZeroLink test notification sent successfully.".to_string(),
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
                            if let Err(error) = clip_mgr_shared.inject_remote_clipboard(&payload) {
                                tracing::warn!("Windows test clipboard injection failed: {}", error);
                            }
                            let serialized = payload.serialize();
                            let _ = ble_dispatch.send_packet(PacketType::Clipboard, &serialized, &raw_ble_sender).await;
                        }
                        UiCommand::ToggleScreenMirror => {
                            if let Some(mut viewer) = screen_viewer.take() {
                                viewer.close();
                                ui_for_events.log("📱 Screen viewer closed");
                            } else {
                                let (device, context) = {
                                    let mut receiver = screen_rcvr_shared.lock().await;
                                    if receiver.d3d11_handles().0.is_none() {
                                        if let Err(error) = receiver.init_d3d11_hardware() {
                                            tracing::warn!(
                                                "D3D11 screen-mirroring initialization warning: {:?}",
                                                error
                                            );
                                        }
                                    }
                                    receiver.d3d11_handles()
                                };
                                screen_viewer = Some(ScreenViewerWindow::spawn(
                                    screen_input_tx.clone(),
                                    device,
                                    context,
                                ));
                                ui_for_events.log("📱 Screen viewer opened — click/drag/scroll to control Android");
                            }
                        }
                        UiCommand::ForgetPairedDevice => {
                            let active_and_paired = ble_dispatch.is_session_active().await && ble_dispatch.is_paired().await;
                            if active_and_paired {
                                match ble_dispatch.send_packet(PacketType::Signaling, &[0x50, 0x02], &raw_ble_sender).await {
                                    Ok(()) => {
                                        pending_forget_ack = true;
                                        ui_for_events.log("🔐 Pairing removal requested. Waiting for Android acknowledgement before clearing Windows state.");
                                    }
                                    Err(e) => ui_for_events.log(&format!("⚠️ Could not send pairing removal request: {}", e)),
                                }
                            } else {
                                match ble_dispatch.forget_pairing().await {
                                    Ok(()) => ui_for_events.log("🔐 Windows pairing removed locally. Open ZeroLink on Android and use Forget Pairing there before pairing again."),
                                    Err(e) => tracing::error!("Could not forget paired device: {}", e),
                                }
                            }
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
                        "Local clipboard updated ({} bytes), dispatching to BLE L2CAP",
                        payload.data.len()
                    );
                    ui_for_events.log(&format!("📋 Local clipboard updated: {} bytes synced to Android", payload.data.len()));
                    let serialized = payload.serialize();
                    let _ = ble_dispatch.send_packet(PacketType::Clipboard, &serialized, &raw_ble_sender).await;
                }

                // Queue Cloud Files fetches and retire work that the OS no longer needs.
                Some(fetch_event) = wake_rx.recv() => {
                    match fetch_event {
                        CloudFilterEvent::Fetch(fetch_request) => {
                            pending_cloud_fetches.push_back(fetch_request);
                            if active_bulk_token.is_none() {
                                ui_for_events.log("⚡ Cloud Files requested a phone photo; waking Wi-Fi");
                                start_next_bulk_wake(
                                    &mut pending_cloud_fetches,
                                    &mut active_bulk_token,
                                    &mut waiting_for_bulk_ack,
                                    &mut waiting_for_fetch,
                                    &ble_dispatch,
                                    &raw_ble_sender,
                                    &bulk_event_tx,
                                    &cloud_drive_dispatch,
                                ).await;
                            }
                        }
                        CloudFilterEvent::Cancel(cancellation) => {
                            let removed_fetches = remove_fully_cancelled_fetches(
                                &mut pending_cloud_fetches,
                                &cancellation,
                            );
                            let waiting_fetch_cancelled = waiting_for_fetch
                                .is_some_and(|identity| removed_fetches.contains(&identity));

                            if waiting_fetch_cancelled {
                                active_bulk_token = None;
                                waiting_for_bulk_ack = false;
                                waiting_for_fetch = None;
                            }

                            let active_fetch_cancelled = active_fetch_cancellations
                                .get(&cancellation.identity)
                                .is_some_and(|(request, _)| request.is_fully_cancelled_by(&cancellation));
                            if active_fetch_cancelled {
                                if let Some((_, cancel_sender)) = active_fetch_cancellations
                                    .remove(&cancellation.identity)
                                {
                                    let _ = cancel_sender.send(());
                                }
                            }

                            if waiting_fetch_cancelled {
                                start_next_bulk_wake(
                                    &mut pending_cloud_fetches,
                                    &mut active_bulk_token,
                                    &mut waiting_for_bulk_ack,
                                    &mut waiting_for_fetch,
                                    &ble_dispatch,
                                    &raw_ble_sender,
                                    &bulk_event_tx,
                                    &cloud_drive_dispatch,
                                ).await;
                            }
                        }
                    }
                }

                Some(event) = bulk_event_rx.recv() => {
                    match event {
                        BulkSessionEvent::Completed(token, identity)
                        | BulkSessionEvent::Cancelled(token, identity)
                            if active_bulk_token.as_deref() == Some(token.as_str()) => {
                            active_fetch_cancellations.remove(&identity);
                            active_bulk_token = None;
                            waiting_for_bulk_ack = false;
                            start_next_bulk_wake(
                                &mut pending_cloud_fetches,
                                &mut active_bulk_token,
                                &mut waiting_for_bulk_ack,
                                &mut waiting_for_fetch,
                                &ble_dispatch,
                                &raw_ble_sender,
                                &bulk_event_tx,
                                &cloud_drive_dispatch,
                            ).await;
                        }
                        BulkSessionEvent::TimedOut(token) if waiting_for_bulk_ack && active_bulk_token.as_deref() == Some(token.as_str()) => {
                            tracing::warn!("Android bulk-transfer wake timed out");
                            ui_for_events.log("⚠️ Phone photo transfer timed out");
                            active_bulk_token = None;
                            waiting_for_bulk_ack = false;
                            waiting_for_fetch = None;
                            let drive = cloud_drive_dispatch.lock().await;
                            while let Some(request) = pending_cloud_fetches.pop_front() {
                                if let Err(error) = drive.fail_fetch(&request) {
                                    tracing::warn!("Could not fail timed-out Cloud Files request: {error}");
                                }
                            }
                        }
                        _ => {}
                    }
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
                        "ZeroLink BLE peer discovered (PSM: 0x{:04X})",
                        psm
                    );
                    ui_for_events.log("📡 Discovered a ZeroLink BLE peer");
                    ui_for_events.update_device_status(&name, 100, false);
                    ble_dispatch.update_device_target(address, psm).await;
                    let _ = ble_dispatch.initiate_handshake(&raw_ble_sender).await;
                }

                // Inbound decrypted packet dispatch pipeline
                Some(packet) = inbound_packet_rx.recv() => {
                    match packet.packet_type {
                        PacketType::Signaling => {
                            if packet.payload == [0x50, 0x01] {
                                ui_for_events.log("🔐 Android approved the same pairing code. This device is now paired.");
                                ui_for_events.update_device_status("Paired Android device", 100, true);
                            } else if packet.payload == [0x50, 0x02] && ble_dispatch.is_paired().await {
                                match ble_dispatch.send_packet(PacketType::Signaling, &[0x50, 0x03], &raw_ble_sender).await {
                                    Ok(()) => match ble_dispatch.forget_pairing().await {
                                        Ok(()) => {
                                            ui_for_events.log("🔐 Android requested device removal; pairing was forgotten on both devices.");
                                            ui_for_events.update_device_status("Android device forgotten", 0, false);
                                        }
                                        Err(e) => tracing::error!("Could not forget paired device: {}", e),
                                    },
                                    Err(e) => tracing::error!("Could not acknowledge pairing removal: {}", e),
                                }
                            } else if packet.payload == [0x50, 0x03] && pending_forget_ack {
                                pending_forget_ack = false;
                                match ble_dispatch.forget_pairing().await {
                                    Ok(()) => {
                                        ui_for_events.log("🔐 Windows received the pairing-removal acknowledgement; pairing was forgotten on both devices.");
                                        ui_for_events.update_device_status("Android device forgotten", 0, false);
                                    }
                                    Err(e) => tracing::error!("Could not forget paired device after acknowledgement: {}", e),
                                }
                            }
                        }
                        PacketType::Clipboard => {
                            if let Ok(clip_payload) = ClipboardPayload::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Remote clipboard received from Android ({} bytes); attempting Windows clipboard injection",
                                    clip_payload.data.len()
                                );
                                ui_for_events.log(&format!("📋 Received remote clipboard from Android ({} bytes)", clip_payload.data.len()));
                                match clip_mgr_shared.inject_remote_clipboard(&clip_payload) {
                                    Ok(()) => tracing::info!("Remote clipboard injected into Windows successfully"),
                                    Err(error) => tracing::warn!("Remote clipboard injection failed: {}", error),
                                }
                            }
                        }
                        PacketType::Notification => {
                            if let Ok(notif) = NotificationPayload::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Remote notification received from Android; forwarding to Windows Action Center"
                                );
                                ui_for_events.log("🔔 Notification received from Android");
                                if let Err(e) = toast_mgr_shared.display_notification(&notif) {
                                    tracing::warn!("Failed to display toast notification: {:?}", e);
                                }
                            }
                        }
                        PacketType::FileMetadata => {
                            let mut drive = cloud_drive_dispatch.lock().await;
                            if let Ok(catalog) = FileCatalog::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Received FileCatalog with {} entries",
                                    catalog.files.len()
                                );
                                ui_for_events.log(&format!("📁 Received catalog: {} photos/files synced to Virtual Drive", catalog.files.len()));
                                for file in catalog.files {
                                    if let Err(e) = drive.create_placeholder(
                                        &file.relative_path,
                                        file.file_size as i64,
                                        &file.file_id,
                                    ) {
                                        tracing::warn!("Failed to create a catalog placeholder: {}", e);
                                    }
                                }
                            } else if let Ok(meta) = FileMetadata::deserialize(&packet.payload) {
                                tracing::info!("Received single-file metadata update");
                                ui_for_events.log("📁 Received a file metadata update");
                                let _ = drive.create_placeholder(
                                    &meta.relative_path,
                                    meta.file_size as i64,
                                    &meta.file_id,
                                );
                            }
                        }
                        PacketType::FileChunk => {
                            if let Ok(chunk) = FileFetchChunk::deserialize(&packet.payload) {
                                ui_for_events.log(&format!("📥 Hydrated a file chunk ({} bytes)", chunk.data.len()));
                                let drive = cloud_drive_dispatch.lock().await;
                                let _ = drive.hydrate_file_chunk(&chunk.file_id, chunk.byte_offset, &chunk.data);
                            }
                        }
                        PacketType::Heartbeat => {
                            if let Ok(hb) = Heartbeat::deserialize(&packet.payload) {
                                tracing::info!(
                                    "Heartbeat from Android: Battery: {}% (charging: {}), Wi-Fi: {}, bulk TCP port: {}, PowerMode: {}",
                                    hb.battery_percentage,
                                    hb.is_charging,
                                    hb.wifi_connected,
                                    hb.bulk_port_active,
                                    hb.device_power_mode
                                );
                                let dev_name = if hb.is_charging { "SM-A075F (Charging)" } else { "SM-A075F" };
                                ui_for_events.update_device_status(
                                    dev_name,
                                    hb.battery_percentage,
                                    true,
                                );

                                // Piggyback media-state refresh on the existing 10 s heartbeat.
                                // No dedicated timer is created, preserving the 0.0% idle CPU claim.
                                let state = tokio::task::spawn_blocking(media::query_state)
                                    .await
                                    .unwrap_or(None);
                                if state != last_media_state {
                                    if let Some(ref now_playing) = state {
                                        let playback_state = if now_playing.is_playing { "playing" } else { "paused" };
                                        tracing::info!(
                                            "Media playback state changed ({})",
                                            playback_state
                                        );
                                        ui_for_events.log(&format!(
                                            "🎵 Media playback state: {}",
                                            playback_state
                                        ));
                                        let serialized = now_playing.serialize();
                                        let _ = ble_dispatch
                                            .send_packet(PacketType::MediaState, &serialized, &raw_ble_sender)
                                            .await;
                                    }
                                    last_media_state = state;
                                }
                            }
                        }
                        PacketType::MediaControl => {
                            if let Ok(control) = MediaControlPayload::deserialize(&packet.payload) {
                                tracing::info!("Media command from Android: {:?}", control.command);
                                ui_for_events.log(&format!("🎵 Media command from Android: {:?}", control.command));
                                let command = control.command;
                                let executed = tokio::task::spawn_blocking(move || {
                                    media::execute_command(command)
                                })
                                .await
                                .unwrap_or(false);

                                if !executed {
                                    tracing::warn!("Media command {:?} was not accepted by the active session", command);
                                }

                                // Reply with a fresh snapshot so the phone UI updates immediately.
                                let state = tokio::task::spawn_blocking(media::query_state)
                                    .await
                                    .unwrap_or(None);
                                if let Some(ref now_playing) = state {
                                    let serialized = now_playing.serialize();
                                    let _ = ble_dispatch
                                        .send_packet(PacketType::MediaState, &serialized, &raw_ble_sender)
                                        .await;
                                }
                                last_media_state = state;
                            }
                        }
                        PacketType::ScreenCast => {
                            let mut receiver = screen_rcvr_shared.lock().await;
                            let (header_opt, nal_data) = if packet.payload.len() >= 27 {
                                if let Ok(header) = VideoFrameHeader::deserialize(&packet.payload[..27]) {
                                    (Some(header), &packet.payload[27..])
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
                            let (width, height) = header_opt
                                .as_ref()
                                .map(|h| (h.width, h.height))
                                .unwrap_or((0, 0));
                            receiver.submit_hevc_packet(nal_data, pts, is_kf, width, height);
                            let direct_gpu = screen_viewer
                                .as_ref()
                                .is_some_and(ScreenViewerWindow::gpu_presentation_enabled);
                            let decoded_frame = if screen_viewer.is_some() {
                                tokio::task::block_in_place(|| {
                                    receiver.decode_hevc_packet(
                                        nal_data,
                                        pts,
                                        width,
                                        height,
                                        direct_gpu,
                                    )
                                })
                            } else {
                                None
                            };
                            let (frames, keyframes, total_bytes) = receiver.stats();
                            drop(receiver);
                            if let Some(viewer) = screen_viewer.as_ref() {
                                viewer.update_remote_dimensions(width, height);
                                if let Some(frame) = decoded_frame {
                                    viewer.present_frame(frame);
                                }
                            }
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
                                let token_matches = active_bulk_token.as_deref()
                                    == Some(wake.session_token.as_str());
                                if wake.reason != "ready" || !token_matches || !waiting_for_bulk_ack {
                                    tracing::warn!("Ignoring stale or unexpected Wi-Fi bulk response");
                                    continue;
                                }
                                let Ok(ip) = wake.target_ip.parse::<IpAddr>() else {
                                    tracing::warn!("Android returned an invalid bulk-transfer address");
                                    continue;
                                };
                                let addr = SocketAddr::new(ip, wake.port);
                                let Some(request) = pending_cloud_fetches.pop_front() else {
                                    tracing::warn!("Android announced a bulk socket with no pending file request");
                                    continue;
                                };
                                let identity = request.identity();
                                if waiting_for_fetch != Some(identity) {
                                    tracing::warn!("Android bulk response did not match the queued Cloud Files request");
                                    let drive = cloud_drive_dispatch.lock().await;
                                    let _ = drive.fail_fetch(&request);
                                    active_bulk_token = None;
                                    waiting_for_bulk_ack = false;
                                    waiting_for_fetch = None;
                                    continue;
                                }
                                let token = wake.session_token;
                                waiting_for_bulk_ack = false;
                                waiting_for_fetch = None;
                                tracing::info!("Android bulk-transfer socket is ready");
                                ui_for_events.log("⚡ Phone photo transfer is ready");
                                let (cancel_sender, cancel_receiver) = oneshot::channel();
                                active_fetch_cancellations
                                    .insert(identity, (request.clone(), cancel_sender));
                                let completion_sender = bulk_event_tx.clone();
                                let drive = Arc::clone(&cloud_drive_dispatch);
                                tokio::spawn(async move {
                                    let Some(transfer_result) = fetch_bulk_range_cancellable(
                                        addr,
                                        &request,
                                        &token,
                                        cancel_receiver,
                                    ).await else {
                                        let _ = completion_sender.send(
                                            BulkSessionEvent::Cancelled(token.clone(), identity),
                                        );
                                        return;
                                    };

                                    let drive = drive.lock().await;
                                    match transfer_result {
                                        Ok((file_size, data))
                                            if request.offset >= 0
                                                && request.offset as u64 <= file_size
                                                && data.len() == request.length as usize
                                                && (request.offset as u64).saturating_add(data.len() as u64) <= file_size =>
                                        {
                                            if let Err(error) = drive.complete_fetch(&request, &data) {
                                                tracing::warn!("Could not transfer fetched media into Cloud Files: {error}");
                                                let _ = drive.fail_fetch(&request);
                                            }
                                        }
                                        Ok(_) => {
                                            tracing::warn!("Android returned a short or out-of-range media response");
                                            let _ = drive.fail_fetch(&request);
                                        }
                                        Err(error) => {
                                            tracing::warn!("Phone photo transfer failed: {error}");
                                            let _ = drive.fail_fetch(&request);
                                        }
                                    }
                                    let _ = completion_sender.send(BulkSessionEvent::Completed(token, identity));
                                });
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

    // Native startup can fault in additional pages just after the initial setup.
    // Give that cold-start activity time to settle, then request one idle trim.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        unsafe {
            let process_handle = GetCurrentProcess();
            let min_size = 4 * 1024 * 1024;
            let max_size = 14 * 1024 * 1024;
            if let Err(error) = SetProcessWorkingSetSize(process_handle, min_size, max_size) {
                tracing::warn!("Could not apply the preferred working-set range: {error}");
            }
        }
    });

    tracing::info!("ZeroLink Daemon is active with UI dashboard");

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

fn resolve_bind_ip(value: Option<&str>) -> std::io::Result<IpAddr> {
    match value {
        Some(value) => value.parse().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid ZEROLINK_BIND_ADDR '{value}': {error}"),
            )
        }),
        None => Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    }
}

fn resolve_bulk_port(value: Option<&str>) -> u16 {
    value
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(EphemeralBulkClient::DEFAULT_PORT)
}

fn try_acquire_tcp_session(slot: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    Arc::clone(slot).try_acquire_owned().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerolink_core::crypto::KeyExchange;
    use zerolink_core::protocol::{ClipboardType, HandshakeInitPayload, HandshakeReplyPayload};

    #[test]
    fn tcp_listener_defaults_to_all_interfaces_and_accepts_loopback_override() {
        assert_eq!(
            resolve_bind_ip(None).unwrap(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            resolve_bind_ip(Some("127.0.0.1")).unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert!(resolve_bind_ip(Some("not-an-ip")).is_err());
    }

    #[test]
    fn bulk_transfer_port_defaults_and_accepts_only_nonzero_u16_values() {
        assert_eq!(resolve_bulk_port(None), EphemeralBulkClient::DEFAULT_PORT);
        assert_eq!(resolve_bulk_port(Some("18443")), 18443);
        assert_eq!(
            resolve_bulk_port(Some("0")),
            EphemeralBulkClient::DEFAULT_PORT
        );
        assert_eq!(
            resolve_bulk_port(Some("65536")),
            EphemeralBulkClient::DEFAULT_PORT
        );
        assert_eq!(
            resolve_bulk_port(Some("invalid")),
            EphemeralBulkClient::DEFAULT_PORT
        );
    }

    #[test]
    fn cancelled_cloud_fetch_is_removed_only_when_its_full_required_range_is_unneeded() {
        let request = CloudFetchRequest {
            connection_key: 10,
            transfer_key: 20,
            request_key: 30,
            file_id: "synthetic-media-id".into(),
            required_offset: 4096,
            required_length: 2048,
            offset: 0,
            length: 8192,
        };
        let identity = request.identity();
        let mut pending = VecDeque::from([request.clone()]);
        let partial = CloudFetchCancellation {
            identity,
            offset: 4096,
            length: 1024,
        };

        assert!(remove_fully_cancelled_fetches(&mut pending, &partial).is_empty());
        assert_eq!(pending.len(), 1);

        let full = CloudFetchCancellation {
            identity,
            offset: 0,
            length: 8192,
        };
        assert_eq!(
            remove_fully_cancelled_fetches(&mut pending, &full),
            vec![identity]
        );
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn cancelling_an_inflight_bulk_fetch_closes_the_socket_without_completing_data() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::time::timeout;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_received_sender, request_received_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&[0xA6; 32]).await.unwrap();
            let mut nonce = [0u8; 12];
            stream.read_exact(&mut nonce).await.unwrap();
            let mut length = [0u8; 4];
            stream.read_exact(&mut length).await.unwrap();
            let mut ciphertext = vec![0u8; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut ciphertext).await.unwrap();
            request_received_sender.send(()).unwrap();

            let mut byte = [0u8; 1];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        let request = CloudFetchRequest {
            connection_key: 1,
            transfer_key: 2,
            request_key: 3,
            file_id: "synthetic-media-id".into(),
            required_offset: 0,
            required_length: 4096,
            offset: 0,
            length: 4096,
        };
        let token = "42".repeat(32);
        let (cancel_sender, cancel_receiver) = oneshot::channel();
        let fetch = tokio::spawn(async move {
            fetch_bulk_range_cancellable(address, &request, &token, cancel_receiver).await
        });

        timeout(Duration::from_secs(3), request_received_receiver)
            .await
            .expect("client should send the encrypted request")
            .expect("server request signal");
        cancel_sender.send(()).unwrap();
        assert!(timeout(Duration::from_secs(3), fetch)
            .await
            .expect("cancelled transfer should stop promptly")
            .unwrap()
            .is_none());
        timeout(Duration::from_secs(3), server)
            .await
            .expect("server should observe the closed bulk socket")
            .unwrap();
    }

    #[test]
    fn tcp_session_slot_rejects_overlapping_peers_and_reopens_after_disconnect() {
        let slot = Arc::new(Semaphore::new(1));

        let first_session = try_acquire_tcp_session(&slot).unwrap();
        assert!(try_acquire_tcp_session(&slot).is_none());

        drop(first_session);
        assert!(try_acquire_tcp_session(&slot).is_some());
    }

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
        assert_eq!(
            init_payload.protocol_version,
            zerolink_core::protocol::HANDSHAKE_PROTOCOL_VERSION
        );

        // 2. Simulate Android server generating reply
        let android_kx = KeyExchange::new();
        let android_public_key = android_kx.public_key;
        let server_cipher = android_kx
            .complete_handshake(
                &init_payload.client_ephemeral_pubkey,
                &init_payload.salt,
                zerolink_core::crypto::SessionRole::Responder,
            )
            .unwrap();
        let mut transcript = b"ZeroLink-v3-server-key-confirmation".to_vec();
        transcript.extend_from_slice(&init_payload.client_ephemeral_pubkey);
        transcript.extend_from_slice(&init_payload.salt);
        transcript.extend_from_slice(&android_public_key);
        let reply = HandshakeReplyPayload {
            server_ephemeral_pubkey: android_public_key,
            auth_tag: server_cipher.key_confirmation_tag(&transcript),
            l2cap_psm: 0x0084,
            server_device_name: "Pixel 8 Pro (ZeroLink)".to_string(),
            pairing_auth_tag: [0u8; 16],
        };

        let server_device_name = ble_client
            .complete_handshake(&reply.serialize(), &raw_tx)
            .await
            .unwrap();
        assert_eq!(server_device_name, "Pixel 8 Pro (ZeroLink)");
        assert!(ble_client.is_session_active().await);
        assert_eq!(ble_client.psm().await, 0x0084);
        let confirmation_raw = raw_rx.recv().await.unwrap();
        let (confirmation_frame, _) = Frame::try_parse(&confirmation_raw).unwrap().unwrap();
        assert_eq!(confirmation_frame.header.packet_type, PacketType::Handshake);
        assert_eq!(confirmation_frame.payload[0], 0x02);
        let mut client_transcript = b"ZeroLink-v3-client-key-confirmation".to_vec();
        client_transcript.extend_from_slice(&init_payload.client_ephemeral_pubkey);
        client_transcript.extend_from_slice(&init_payload.salt);
        client_transcript.extend_from_slice(&android_public_key);
        assert_eq!(
            confirmation_frame.payload[1..],
            server_cipher.key_confirmation_tag(&client_transcript)
        );

        let clip = ClipboardPayload {
            format: ClipboardType::PlainText,
            payload_hash: 0xABCDEF0123456789,
            timestamp_ms: 1700000000000,
            data: b"Clipboard text from Windows".to_vec(),
        };
        assert!(ble_client
            .send_packet(PacketType::Clipboard, &clip.serialize(), &raw_tx)
            .await
            .is_err());
        ble_client.mark_paired_for_test().await;

        // 3. Android completes its own handshake with client pubkey & salt
        let mut android_cipher = server_cipher;

        // 4. Test outbound packet (Windows -> Android)
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
        receiver.submit_hevc_packet(&vps_nal, 1000, false, 1080, 2400);

        let idr_nal = [0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x99];
        receiver.submit_hevc_packet(&idr_nal, 2000, true, 1080, 2400);

        let trail_nal = [0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0x88, 0x77];
        receiver.submit_hevc_packet(&trail_nal, 3000, false, 1080, 2400);

        let (frames, keyframes, total_bytes) = receiver.stats();
        assert_eq!(frames, 3);
        assert_eq!(keyframes, 1);
        assert_eq!(total_bytes, 24);
        assert_eq!(receiver.dimensions(), (1080, 2400));
    }
}
