use std::mem::ManuallyDrop;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{LUID, VARIANT_BOOL};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264EncoderMFT, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFShutdown, IMFTransform, METransformHaveOutput, METransformNeedInput,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NONE, MF_EVENT_TYPE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFNominalRange_16_235, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnum2, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR,
    eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use super::super::Params;
use super::super::annexb::ParamSets;
use super::Context;
use super::convert::Gpu;
use super::share::Wake;

pub struct Encoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<JoinHandle<()>>,
    output: Option<IMFSample>,
    params: ParamSets,
    name: String,
    activate: Option<IMFActivate>,
    _manager: IMFDXGIDeviceManager,
}

impl Encoder {
    pub fn new(gpu: &Gpu, size: (u32, u32), params: &Params, wake: &Sender<Wake>) -> Result<Encoder, String> {
        let (mut token, mut manager) = (0, None);
        let manager: IMFDXGIDeviceManager = unsafe {
            MFCreateDXGIDeviceManager(&raw mut token, &raw mut manager).context("DXGI device manager")?;
            let manager = manager.ok_or("no DXGI device manager")?;
            manager.ResetDevice(&gpu.device, token).context("DXGI device manager")?;
            manager
        };
        let mut errors = Vec::new();
        if std::env::var_os("T7_SOFTWARE_ENCODER").is_none() {
            for activate in hardware_encoders(gpu.luid) {
                let name = friendly_name(&activate);
                let transform = unsafe { activate.ActivateObject::<IMFTransform>() }.context("activating");
                match transform.and_then(|t| Encoder::setup(t, &manager, size, params, wake)) {
                    Ok(mut encoder) => {
                        encoder.name = format!("{name} (GPU)");
                        encoder.activate = Some(activate);
                        return Ok(encoder);
                    }
                    Err(e) => {
                        errors.push(format!("{name}: {e}"));
                        let _ = unsafe { activate.ShutdownObject() };
                    }
                }
            }
        }
        let software = unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) };
        match software.context("software H.264 encoder").and_then(|t| Encoder::setup(t, &manager, size, params, wake)) {
            Ok(mut encoder) => {
                if !errors.is_empty() {
                    tracing::warn!("screen share falls back to software encoding: {}", errors.join("; "));
                }
                encoder.name = "Microsoft H.264 encoder (software)".into();
                Ok(encoder)
            }
            Err(e) => {
                errors.push(e);
                Err(format!("no H.264 encoder works: {}", errors.join("; ")))
            }
        }
    }

    fn setup(
        transform: IMFTransform,
        manager: &IMFDXGIDeviceManager,
        size: (u32, u32),
        params: &Params,
        wake: &Sender<Wake>,
    ) -> Result<Encoder, String> {
        unsafe {
            let attributes = transform.GetAttributes().context("encoder attributes")?;
            let is_async = attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if is_async {
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("unlocking the encoder")?;
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            if is_async || attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0 {
                (transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize))
                    .context("giving the encoder the GPU")?;
            }
            // Rate control goes before the media types: some encoders only read it then.
            let codec: Option<ICodecAPI> = transform.cast().ok();
            if let Some(codec) = &codec {
                let settings = [
                    (CODECAPI_AVLowLatencyMode, boolean(true)),
                    (CODECAPI_AVEncCommonRateControlMode, uint(eAVEncCommonRateControlMode_CBR.0 as u32)),
                    (CODECAPI_AVEncCommonMeanBitRate, uint(params.bitrate)),
                    (CODECAPI_AVEncMPVDefaultBPictureCount, uint(0)),
                    (CODECAPI_AVEncMPVGOPSize, uint(params.fps * 600)),
                ];
                for (api, value) in &settings {
                    if let Err(e) = codec.SetValue(api, value) {
                        tracing::debug!("encoder setting {api:?}: {e}");
                    }
                }
            }
            let output = video_type(MFVideoFormat_H264, size, params.fps).context("encoder output type")?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, params.bitrate).context("encoder output type")?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).context("encoder output type")?;
            transform.SetOutputType(0, &output, 0).context("encoder output type")?;
            let input = video_type(MFVideoFormat_NV12, size, params.fps).context("encoder input type")?;
            transform.SetInputType(0, &input, 0).context("encoder input type")?;

            let info = transform.GetOutputStreamInfo(0).context("encoder output")?;
            let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            let output = if info.dwFlags & provides == 0 {
                let sample = MFCreateSample().context("encoder output")?;
                let buffer =
                    MFCreateMemoryBuffer(info.cbSize.max(size.0 * size.1 * 3 / 2)).context("encoder output")?;
                sample.AddBuffer(&buffer).context("encoder output")?;
                Some(sample)
            } else {
                None
            };
            let events = if is_async { Some(forward_events(&transform, wake.clone())?) } else { None };
            let encoder = Encoder {
                transform,
                codec,
                events,
                output,
                params: ParamSets::default(),
                name: String::new(),
                activate: None,
                _manager: manager.clone(),
            };
            encoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).context("starting the encoder")?;
            encoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).context("starting the encoder")?;
            Ok(encoder)
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Asynchronous encoders take input only after `Wake::NeedInput` and give
    /// output after `Wake::HaveOutput`; synchronous ones take input any time
    /// and have their output right after.
    pub fn is_async(&self) -> bool {
        self.events.is_some()
    }

    pub fn input(&mut self, texture: &ID3D11Texture2D, time: i64, duration: i64, key: bool) -> Result<(), String> {
        // SAFETY: the texture stays alive in the sample for as long as the encoder holds it.
        unsafe {
            if key && let Some(codec) = &self.codec {
                let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &uint(1));
            }
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false).context("encoder input")?;
            let len = buffer.cast::<IMF2DBuffer>().and_then(|b| b.GetContiguousLength()).context("encoder input")?;
            buffer.SetCurrentLength(len).context("encoder input")?;
            let sample = MFCreateSample().context("encoder input")?;
            sample.AddBuffer(&buffer).context("encoder input")?;
            sample.SetSampleTime(time).context("encoder input")?;
            sample.SetSampleDuration(duration).context("encoder input")?;
            self.transform.ProcessInput(0, &sample, 0).context("encoding")
        }
    }

    /// One encoded frame in Annex B form, keyframes with SPS and PPS; `None`
    /// when the encoder needs more input first.
    pub fn output(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            // SAFETY: the output struct owns one reference to each COM object
            // it holds after the call, and both are taken back out below.
            let (result, sample) = unsafe {
                if let Some(sample) = &self.output {
                    sample.GetBufferByIndex(0).and_then(|b| b.SetCurrentLength(0)).context("encoder output")?;
                }
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(self.output.clone()),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut buffers, &raw mut status);
                let [buffer] = buffers;
                drop(ManuallyDrop::into_inner(buffer.pEvents));
                (result, ManuallyDrop::into_inner(buffer.pSample))
            };
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    unsafe {
                        let output = self.transform.GetOutputAvailableType(0, 0).context("encoder output type")?;
                        self.transform.SetOutputType(0, &output, 0).context("encoder output type")?;
                    }
                    continue;
                }
                Err(e) => return Err(format!("encoding: {e}")),
            }
            let mut data = read(&sample.ok_or("the encoder gave no output")?)?;
            self.params.complete(&mut data);
            return Ok(Some(data));
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Shutting down ends the event thread's wait.
        unsafe {
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            if let Some(events) = self.events.take() {
                let _ = events.join();
            }
            if let Some(activate) = &self.activate {
                let _ = activate.ShutdownObject();
            }
        }
    }
}

fn read(sample: &IMFSample) -> Result<Vec<u8>, String> {
    // SAFETY: the buffer is locked while its bytes are copied.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().context("encoder output")?;
        let (mut data, mut len) = (std::ptr::null_mut(), 0);
        buffer.Lock(&raw mut data, None, Some(&raw mut len)).context("encoder output")?;
        let bytes = std::slice::from_raw_parts(data, len as usize).to_vec();
        let _ = buffer.Unlock();
        Ok(bytes)
    }
}

struct EventQueue(IMFMediaEventGenerator);
// SAFETY: a Media Foundation transform's event queue is free-threaded.
unsafe impl Send for EventQueue {}

fn forward_events(transform: &IMFTransform, wake: Sender<Wake>) -> Result<JoinHandle<()>, String> {
    let events = EventQueue(transform.cast().context("encoder events")?);
    let spawned = std::thread::Builder::new().name("screen encoder events".into()).spawn(move || {
        let events = events;
        while let Ok(event) = unsafe { events.0.GetEvent(MF_EVENT_FLAG_NONE) } {
            let kind = unsafe { event.GetType() }.map_or(MF_EVENT_TYPE(0), |t| MF_EVENT_TYPE(t as i32));
            let message = if kind == METransformNeedInput {
                Wake::NeedInput
            } else if kind == METransformHaveOutput {
                Wake::HaveOutput
            } else {
                match unsafe { event.GetStatus() } {
                    Ok(status) if status.is_err() => {
                        Wake::Failed(format!("encoder: {}", windows::core::Error::from(status)))
                    }
                    _ => continue,
                }
            };
            if wake.send(message).is_err() {
                return;
            }
        }
    });
    spawned.map_err(|e| format!("encoder event thread: {e}"))
}

fn hardware_encoders(luid: LUID) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let luid = (u64::from(luid.HighPart as u32) << 32 | u64::from(luid.LowPart)).to_le_bytes();
    // SAFETY: out-pointers to locals; the returned array is freed once its
    // entries are moved out.
    unsafe {
        let mut attributes = None;
        if MFCreateAttributes(&raw mut attributes, 1).is_err() {
            return Vec::new();
        }
        let Some(attributes) = attributes else { return Vec::new() };
        if attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid).is_err() {
            return Vec::new();
        }
        let (mut list, mut count) = (std::ptr::null_mut(), 0);
        let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
        let found = MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&raw const input),
            Some(&raw const output),
            &attributes,
            &raw mut list,
            &raw mut count,
        );
        if found.is_err() || list.is_null() {
            return Vec::new();
        }
        let encoders =
            std::slice::from_raw_parts_mut(list, count as usize).iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(list as _));
        encoders
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = [0u16; 128];
    let mut len = 0;
    match unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name, Some(&raw mut len)) } {
        Ok(()) => String::from_utf16_lossy(&name[..len as usize]),
        Err(_) => "hardware H.264 encoder".into(),
    }
}

fn video_type(subtype: GUID, size: (u32, u32), fps: u32) -> windows::core::Result<IMFMediaType> {
    let pair = |hi: u32, lo: u32| u64::from(hi) << 32 | u64::from(lo);
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &raw const subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pair(fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        Ok(t)
    }
}

fn uint(value: u32) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    v
}

fn boolean(value: bool) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = VARIANT_BOOL(if value { -1 } else { 0 });
    }
    v
}
