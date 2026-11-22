use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_VOLUME_DOWN, VK_VOLUME_UP,
};
use zerolink_core::protocol::{MediaCommand, MediaStatePayload};

/// Queries Windows Global System Media Transport Controls (GSMTC) for the
/// currently active media session. Returns `None` when no app is publishing
/// a media session.
pub fn query_state() -> Option<MediaStatePayload> {
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .ok()?
        .get()
        .ok()?;
    let session = manager.GetCurrentSession().ok()?;

    let properties = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let title = properties
        .Title()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let artist = properties
        .Artist()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let album = properties
        .AlbumTitle()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let app_name = session
        .SourceAppUserModelId()
        .map(|s| s.to_string())
        .unwrap_or_default();

    let is_playing = session
        .GetPlaybackInfo()
        .ok()
        .and_then(|info| info.PlaybackStatus().ok())
        .map(|status| status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
        .unwrap_or(false);

    let (position_ms, duration_ms) = session
        .GetTimelineProperties()
        .ok()
        .map(|timeline| {
            let position = timeline
                .Position()
                .map(|t| (t.Duration / 10_000).max(0) as u64)
                .unwrap_or(0);
            let duration = timeline
                .EndTime()
                .map(|t| (t.Duration / 10_000).max(0) as u64)
                .unwrap_or(0);
            (position, duration)
        })
        .unwrap_or((0, 0));

    if title.is_empty() && artist.is_empty() && app_name.is_empty() {
        return None;
    }

    Some(MediaStatePayload {
        title,
        artist,
        album,
        app_name,
        is_playing,
        position_ms,
        duration_ms,
    })
}

/// Executes a media command against the active Windows media session.
/// Volume commands fall back to the system multimedia keys because GSMTC does
/// not expose a volume API.
pub fn execute_command(command: MediaCommand) -> bool {
    if matches!(command, MediaCommand::VolumeUp | MediaCommand::VolumeDown) {
        let key = if command == MediaCommand::VolumeUp {
            VK_VOLUME_UP
        } else {
            VK_VOLUME_DOWN
        };
        unsafe {
            keybd_event(key.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(key.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        return true;
    }

    let Ok(manager) =
        GlobalSystemMediaTransportControlsSessionManager::RequestAsync().and_then(|op| op.get())
    else {
        return false;
    };
    let Ok(session) = manager.GetCurrentSession() else {
        return false;
    };

    let result = match command {
        MediaCommand::Play => session.TryPlayAsync(),
        MediaCommand::Pause => session.TryPauseAsync(),
        MediaCommand::TogglePlayPause => session.TryTogglePlayPauseAsync(),
        MediaCommand::Next => session.TrySkipNextAsync(),
        MediaCommand::Previous => session.TrySkipPreviousAsync(),
        MediaCommand::VolumeUp | MediaCommand::VolumeDown => unreachable!(),
    };

    result.and_then(|op| op.get()).unwrap_or(false)
}
