use std::mem::ManuallyDrop;

use crate::hevc_decoder::DecodedSurface;
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
    DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory, IDXGISwapChain, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

struct ProcessorState {
    input_width: u32,
    input_height: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: ID3D11VideoProcessorOutputView,
}

/// Presents Media Foundation NV12 decoder surfaces directly through D3D11.
pub struct D3dScreenPresenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    swap_chain: IDXGISwapChain,
    back_buffer: Option<ID3D11Texture2D>,
    render_target: Option<ID3D11RenderTargetView>,
    output_width: u32,
    output_height: u32,
    processor: Option<ProcessorState>,
}

impl D3dScreenPresenter {
    pub fn new(
        hwnd: HWND,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    ) -> Result<Self, String> {
        unsafe {
            let mut client = RECT::default();
            windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client)
                .map_err(|e| format!("Could not size the D3D11 screen viewer: {e}"))?;
            let output_width = (client.right - client.left).max(1) as u32;
            let output_height = (client.bottom - client.top).max(1) as u32;

            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|e| format!("Could not query the D3D11 DXGI device: {e}"))?;
            let adapter = dxgi_device
                .GetAdapter()
                .map_err(|e| format!("Could not get the D3D11 display adapter: {e}"))?;
            let factory: IDXGIFactory = adapter
                .GetParent()
                .map_err(|e| format!("Could not get the DXGI factory: {e}"))?;

            let swap_desc = DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Width: output_width,
                    Height: output_height,
                    RefreshRate: DXGI_RATIONAL {
                        Numerator: 60,
                        Denominator: 1,
                    },
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
                    Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                OutputWindow: hwnd,
                Windowed: BOOL(1),
                SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
                Flags: 0,
            };
            let mut swap_chain = None;
            factory
                .CreateSwapChain(&device, &swap_desc, &mut swap_chain)
                .ok()
                .map_err(|e| format!("Could not create the D3D11 viewer swap chain: {e}"))?;
            let swap_chain = swap_chain
                .ok_or_else(|| "DXGI returned an empty screen viewer swap chain".to_string())?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

            let video_device: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| format!("The D3D11 device has no video processor: {e}"))?;
            let video_context: ID3D11VideoContext = context
                .cast()
                .map_err(|e| format!("The D3D11 context has no video processor: {e}"))?;
            let (back_buffer, render_target) = create_back_buffer(&device, &swap_chain)?;

            Ok(Self {
                device,
                context,
                video_device,
                video_context,
                swap_chain,
                back_buffer: Some(back_buffer),
                render_target: Some(render_target),
                output_width,
                output_height,
                processor: None,
            })
        }
    }

    pub fn present(
        &mut self,
        hwnd: HWND,
        surface: &DecodedSurface,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        unsafe {
            self.resize_to_client(hwnd)?;
            self.ensure_processor(width, height)?;
            let state = self
                .processor
                .as_ref()
                .ok_or_else(|| "D3D11 screen video processor was not initialized".to_string())?;

            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: surface.mip_slice,
                        ArraySlice: surface.array_slice,
                    },
                },
                ..Default::default()
            };
            let mut input_view = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    &surface.texture,
                    &state.enumerator,
                    &desc,
                    Some(&mut input_view),
                )
                .map_err(|e| {
                    format!("Could not bind the HEVC NV12 surface for presentation: {e}")
                })?;
            let input_view =
                input_view.ok_or_else(|| "D3D11 returned an empty video input view".to_string())?;

            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                pInputSurface: ManuallyDrop::new(Some(input_view)),
                ..Default::default()
            };
            let (dest_rect, target_rect) =
                fitted_rect(self.output_width, self.output_height, width, height);
            if let Some(render_target) = self.render_target.as_ref() {
                self.context
                    .ClearRenderTargetView(render_target, &[0.0, 0.0, 0.0, 1.0]);
            }
            self.video_context.VideoProcessorSetOutputTargetRect(
                &state.processor,
                BOOL(1),
                Some(&target_rect),
            );
            self.video_context.VideoProcessorSetStreamSourceRect(
                &state.processor,
                0,
                BOOL(1),
                Some(&RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &state.processor,
                0,
                BOOL(1),
                Some(&dest_rect),
            );

            let blit_result = self.video_context.VideoProcessorBlt(
                &state.processor,
                &state.output_view,
                0,
                std::slice::from_ref(&stream),
            );
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            blit_result.map_err(|e| format!("D3D11 video presentation failed: {e}"))?;
            self.swap_chain
                .Present(1, DXGI_PRESENT(0))
                .ok()
                .map_err(|e| format!("Could not present the D3D11 screen frame: {e}"))
        }
    }

    unsafe fn resize_to_client(&mut self, hwnd: HWND) -> Result<(), String> {
        let mut client = RECT::default();
        windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client)
            .map_err(|e| e.to_string())?;
        let width = (client.right - client.left).max(1) as u32;
        let height = (client.bottom - client.top).max(1) as u32;
        if width == self.output_width && height == self.output_height {
            return Ok(());
        }

        self.processor = None;
        self.render_target = None;
        self.back_buffer = None;
        self.swap_chain
            .ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                Default::default(),
            )
            .map_err(|e| format!("Could not resize the D3D11 viewer: {e}"))?;
        let (back_buffer, render_target) = create_back_buffer(&self.device, &self.swap_chain)?;
        self.back_buffer = Some(back_buffer);
        self.render_target = Some(render_target);
        self.output_width = width;
        self.output_height = height;
        Ok(())
    }

    unsafe fn ensure_processor(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self
            .processor
            .as_ref()
            .is_some_and(|state| state.input_width == width && state.input_height == height)
        {
            return Ok(());
        }

        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: self.output_width,
            OutputHeight: self.output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = self
            .video_device
            .CreateVideoProcessorEnumerator(&content)
            .map_err(|e| format!("Could not create a D3D11 video processor: {e}"))?;
        let input_support = enumerator
            .CheckVideoProcessorFormat(DXGI_FORMAT_NV12)
            .map_err(|e| format!("Could not query NV12 presentation support: {e}"))?;
        if input_support & (D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32) == 0 {
            return Err("The D3D11 video processor cannot accept NV12 input".into());
        }
        let output_support = enumerator
            .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
            .map_err(|e| format!("Could not query BGRA presentation support: {e}"))?;
        if output_support & (D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32) == 0 {
            return Err("The D3D11 video processor cannot output the viewer surface format".into());
        }
        let processor = self
            .video_device
            .CreateVideoProcessor(&enumerator, 0)
            .map_err(|e| format!("Could not create the D3D11 video processor: {e}"))?;

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let back_buffer = self
            .back_buffer
            .as_ref()
            .ok_or_else(|| "D3D11 viewer back buffer is unavailable".to_string())?;
        let mut output_view = None;
        self.video_device
            .CreateVideoProcessorOutputView(
                back_buffer,
                &enumerator,
                &output_desc,
                Some(&mut output_view),
            )
            .map_err(|e| format!("Could not bind the D3D11 viewer surface: {e}"))?;
        let output_view =
            output_view.ok_or_else(|| "D3D11 returned an empty video output view".to_string())?;

        self.processor = Some(ProcessorState {
            input_width: width,
            input_height: height,
            enumerator,
            processor,
            output_view,
        });
        Ok(())
    }
}

unsafe fn create_back_buffer(
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView), String> {
    let back_buffer: ID3D11Texture2D = swap_chain
        .GetBuffer(0)
        .map_err(|e| format!("Could not get the D3D11 viewer back buffer: {e}"))?;
    let mut render_target = None;
    device
        .CreateRenderTargetView(&back_buffer, None, Some(&mut render_target))
        .map_err(|e| format!("Could not create the D3D11 viewer render target: {e}"))?;
    let render_target =
        render_target.ok_or_else(|| "D3D11 returned an empty viewer render target".to_string())?;
    Ok((back_buffer, render_target))
}

fn fitted_rect(
    output_width: u32,
    output_height: u32,
    frame_width: u32,
    frame_height: u32,
) -> (RECT, RECT) {
    let scale =
        (output_width as f32 / frame_width as f32).min(output_height as f32 / frame_height as f32);
    let width = (frame_width as f32 * scale).round().max(1.0) as i32;
    let height = (frame_height as f32 * scale).round().max(1.0) as i32;
    let x = (output_width as i32 - width) / 2;
    let y = (output_height as i32 - height) / 2;
    (
        RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        },
        RECT {
            left: 0,
            top: 0,
            right: output_width as i32,
            bottom: output_height as i32,
        },
    )
}
