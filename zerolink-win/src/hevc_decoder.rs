use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIBuffer, IMFMediaEventGenerator, IMFMediaType, IMFTransform, MEError,
    METransformHaveOutput, METransformNeedInput, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFShutdown, MFStartup, MFTEnumEx,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_DECODER,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_EVENT_MFT_INPUT_STREAM_ID,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;

struct MfSession;

impl MfSession {
    fn start() -> Result<Self, String> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| e.to_string())?;
        }
        Ok(Self)
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Top-down BGRA pixels for the native Win32 viewer.
    pub bgra: Vec<u8>,
    /// Optional decoder surface for direct D3D11 presentation.
    pub surface: Option<DecodedSurface>,
}

pub struct DecodedSurface {
    pub texture: ID3D11Texture2D,
    pub mip_slice: u32,
    pub array_slice: u32,
}

// D3D resources are retained across the decoder and the viewer's MTA UI thread.
unsafe impl Send for DecodedSurface {}
unsafe impl Send for DecodedFrame {}

const MAX_FRAME_PIXELS: u64 = 16_777_216;

pub struct HevcDecoder {
    hardware: Option<HardwareHevcDecoder>,
    software: Option<SoftwareHevcDecoder>,
    _session: MfSession,
}

// Media Foundation is created in the process MTA and every call made from the
// asynchronous packet loop is guarded by a matching MTA COM initialization.
unsafe impl Send for HevcDecoder {}

impl HevcDecoder {
    pub fn new(
        device: Option<ID3D11Device>,
        context: Option<ID3D11DeviceContext>,
    ) -> Result<Self, String> {
        let session = MfSession::start()?;
        let hardware = match (device, context) {
            (Some(device), Some(context)) => match HardwareHevcDecoder::new(device, context) {
                Ok(decoder) => {
                    tracing::info!("Media Foundation D3D11 hardware HEVC decoder selected");
                    Some(decoder)
                }
                Err(error) => {
                    tracing::info!(
                        "D3D11 hardware HEVC decoder unavailable; using software: {error}"
                    );
                    None
                }
            },
            _ => None,
        };
        let software = match SoftwareHevcDecoder::new() {
            Ok(decoder) => Some(decoder),
            Err(error) => {
                tracing::warn!("Synchronous Media Foundation HEVC decoder unavailable: {error}");
                None
            }
        };
        if hardware.is_none() && software.is_none() {
            return Err("No usable Media Foundation HEVC decoder was found".into());
        }
        Ok(Self {
            hardware,
            software,
            _session: session,
        })
    }

    pub fn decode(
        &mut self,
        annex_b: &[u8],
        pts_microseconds: u64,
        input_width: u16,
        input_height: u16,
        direct_gpu: bool,
    ) -> Result<Option<DecodedFrame>, String> {
        if let Some(hardware) = self.hardware.as_mut() {
            match hardware.decode(
                annex_b,
                pts_microseconds,
                input_width,
                input_height,
                direct_gpu,
            ) {
                Ok(frame) => return Ok(frame),
                Err(error) => {
                    tracing::warn!(
                        "D3D11 hardware HEVC decoder failed; switching to software: {error}"
                    );
                    self.hardware = None;
                }
            }
        }
        self.software
            .as_mut()
            .ok_or_else(|| "No software HEVC fallback is available".to_string())?
            .decode(annex_b, pts_microseconds, input_width, input_height)
    }
}

struct SoftwareHevcDecoder {
    transform: IMFTransform,
    output_info: Option<windows::Win32::Media::MediaFoundation::MFT_OUTPUT_STREAM_INFO>,
    stream_started: bool,
}

impl SoftwareHevcDecoder {
    fn new() -> Result<Self, String> {
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_HEVC,
            };
            let mut activations: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut activation_count = 0u32;
            if let Err(error) = MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                None,
                &mut activations,
                &mut activation_count,
            ) {
                return Err(format!(
                    "Could not enumerate Media Foundation HEVC decoders: {error}"
                ));
            }

            let mut activation = None;
            for index in 0..activation_count as usize {
                if let Some(item) = (*activations.add(index)).take() {
                    if activation.is_none() {
                        activation = Some(item);
                    }
                }
            }
            if !activations.is_null() {
                CoTaskMemFree(Some(activations.cast()));
            }
            let activation = match activation {
                Some(value) => value,
                None => {
                    return Err("No installed Media Foundation HEVC decoder was found".into());
                }
            };
            let transform: IMFTransform = match activation.ActivateObject() {
                Ok(value) => value,
                Err(error) => {
                    return Err(format!("Could not activate HEVC decoder: {error}"));
                }
            };

            Ok(Self {
                transform,
                output_info: None,
                stream_started: false,
            })
        }
    }

    /// Decodes one Annex B access unit. The Windows HEVC MFT reads dimensions from SPS/PPS;
    /// the transmitted dimensions are used to size caller-provided output samples.
    pub fn decode(
        &mut self,
        annex_b: &[u8],
        pts_microseconds: u64,
        input_width: u16,
        input_height: u16,
    ) -> Result<Option<DecodedFrame>, String> {
        if annex_b.is_empty() {
            return Ok(None);
        }
        if input_width > 8192
            || input_height > 8192
            || (input_width > 0
                && input_height > 0
                && input_width as u64 * input_height as u64 > MAX_FRAME_PIXELS)
        {
            return Ok(None);
        }
        unsafe {
            if !self.stream_started {
                self.initialize_stream(input_width, input_height)?;
            }
            let input_buffer =
                MFCreateMemoryBuffer(annex_b.len() as u32).map_err(|e| e.to_string())?;
            let mut data = std::ptr::null_mut();
            input_buffer
                .Lock(&mut data, None, None)
                .map_err(|e| e.to_string())?;
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), data, annex_b.len());
            input_buffer.Unlock().map_err(|e| e.to_string())?;
            input_buffer
                .SetCurrentLength(annex_b.len() as u32)
                .map_err(|e| e.to_string())?;

            let input_sample = MFCreateSample().map_err(|e| e.to_string())?;
            input_sample
                .AddBuffer(&input_buffer)
                .map_err(|e| e.to_string())?;
            input_sample
                .SetSampleTime((pts_microseconds.saturating_mul(10).min(i64::MAX as u64)) as i64)
                .map_err(|e| e.to_string())?;
            input_sample
                .SetSampleDuration(333_333)
                .map_err(|e| e.to_string())?;

            let pending = match self.transform.ProcessInput(0, &input_sample, 0) {
                Ok(()) => None,
                Err(error)
                    if error.code()
                        == windows::Win32::Media::MediaFoundation::MF_E_NOTACCEPTING =>
                {
                    let frame = self.drain_output(input_width, input_height)?;
                    self.transform
                        .ProcessInput(0, &input_sample, 0)
                        .map_err(|e| {
                            format!(
                                "HEVC decoder rejected an access unit after draining output: {e}"
                            )
                        })?;
                    frame
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(error) => return Err(format!("HEVC decoder rejected an access unit: {error}")),
            };

            Ok(self.drain_output(input_width, input_height)?.or(pending))
        }
    }

    unsafe fn initialize_stream(&mut self, width: u16, height: u16) -> Result<(), String> {
        let input_type = MFCreateMediaType().map_err(|e| e.to_string())?;
        input_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| e.to_string())?;
        input_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)
            .map_err(|e| e.to_string())?;
        if width > 0 && height > 0 {
            input_type
                .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                .map_err(|e| e.to_string())?;
        }
        self.transform
            .SetInputType(0, &input_type, 0)
            .map_err(|e| format!("HEVC decoder rejected Annex B input: {e}"))?;

        set_nv12_output_type(&self.transform)?;
        self.transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|e| e.to_string())?;
        self.transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|e| e.to_string())?;
        self.output_info = Some(
            self.transform
                .GetOutputStreamInfo(0)
                .map_err(|e| e.to_string())?,
        );
        self.stream_started = true;
        Ok(())
    }

    unsafe fn drain_output(
        &mut self,
        input_width: u16,
        input_height: u16,
    ) -> Result<Option<DecodedFrame>, String> {
        for _ in 0..8 {
            let output_info = self
                .output_info
                .ok_or_else(|| "HEVC stream is not initialized".to_string())?;
            let provides_samples =
                output_info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let supplied_sample = if provides_samples {
                None
            } else {
                let sample = MFCreateSample().map_err(|e| e.to_string())?;
                let suggested = output_info.cbSize.max(
                    (input_width as u32)
                        .saturating_mul(input_height as u32)
                        .saturating_mul(2),
                );
                let buffer =
                    MFCreateMemoryBuffer(suggested.max(4096)).map_err(|e| e.to_string())?;
                sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
                Some(sample)
            };

            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(supplied_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            match self.transform.ProcessOutput(0, &mut output, &mut status) {
                Ok(()) => {
                    let sample = ManuallyDrop::take(&mut output[0].pSample);
                    let events = ManuallyDrop::take(&mut output[0].pEvents);
                    let frame = match sample {
                        Some(sample) => self.sample_to_frame(&sample)?,
                        None => None,
                    };
                    drop(events);
                    if frame.is_some() {
                        return Ok(frame);
                    }
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    drop(ManuallyDrop::take(&mut output[0].pSample));
                    drop(ManuallyDrop::take(&mut output[0].pEvents));
                    return Ok(None);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    drop(ManuallyDrop::take(&mut output[0].pSample));
                    drop(ManuallyDrop::take(&mut output[0].pEvents));
                    set_nv12_output_type(&self.transform)?;
                    self.output_info = Some(
                        self.transform
                            .GetOutputStreamInfo(0)
                            .map_err(|e| e.to_string())?,
                    );
                }
                Err(error) => {
                    drop(ManuallyDrop::take(&mut output[0].pSample));
                    drop(ManuallyDrop::take(&mut output[0].pEvents));
                    return Err(format!("HEVC decoder output failed: {error}"));
                }
            }
        }
        Ok(None)
    }

    unsafe fn sample_to_frame(
        &self,
        sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    ) -> Result<Option<DecodedFrame>, String> {
        sample_to_frame_with_d3d(&self.transform, sample, None)
    }
}

impl Drop for SoftwareHevcDecoder {
    fn drop(&mut self) {
        unsafe {
            if self.stream_started {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
        }
    }
}

struct HardwareHevcDecoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    device_manager: windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager,
    pending_input: VecDeque<windows::Win32::Media::MediaFoundation::IMFSample>,
    input_credits: usize,
    output_credits: usize,
    output_info: Option<windows::Win32::Media::MediaFoundation::MFT_OUTPUT_STREAM_INFO>,
    stream_started: bool,
}

impl HardwareHevcDecoder {
    fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Result<Self, String> {
        unsafe {
            let mut reset_token = 0u32;
            let mut device_manager = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)
                .map_err(|e| format!("Could not create a DXGI device manager: {e}"))?;
            let device_manager = device_manager.ok_or_else(|| {
                "Media Foundation returned an empty DXGI device manager".to_string()
            })?;
            device_manager
                .ResetDevice(&device, reset_token)
                .map_err(|e| format!("Could not attach D3D11 to Media Foundation: {e}"))?;

            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_HEVC,
            };
            let mut activations: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut activation_count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                None,
                &mut activations,
                &mut activation_count,
            )
            .map_err(|e| format!("Could not enumerate D3D11 HEVC decoders: {e}"))?;

            let mut selected = None;
            for index in 0..activation_count as usize {
                if let Some(activation) = (*activations.add(index)).take() {
                    if selected.is_none() {
                        if let Ok(transform) = activation.ActivateObject::<IMFTransform>() {
                            if let Ok(attributes) = transform.GetAttributes() {
                                let asynchronous = attributes
                                    .GetUINT32(&MF_TRANSFORM_ASYNC)
                                    .unwrap_or_default()
                                    != 0;
                                let d3d11_aware =
                                    attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or_default()
                                        != 0;
                                if asynchronous && d3d11_aware {
                                    if let Err(error) =
                                        attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                                    {
                                        tracing::debug!(
                                            "Could not unlock async HEVC transform: {error}"
                                        );
                                    } else if let Ok(events) =
                                        transform.cast::<IMFMediaEventGenerator>()
                                    {
                                        selected = Some((transform, events));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !activations.is_null() {
                CoTaskMemFree(Some(activations.cast()));
            }
            let (transform, events) = selected.ok_or_else(|| {
                "No asynchronous D3D11-aware Media Foundation HEVC decoder is installed".to_string()
            })?;

            Ok(Self {
                transform,
                events,
                device,
                context,
                device_manager,
                pending_input: VecDeque::new(),
                input_credits: 0,
                output_credits: 0,
                output_info: None,
                stream_started: false,
            })
        }
    }

    fn decode(
        &mut self,
        annex_b: &[u8],
        pts_microseconds: u64,
        input_width: u16,
        input_height: u16,
        direct_gpu: bool,
    ) -> Result<Option<DecodedFrame>, String> {
        if annex_b.is_empty() {
            return Ok(None);
        }
        if input_width > 8192
            || input_height > 8192
            || (input_width > 0
                && input_height > 0
                && input_width as u64 * input_height as u64 > MAX_FRAME_PIXELS)
        {
            return Ok(None);
        }

        unsafe {
            if !self.stream_started {
                self.initialize_stream(input_width, input_height)?;
            }
            if self.pending_input.len() >= 8 {
                return Err("D3D11 HEVC decoder stopped requesting input".into());
            }
            self.pending_input
                .push_back(create_input_sample(annex_b, pts_microseconds)?);

            let mut latest_frame = None;
            for _ in 0..32 {
                let mut progress = self.collect_events()?;
                while self.input_credits > 0 {
                    let Some(sample) = self.pending_input.pop_front() else {
                        break;
                    };
                    self.transform
                        .ProcessInput(0, &sample, 0)
                        .map_err(|e| format!("Hardware HEVC decoder rejected input: {e}"))?;
                    self.input_credits -= 1;
                    progress = true;
                }
                while self.output_credits > 0 {
                    if let Some(frame) = self.process_one_output(direct_gpu)? {
                        latest_frame = Some(frame);
                    }
                    self.output_credits -= 1;
                    progress = true;
                }
                if !progress {
                    break;
                }
            }
            Ok(latest_frame)
        }
    }

    unsafe fn initialize_stream(&mut self, width: u16, height: u16) -> Result<(), String> {
        // Media Foundation requires the D3D manager before setting either media type.
        self.transform
            .ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                self.device_manager.as_raw() as usize,
            )
            .map_err(|e| format!("Hardware HEVC decoder rejected the D3D11 manager: {e}"))?;
        let input_type = MFCreateMediaType().map_err(|e| e.to_string())?;
        input_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| e.to_string())?;
        input_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)
            .map_err(|e| e.to_string())?;
        if width > 0 && height > 0 {
            input_type
                .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                .map_err(|e| e.to_string())?;
        }
        self.transform
            .SetInputType(0, &input_type, 0)
            .map_err(|e| format!("Hardware HEVC decoder rejected Annex B input: {e}"))?;
        set_nv12_output_type(&self.transform)?;
        self.transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|e| e.to_string())?;
        self.transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|e| e.to_string())?;
        self.output_info = Some(
            self.transform
                .GetOutputStreamInfo(0)
                .map_err(|e| e.to_string())?,
        );
        self.stream_started = true;
        Ok(())
    }

    unsafe fn collect_events(&mut self) -> Result<bool, String> {
        let mut received = false;
        loop {
            let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(format!("Hardware HEVC event read failed: {error}")),
            };
            received = true;
            let event_type = event.GetType().map_err(|e| e.to_string())?;
            if event_type == METransformNeedInput.0 as u32 {
                let stream_id = event
                    .GetUINT32(&MF_EVENT_MFT_INPUT_STREAM_ID)
                    .map_err(|e| format!("Hardware decoder omitted its input stream id: {e}"))?;
                if stream_id != 0 {
                    return Err(format!("Unsupported HEVC input stream id {stream_id}"));
                }
                self.input_credits = self.input_credits.saturating_add(1).min(16);
            } else if event_type == METransformHaveOutput.0 as u32 {
                self.output_credits = self.output_credits.saturating_add(1).min(16);
            } else if event_type == MEError.0 as u32 {
                let status = event.GetStatus().err();
                return Err(format!(
                    "Hardware HEVC decoder reported an asynchronous error: {status:?}"
                ));
            } else if let Err(error) = event.GetStatus() {
                return Err(format!("Hardware HEVC decoder event failed: {error}"));
            }
        }
        Ok(received)
    }

    unsafe fn process_one_output(
        &mut self,
        direct_gpu: bool,
    ) -> Result<Option<DecodedFrame>, String> {
        let output_info = self
            .output_info
            .ok_or_else(|| "D3D11 HEVC stream is not initialized".to_string())?;
        let provides_samples =
            output_info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        let supplied_sample = if provides_samples {
            None
        } else {
            let sample = MFCreateSample().map_err(|e| e.to_string())?;
            let required_size = self
                .transform
                .GetOutputCurrentType(0)
                .ok()
                .and_then(|media_type| {
                    let dimensions = media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
                    let width = (dimensions >> 32) as u32 as usize;
                    let height = dimensions as u32 as usize;
                    let stride = media_type
                        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                        .map(|value| (value as i32).unsigned_abs() as usize)
                        .unwrap_or(width)
                        .max(width)
                        .max((width + 1) & !1);
                    stride.checked_mul(height + height.div_ceil(2))
                })
                .and_then(|size| u32::try_from(size).ok())
                .unwrap_or(0);
            let size = output_info.cbSize.max(required_size).max(4096);
            let buffer = MFCreateMemoryBuffer(size).map_err(|e| e.to_string())?;
            sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
            Some(sample)
        };
        let mut output = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(supplied_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        match self.transform.ProcessOutput(0, &mut output, &mut status) {
            Ok(()) => {
                let sample = ManuallyDrop::take(&mut output[0].pSample);
                let events = ManuallyDrop::take(&mut output[0].pEvents);
                let frame = match sample {
                    Some(sample) if direct_gpu => {
                        match sample_to_gpu_frame(&self.transform, &sample) {
                            Ok(Some(frame)) => Some(frame),
                            _ => sample_to_frame_with_d3d(
                                &self.transform,
                                &sample,
                                Some((&self.device, &self.context)),
                            )?,
                        }
                    }
                    Some(sample) => sample_to_frame_with_d3d(
                        &self.transform,
                        &sample,
                        Some((&self.device, &self.context)),
                    )?,
                    None => None,
                };
                drop(events);
                Ok(frame)
            }
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                drop(ManuallyDrop::take(&mut output[0].pSample));
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                set_nv12_output_type(&self.transform)?;
                self.output_info = Some(
                    self.transform
                        .GetOutputStreamInfo(0)
                        .map_err(|e| e.to_string())?,
                );
                Ok(None)
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                drop(ManuallyDrop::take(&mut output[0].pSample));
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                Ok(None)
            }
            Err(error) => {
                drop(ManuallyDrop::take(&mut output[0].pSample));
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                Err(format!("Hardware HEVC output failed: {error}"))
            }
        }
    }
}

impl Drop for HardwareHevcDecoder {
    fn drop(&mut self) {
        unsafe {
            if self.stream_started {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
    }
}

unsafe fn set_nv12_output_type(transform: &IMFTransform) -> Result<(), String> {
    for index in 0..32 {
        let media_type: IMFMediaType = match transform.GetOutputAvailableType(0, index) {
            Ok(value) => value,
            Err(_) => break,
        };
        if media_type.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
            transform
                .SetOutputType(0, &media_type, 0)
                .map_err(|e| format!("HEVC decoder rejected NV12 output: {e}"))?;
            return Ok(());
        }
    }
    Err("Installed HEVC decoder does not offer NV12 output".into())
}

unsafe fn create_input_sample(
    annex_b: &[u8],
    pts_microseconds: u64,
) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, String> {
    if annex_b.len() > u32::MAX as usize {
        return Err("HEVC access unit exceeds Media Foundation's sample limit".into());
    }
    let input_buffer = MFCreateMemoryBuffer(annex_b.len() as u32).map_err(|e| e.to_string())?;
    let mut data = std::ptr::null_mut();
    input_buffer
        .Lock(&mut data, None, None)
        .map_err(|e| e.to_string())?;
    std::ptr::copy_nonoverlapping(annex_b.as_ptr(), data, annex_b.len());
    input_buffer.Unlock().map_err(|e| e.to_string())?;
    input_buffer
        .SetCurrentLength(annex_b.len() as u32)
        .map_err(|e| e.to_string())?;

    let sample = MFCreateSample().map_err(|e| e.to_string())?;
    sample.AddBuffer(&input_buffer).map_err(|e| e.to_string())?;
    sample
        .SetSampleTime((pts_microseconds.saturating_mul(10).min(i64::MAX as u64)) as i64)
        .map_err(|e| e.to_string())?;
    sample
        .SetSampleDuration(333_333)
        .map_err(|e| e.to_string())?;
    Ok(sample)
}

unsafe fn sample_to_frame_with_d3d(
    transform: &IMFTransform,
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    d3d: Option<(&ID3D11Device, &ID3D11DeviceContext)>,
) -> Result<Option<DecodedFrame>, String> {
    let media_type = match transform.GetOutputCurrentType(0) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let dimensions = media_type
        .GetUINT64(&MF_MT_FRAME_SIZE)
        .map_err(|e| e.to_string())?;
    let width = (dimensions >> 32) as u32;
    let height = dimensions as u32;
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || width as u64 * height as u64 > MAX_FRAME_PIXELS
    {
        return Ok(None);
    }

    let buffer = match sample.ConvertToContiguousBuffer() {
        Ok(value) => value,
        Err(error) => {
            return match d3d {
                Some((device, context)) => read_dxgi_nv12(sample, device, context, width, height),
                None => Err(format!("Could not access decoded HEVC sample: {error}")),
            };
        }
    };
    let mut data = std::ptr::null_mut();
    if let Err(error) = buffer.Lock(&mut data, None, None) {
        return match d3d {
            Some((device, context)) => read_dxgi_nv12(sample, device, context, width, height),
            None => Err(format!("Could not lock decoded HEVC sample: {error}")),
        };
    }
    let length = match buffer.GetCurrentLength() {
        Ok(length) => length as usize,
        Err(error) => {
            let _ = buffer.Unlock();
            return Err(error.to_string());
        }
    };
    let stride = media_type
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|value| (value as i32).unsigned_abs() as usize)
        .unwrap_or(width as usize)
        .max(width as usize)
        .max((width as usize + 1) & !1);
    let y_bytes = stride.saturating_mul(height as usize);
    let required = y_bytes.saturating_add(stride.saturating_mul((height as usize).div_ceil(2)));
    if data.is_null() || length < required {
        let _ = buffer.Unlock();
        return Ok(None);
    }
    let nv12 = std::slice::from_raw_parts(data, length);
    let bgra = nv12_to_bgra(nv12, width as usize, height as usize, stride);
    buffer.Unlock().map_err(|e| e.to_string())?;
    Ok(Some(DecodedFrame {
        width,
        height,
        bgra,
        surface: None,
    }))
}

unsafe fn sample_to_gpu_frame(
    transform: &IMFTransform,
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
) -> Result<Option<DecodedFrame>, String> {
    let media_type = match transform.GetOutputCurrentType(0) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let dimensions = media_type
        .GetUINT64(&MF_MT_FRAME_SIZE)
        .map_err(|e| e.to_string())?;
    let width = (dimensions >> 32) as u32;
    let height = dimensions as u32;
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || width as u64 * height as u64 > MAX_FRAME_PIXELS
    {
        return Ok(None);
    }

    let buffer = sample
        .GetBufferByIndex(0)
        .map_err(|e| format!("Hardware output did not expose a DXGI buffer: {e}"))?;
    let dxgi_buffer: IMFDXGIBuffer = buffer
        .cast()
        .map_err(|e| format!("Hardware output was not a DXGI surface: {e}"))?;
    let subresource = dxgi_buffer
        .GetSubresourceIndex()
        .map_err(|e| format!("Could not read the decoder surface index: {e}"))?;
    let mut raw_texture = std::ptr::null_mut();
    dxgi_buffer
        .GetResource(&ID3D11Texture2D::IID, &mut raw_texture)
        .map_err(|e| format!("Could not get the decoded D3D11 texture: {e}"))?;
    if raw_texture.is_null() {
        return Err("Hardware decoder returned an empty D3D11 texture".into());
    }
    let texture = ID3D11Texture2D::from_raw(raw_texture);
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut desc);
    let mip_levels = desc.MipLevels.max(1);
    let mip_slice = subresource % mip_levels;
    let array_slice = subresource / mip_levels;
    if desc.Format != DXGI_FORMAT_NV12
        || desc.Width < width
        || desc.Height < height
        || desc.SampleDesc.Count != 1
        || array_slice >= desc.ArraySize
        || mip_slice != 0
    {
        return Err(format!(
            "Unsupported hardware HEVC surface: {:?} {}x{}, slice {}, mip {}",
            desc.Format, desc.Width, desc.Height, array_slice, mip_slice
        ));
    }

    Ok(Some(DecodedFrame {
        width,
        height,
        bgra: Vec::new(),
        surface: Some(DecodedSurface {
            texture,
            mip_slice,
            array_slice,
        }),
    }))
}

unsafe fn read_dxgi_nv12(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    width: u32,
    height: u32,
) -> Result<Option<DecodedFrame>, String> {
    let buffer = sample
        .GetBufferByIndex(0)
        .map_err(|e| format!("Hardware output did not expose a DXGI buffer: {e}"))?;
    let dxgi_buffer: IMFDXGIBuffer = buffer
        .cast()
        .map_err(|e| format!("Hardware output was not a DXGI surface: {e}"))?;
    let mut raw_texture = std::ptr::null_mut();
    dxgi_buffer
        .GetResource(&ID3D11Texture2D::IID, &mut raw_texture)
        .map_err(|e| format!("Could not get the decoded D3D11 texture: {e}"))?;
    if raw_texture.is_null() {
        return Err("Hardware decoder returned an empty D3D11 texture".into());
    }
    let texture = ID3D11Texture2D::from_raw(raw_texture);
    let mut staging_desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut staging_desc);
    if staging_desc.Format != DXGI_FORMAT_NV12
        || staging_desc.Width < width
        || staging_desc.Height < height
        || staging_desc.SampleDesc.Count != 1
        || staging_desc.ArraySize != 1
    {
        return Err(format!(
            "Unsupported hardware HEVC surface: {:?} {}x{}",
            staging_desc.Format, staging_desc.Width, staging_desc.Height
        ));
    }
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;
    let mut staging = None;
    device
        .CreateTexture2D(&staging_desc, None, Some(&mut staging))
        .map_err(|e| format!("Could not create a D3D11 readback texture: {e}"))?;
    let staging = staging.ok_or_else(|| "D3D11 did not create a readback texture".to_string())?;
    let staging_resource: ID3D11Resource = staging
        .cast()
        .map_err(|e| format!("Could not access the readback resource: {e}"))?;
    let source_resource: ID3D11Resource = texture
        .cast()
        .map_err(|e| format!("Could not access the decoded resource: {e}"))?;
    context.CopyResource(&staging_resource, &source_resource);

    let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
    context
        .Map(&staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .map_err(|e| format!("Could not map the hardware decoder output: {e}"))?;
    let row_pitch = mapped.RowPitch as usize;
    let source_width = staging_desc.Width as usize;
    let source_height = staging_desc.Height as usize;
    let output_stride = (width as usize + 1) & !1;
    if mapped.pData.is_null() || row_pitch < source_width || source_width < output_stride {
        context.Unmap(&staging_resource, 0);
        return Err("D3D11 returned an invalid NV12 row pitch".into());
    }
    let source_row_count = source_height + source_height.div_ceil(2);
    let Some(source_bytes) = row_pitch.checked_mul(source_row_count) else {
        context.Unmap(&staging_resource, 0);
        return Err("D3D11 NV12 readback size overflowed".into());
    };
    if source_bytes > (MAX_FRAME_PIXELS as usize * 3 / 2) + (row_pitch * 64) {
        context.Unmap(&staging_resource, 0);
        return Err("D3D11 NV12 readback exceeded the frame limit".into());
    }
    let output_height = height as usize;
    let uv_rows = output_height.div_ceil(2);
    let mut nv12 = vec![0u8; output_stride * (output_height + uv_rows)];
    let source = mapped.pData.cast::<u8>();
    for row in 0..output_height {
        std::ptr::copy_nonoverlapping(
            source.add(row * row_pitch),
            nv12.as_mut_ptr().add(row * output_stride),
            output_stride,
        );
    }
    let source_uv = source.add(row_pitch * source_height);
    let output_uv = nv12.as_mut_ptr().add(output_stride * output_height);
    for row in 0..uv_rows {
        std::ptr::copy_nonoverlapping(
            source_uv.add(row * row_pitch),
            output_uv.add(row * output_stride),
            output_stride,
        );
    }
    context.Unmap(&staging_resource, 0);
    let bgra = nv12_to_bgra(&nv12, width as usize, height as usize, output_stride);
    Ok(Some(DecodedFrame {
        width,
        height,
        bgra,
        surface: None,
    }))
}

fn nv12_to_bgra(nv12: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let y_plane_size = stride * height;
    let mut bgra = vec![0u8; width * height * 4];
    for y in 0..height {
        let y_row = y * stride;
        let uv_row = y_plane_size + (y / 2) * stride;
        for x in 0..width {
            let luminance = nv12[y_row + x] as i32 - 16;
            let chroma_index = uv_row + (x & !1);
            let u = nv12[chroma_index] as i32 - 128;
            let v = nv12[chroma_index + 1] as i32 - 128;
            let c = luminance.max(0);
            let r = (298 * c + 459 * v + 128) >> 8;
            let g = (298 * c - 55 * u - 136 * v + 128) >> 8;
            let b = (298 * c + 541 * u + 128) >> 8;
            let offset = (y * width + x) * 4;
            bgra[offset] = b.clamp(0, 255) as u8;
            bgra[offset + 1] = g.clamp(0, 255) as u8;
            bgra[offset + 2] = r.clamp(0, 255) as u8;
            bgra[offset + 3] = 255;
        }
    }
    bgra
}
