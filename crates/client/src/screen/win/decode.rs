//! H.264 decoding with Microsoft's decoder transform into system memory, then
//! NV12 to RGB on the CPU. No GPU device: the viewer draws with softbuffer.

use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, IMFMediaBuffer, IMFSample, IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_E_TRANSFORM_TYPE_NOT_SET, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

use super::super::Picture;
use super::super::nv12::{self, Nv12};
use super::{Context, Runtime};

pub struct Decoder {
    transform: IMFTransform,
    output: Option<Output>,
    _runtime: Runtime,
}

// SAFETY: Microsoft's H.264 decoder is free-threaded, and every object here
// is only used through `&mut self`.
unsafe impl Send for Decoder {}

/// The negotiated NV12 output and the sample it is written into.
struct Output {
    sample: IMFSample,
    buffer: IMFMediaBuffer,
    stride: usize,
    /// Rows in the Y plane, often more than shown (1088 for 1080p).
    rows: usize,
    /// The part to show.
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Decoder {
    pub fn new() -> Result<Decoder, String> {
        let runtime = Runtime::start()?;
        // SAFETY: creates an in-process COM object and sets it up with locals.
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER).context("H.264 decoder")?;
            // Output each frame as soon as it is decoded, not after a reorder delay.
            transform.GetAttributes().and_then(|a| a.SetUINT32(&MF_LOW_LATENCY, 1)).context("low-latency decoding")?;
            let input = MFCreateMediaType().context("decoder input type")?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).context("decoder input type")?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).context("decoder input type")?;
            transform.SetInputType(0, &input, 0).context("decoder input type")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).context("starting the decoder")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).context("starting the decoder")?;
            Ok(Decoder { transform, output: None, _runtime: runtime })
        }
    }

    /// Feeds one access unit; when it completes a picture, writes the newest
    /// one into `picture` and returns true.
    pub fn decode(&mut self, data: &[u8], picture: &mut Picture) -> Result<bool, String> {
        let sample = input(data)?;
        let mut decoded = false;
        loop {
            // SAFETY: a plain call with a live sample.
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // Full: take what is ready, then the input fits.
                    if !self.drain()? {
                        return Err("the decoder takes no input and has no output".into());
                    }
                    decoded = true;
                }
                Err(e) => return Err(format!("decoding: {e}")),
            }
        }
        decoded |= self.drain()?;
        if decoded {
            self.picture(picture)?;
        }
        Ok(decoded)
    }

    /// Takes every ready picture, each over the last; true if there was one.
    fn drain(&mut self) -> Result<bool, String> {
        let mut decoded = false;
        let mut changes = 0;
        loop {
            let sample = self.output.as_ref().map(|o| o.sample.clone());
            // SAFETY: the output struct holds one extra sample reference across
            // the call, taken back out right after.
            let result = unsafe {
                if let Some(output) = &self.output {
                    output.buffer.SetCurrentLength(0).context("decoder output")?;
                }
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(sample),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut buffers, &raw mut status);
                let [buffer] = buffers;
                drop(ManuallyDrop::into_inner(buffer.pSample));
                drop(ManuallyDrop::into_inner(buffer.pEvents));
                result
            };
            match result {
                Ok(()) => decoded = true,
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(decoded),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE || e.code() == MF_E_TRANSFORM_TYPE_NOT_SET => {
                    changes += 1;
                    if changes > 2 {
                        return Err(format!("the decoder keeps changing format: {e}"));
                    }
                    self.negotiate()?;
                }
                Err(e) => return Err(format!("decoding: {e}")),
            }
        }
    }

    /// Picks NV12 output at the stream's current size.
    fn negotiate(&mut self) -> Result<(), String> {
        // SAFETY: plain calls on the live transform; the aperture blob is read
        // into a buffer of its exact size.
        unsafe {
            let nv12 = (0..)
                .map_while(|i| self.transform.GetOutputAvailableType(0, i).ok())
                .find(|t| t.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|s| s == MFVideoFormat_NV12))
                .ok_or("the decoder offers no NV12 output")?;
            self.transform.SetOutputType(0, &nv12, 0).context("decoder output type")?;
            let size = nv12.GetUINT64(&MF_MT_FRAME_SIZE).context("decoded size")?;
            let (width, rows) = ((size >> 32) as usize, (size & 0xffff_ffff) as usize);
            let stride = nv12.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(width, |s| (s as i32).unsigned_abs() as usize);
            let mut area = [0u8; size_of::<MFVideoArea>()];
            let shown = nv12.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, None).ok().map(|()| {
                let area: MFVideoArea = std::ptr::read_unaligned(area.as_ptr().cast());
                let offset = |o: i16| o.max(0) as usize;
                (offset(area.OffsetX.value), offset(area.OffsetY.value), area.Area.cx as usize, area.Area.cy as usize)
            });
            let (x, y, w, h) = shown.filter(|&(_, _, w, h)| w > 0 && h > 0).unwrap_or((0, 0, width, rows));
            let (w, h) = (w.min(width.saturating_sub(x)), h.min(rows.saturating_sub(y)));
            let info = self.transform.GetOutputStreamInfo(0).context("decoder output")?;
            let len = (stride * rows * 3 / 2).max(info.cbSize as usize);
            let buffer = MFCreateMemoryBuffer(len as u32).context("decoder output")?;
            let sample = MFCreateSample().context("decoder output")?;
            sample.AddBuffer(&buffer).context("decoder output")?;
            self.output = Some(Output { sample, buffer, stride, rows, x, y, width: w, height: h });
            Ok(())
        }
    }

    fn picture(&self, picture: &mut Picture) -> Result<(), String> {
        let out = self.output.as_ref().ok_or("no decoder output")?;
        // SAFETY: the buffer is locked while its bytes are read.
        unsafe {
            let (mut data, mut len) = (std::ptr::null_mut(), 0);
            out.buffer.Lock(&raw mut data, None, Some(&raw mut len)).context("decoded picture")?;
            let bytes = std::slice::from_raw_parts(data, len as usize);
            let result = convert(out, bytes, picture);
            let _ = out.buffer.Unlock();
            result
        }
    }
}

fn convert(out: &Output, bytes: &[u8], picture: &mut Picture) -> Result<(), String> {
    let plane = out.stride * out.rows;
    let uv_rows = out.rows.div_ceil(2);
    if bytes.len() < plane + out.stride * uv_rows {
        return Err(format!("decoded picture is {} bytes, short of {}", bytes.len(), plane + out.stride * uv_rows));
    }
    let (y, uv) = bytes.split_at(plane);
    let src = Nv12 {
        y: &y[out.y * out.stride + out.x..],
        uv: &uv[out.y / 2 * out.stride + out.x / 2 * 2..],
        stride: out.stride,
        width: out.width,
        height: out.height,
    };
    picture.width = out.width as u32;
    picture.height = out.height as u32;
    picture.pixels.resize(out.width * out.height, 0);
    nv12::to_rgb(&src, &mut picture.pixels);
    Ok(())
}

fn input(data: &[u8]) -> Result<IMFSample, String> {
    // SAFETY: the buffer is locked while `data` is copied in.
    unsafe {
        let buffer = MFCreateMemoryBuffer(data.len() as u32).context("decoder input")?;
        let mut dest = std::ptr::null_mut();
        buffer.Lock(&raw mut dest, None, None).context("decoder input")?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), dest, data.len());
        let _ = buffer.Unlock();
        buffer.SetCurrentLength(data.len() as u32).context("decoder input")?;
        let sample = MFCreateSample().context("decoder input")?;
        sample.AddBuffer(&buffer).context("decoder input")?;
        Ok(sample)
    }
}
