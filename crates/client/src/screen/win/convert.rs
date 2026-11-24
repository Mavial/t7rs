//! The D3D11 device, and the GPU video processor that scales captured BGRA
//! frames into the NV12 textures the encoder reads.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{HMODULE, LUID, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::Interface;

use super::Context;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// The adapter, to pick an encoder that runs on it.
    pub luid: LUID,
}

impl Gpu {
    /// A device on the adapter that drives `monitor`, so frames need no copy
    /// between GPUs; the default adapter when none does.
    pub fn new(monitor: HMONITOR) -> Result<Gpu, String> {
        let adapter = adapter_for(monitor);
        let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        let (mut device, mut context) = (None, None);
        // SAFETY: out-pointers to locals.
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )
        }
        .context("creating a Direct3D 11 device")?;
        let (device, context): (ID3D11Device, ID3D11DeviceContext) =
            device.zip(context).ok_or("Direct3D 11 returned no device")?;
        // Capture, conversion and the encoder all use it from their own threads.
        // SAFETY: plain calls on a live device.
        unsafe {
            let _ = device.cast::<ID3D11Multithread>().context("multithread protection")?.SetMultithreadProtected(true);
            let luid = device.cast::<IDXGIDevice>().and_then(|d| d.GetAdapter()).and_then(|a| a.GetDesc());
            Ok(Gpu { luid: luid.context("adapter")?.AdapterLuid, device, context })
        }
    }
}

fn adapter_for(monitor: HMONITOR) -> Option<IDXGIAdapter> {
    // SAFETY: enumeration calls with no pointers of ours.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapters = (0..).map_while(|i| factory.EnumAdapters1(i).ok());
        adapters
            .filter(|adapter| {
                let mut outputs = (0..).map_while(|i| adapter.EnumOutputs(i).ok());
                outputs.any(|output| output.GetDesc().is_ok_and(|d| d.Monitor == monitor))
            })
            .find_map(|adapter| adapter.cast().ok())
    }
}

/// Number of NV12 textures, so the encoder can still be reading a few while
/// the next frame is converted.
pub const SLOTS: usize = 4;

/// Scales a frame into one of `SLOTS` NV12 textures of a fixed size, keeping
/// its aspect ratio with black bars. BT.709, limited range.
pub struct Converter {
    video: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    input: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    input_size: (u32, u32),
    output_size: (u32, u32),
}

impl Converter {
    pub fn new(gpu: &Gpu, input_size: (u32, u32), output_size: (u32, u32), fps: u32) -> Result<Converter, String> {
        let device: ID3D11VideoDevice = gpu.device.cast().context("this GPU has no video processor")?;
        let video: ID3D11VideoContext1 = gpu.context.cast().context("this GPU has no video processor")?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_size.0,
            InputHeight: input_size.1,
            OutputFrameRate: rate,
            OutputWidth: output_size.0,
            OutputHeight: output_size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: descriptors are locals that outlive each call.
        unsafe {
            let enumerator = device.CreateVideoProcessorEnumerator(&raw const desc).context("video processor")?;
            let processor = device.CreateVideoProcessor(&enumerator, 0).context("video processor")?;
            let input = texture(gpu, input_size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            let input_view = input_view(&device, &enumerator, &input)?;
            let outputs = (0..SLOTS)
                .map(|_| {
                    let texture = texture(gpu, output_size, DXGI_FORMAT_NV12)?;
                    Ok((texture.clone(), output_view(&device, &enumerator, &texture)?))
                })
                .collect::<Result<_, String>>()?;
            video.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            video.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
            };
            video.VideoProcessorSetOutputBackgroundColor(&processor, false, &raw const black);
            Ok(Converter { video, processor, input, input_view, outputs, input_size, output_size })
        }
    }

    /// The frame size this converter takes; frames of another size need a new one.
    pub fn input_size(&self) -> (u32, u32) {
        self.input_size
    }

    pub fn output(&self, slot: usize) -> &ID3D11Texture2D {
        &self.outputs[slot].0
    }

    /// Scales the top-left `content` of `frame` into `slot`.
    pub fn convert(&self, gpu: &Gpu, frame: &ID3D11Texture2D, content: (u32, u32), slot: usize) -> Result<(), String> {
        let content = (content.0.min(self.input_size.0), content.1.min(self.input_size.1));
        let source = RECT { left: 0, top: 0, right: content.0 as i32, bottom: content.1 as i32 };
        let dest = letterbox(content, self.output_size);
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        };
        // SAFETY: all views and rects are alive for the calls; the stream's
        // extra view reference is released right after.
        unsafe {
            gpu.context.CopyResource(&self.input, frame);
            self.video.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&raw const source));
            self.video.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&raw const dest));
            let result =
                self.video.VideoProcessorBlt(&self.processor, &self.outputs[slot].1, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result.context("scaling the frame")?;
            gpu.context.Flush();
        }
        Ok(())
    }
}

/// Where a `content`-sized picture lands in `frame`, centred at its own aspect ratio.
fn letterbox(content: (u32, u32), frame: (u32, u32)) -> RECT {
    let (cw, ch) = (u64::from(content.0.max(1)), u64::from(content.1.max(1)));
    let (fw, fh) = (u64::from(frame.0), u64::from(frame.1));
    let (w, h) = if cw * fh > ch * fw { (fw, ch * fw / cw) } else { (cw * fh / ch, fh) };
    let (x, y) = ((fw - w) / 2, (fh - h) / 2);
    RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 }
}

fn texture(gpu: &Gpu, size: (u32, u32), format: DXGI_FORMAT) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: `desc` outlives the call.
    unsafe { gpu.device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }.context("GPU texture")?;
    texture.ok_or_else(|| "GPU texture".into())
}

unsafe fn input_view(
    device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorInputView, String> {
    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
        },
    };
    let mut view = None;
    // SAFETY: `desc` outlives the call.
    unsafe { device.CreateVideoProcessorInputView(texture, enumerator, &raw const desc, Some(&raw mut view)) }
        .context("video processor input")?;
    view.ok_or_else(|| "video processor input".into())
}

unsafe fn output_view(
    device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorOutputView, String> {
    let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
    };
    let mut view = None;
    // SAFETY: `desc` outlives the call.
    unsafe { device.CreateVideoProcessorOutputView(texture, enumerator, &raw const desc, Some(&raw mut view)) }
        .context("video processor output")?;
    view.ok_or_else(|| "video processor output".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_centres_at_the_content_aspect_ratio() {
        let rect = |r: RECT| (r.left, r.top, r.right, r.bottom);
        assert_eq!(rect(letterbox((2560, 1440), (1920, 1080))), (0, 0, 1920, 1080));
        assert_eq!(rect(letterbox((1000, 1000), (1920, 1080))), (420, 0, 1500, 1080));
        assert_eq!(rect(letterbox((1920, 540), (1920, 1080))), (0, 270, 1920, 810));
    }
}
