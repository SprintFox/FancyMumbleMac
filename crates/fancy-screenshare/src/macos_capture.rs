//! Native macOS capture backend.
//!
//! ScreenCaptureKit owns display/window capture and delivers IOSurface-backed
//! frames. VideoToolbox consumes those IOSurfaces directly and emits H.264;
//! no CPU-side full-frame copy is performed on the normal path.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use apple_cf::cf::{AsCFType, CFDictionary, CFNumber, CFString};
use apple_cf::iosurface::IOSurface;
use screencapturekit::prelude::{
    CMSampleBuffer, CMSampleBufferExt, CMTime, PixelFormat, SCContentFilter, SCShareableContent,
    SCStream, SCStreamConfiguration,
};
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use videotoolbox::compression::{
    CompressionSession, FrameProperties, HardwareAcceleration, ProfileLevel,
};
use videotoolbox::session::Codec;

use crate::encode::{EncodeSettings, EncodedFrame};
use crate::pipeline::EncodePipeline;
use crate::sources::{CaptureSource, SourceKind};

/// Enumerate macOS capture sources from ScreenCaptureKit. The ids are native
/// CG display/window ids, matching the ids accepted by the stream filters.
pub(crate) fn list_sources() -> Result<Vec<CaptureSource>, String> {
    let content = SCShareableContent::get()
        .map_err(|e| format!("ScreenCaptureKit shareable content: {e}"))?;
    let mut sources = Vec::new();

    for (index, display) in content.displays().into_iter().enumerate() {
        let (width, height) = (display.width(), display.height());
        if width == 0 || height == 0 {
            continue;
        }
        sources.push(CaptureSource {
            id: display.display_id(),
            kind: SourceKind::Screen,
            title: format!("Screen {} ({}x{})", index + 1, width, height),
            width,
            height,
        });
    }

    for window in content.windows() {
        if !window.is_on_screen() {
            continue;
        }
        let title = window.title().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let frame = window.frame();
        let width = dimension_from_points(frame.size.width);
        let height = dimension_from_points(frame.size.height);
        if width == 0 || height == 0 {
            continue;
        }
        sources.push(CaptureSource {
            id: window.window_id(),
            kind: SourceKind::Window,
            title,
            width,
            height,
        });
    }

    Ok(sources)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn dimension_from_points(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else {
        value.round().min(f64::from(u32::MAX)) as u32
    }
}

fn resolve_filter(kind: SourceKind, source_id: u32) -> Result<(SCContentFilter, u32, u32), String> {
    let content = SCShareableContent::get()
        .map_err(|e| format!("ScreenCaptureKit enumerate sources: {e}"))?;
    match kind {
        SourceKind::Screen => {
            let display = content
                .displays()
                .into_iter()
                .find(|display| display.display_id() == source_id)
                .ok_or_else(|| format!("ScreenCaptureKit display {source_id} not found"))?;
            let dims = (display.width(), display.height());
            let filter = SCContentFilter::create()
                .with_display(&display)
                .with_excluding_windows(&[])
                .build()
                .map_err(|e| format!("ScreenCaptureKit display filter: {e}"))?;
            Ok((filter, dims.0, dims.1))
        }
        SourceKind::Window => {
            let window = content
                .windows()
                .into_iter()
                .find(|window| window.window_id() == source_id)
                .ok_or_else(|| format!("ScreenCaptureKit window {source_id} not found"))?;
            if !window.is_on_screen() {
                return Err(format!(
                    "ScreenCaptureKit window {source_id} is not on screen"
                ));
            }
            let frame = window.frame();
            let width = dimension_from_points(frame.size.width);
            let height = dimension_from_points(frame.size.height);
            let filter = SCContentFilter::create()
                .with_window(&window)
                .build()
                .map_err(|e| format!("ScreenCaptureKit window filter: {e}"))?;
            Ok((filter, width, height))
        }
        SourceKind::Device => {
            Err("camera sources are handled by the AVFoundation camera pipeline".to_owned())
        }
    }
}

#[derive(Default)]
struct LatestFrame {
    frame: Mutex<Option<IOSurface>>,
    ready: Condvar,
}

impl LatestFrame {
    fn push(&self, surface: IOSurface) {
        let mut latest = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *latest = Some(surface);
        self.ready.notify_one();
    }

    fn take(&self, wait: Duration) -> Option<IOSurface> {
        let mut latest = self
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if latest.is_none() && !wait.is_zero() {
            let (guard, _) = self
                .ready
                .wait_timeout(latest, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            latest = guard;
        }
        latest.take()
    }
}

struct ScreenOutput {
    latest: Arc<LatestFrame>,
}

impl SCStreamOutputTrait for ScreenOutput {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Screen || !sample.is_valid() {
            return;
        }
        let Some(pixel_buffer) = sample.pixel_buffer() else {
            return;
        };
        let Some(surface) = pixel_buffer.io_surface() else {
            tracing::warn!("screenshare: ScreenCaptureKit delivered a frame without an IOSurface");
            return;
        };
        self.latest.push(surface);
    }
}

pub(crate) struct MacCapturePipeline {
    stream: SCStream,
    latest: Arc<LatestFrame>,
    encoder: CompressionSession,
    last_surface: Option<IOSurface>,
    frame_index: i64,
    frame_rate: i32,
    settings: EncodeSettings,
    first_frame: bool,
}

impl std::fmt::Debug for MacCapturePipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacCapturePipeline")
            .field("frame_index", &self.frame_index)
            .field("frame_rate", &self.frame_rate)
            .finish_non_exhaustive()
    }
}

impl MacCapturePipeline {
    pub(crate) fn new(
        kind: SourceKind,
        source_id: u32,
        settings: EncodeSettings,
    ) -> Result<Self, String> {
        let (filter, source_width, source_height) = resolve_filter(kind, source_id)?;
        if source_width < 2 || source_height < 2 {
            return Err(format!(
                "capture source has invalid dimensions {source_width}x{source_height}"
            ));
        }

        let (width, height) =
            output_dimensions(source_width, source_height, settings.max_dimension);
        let frame_rate = frame_rate(settings.max_fps);
        let interval = CMTime::new(1, frame_rate);
        let configuration = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&interval);

        // VideoToolbox consumes the IOSurface supplied by ScreenCaptureKit
        // directly. Preferred hardware acceleration lets VideoToolbox choose
        // the Apple media engine when available and use its software encoder
        // otherwise.
        let bitrate = settings.bitrate_bps.min(i32::MAX as u32) as i32;
        let keyframe_interval = frame_rate.saturating_mul(2).max(1);
        let encoder = CompressionSession::builder(
            i32::try_from(width).map_err(|_| "capture width exceeds VideoToolbox limits")?,
            i32::try_from(height).map_err(|_| "capture height exceeds VideoToolbox limits")?,
            Codec::H264,
        )
        .with_real_time(true)
        .with_allow_frame_reordering(false)
        .with_average_bit_rate(bitrate)
        .with_expected_frame_rate(f64::from(frame_rate))
        .with_max_keyframe_interval(keyframe_interval)
        .with_profile_level(ProfileLevel::H264ConstrainedBaselineAutoLevel)
        .with_hardware_acceleration(HardwareAcceleration::Preferred)
        .with_low_latency_rate_control(true)
        .build()
        .map_err(|e| format!("VideoToolbox H.264 encoder: {e}"))?;

        let latest = Arc::new(LatestFrame::default());
        let mut stream = SCStream::new(&filter, &configuration)
            .map_err(|e| format!("ScreenCaptureKit stream: {e}"))?;
        stream
            .add_output_handler(
                ScreenOutput {
                    latest: Arc::clone(&latest),
                },
                SCStreamOutputType::Screen,
            )
            .map_err(|e| format!("ScreenCaptureKit register screen output: {e}"))?;
        stream
            .start_capture()
            .map_err(|e| format!("ScreenCaptureKit start capture: {e}"))?;

        tracing::info!(
            source_kind = ?kind,
            source_id,
            width,
            height,
            frame_rate,
            "screenshare: native ScreenCaptureKit + VideoToolbox pipeline active"
        );

        Ok(Self {
            stream,
            latest,
            encoder,
            last_surface: None,
            frame_index: 0,
            frame_rate,
            settings,
            first_frame: true,
        })
    }

    fn encode_surface(
        &mut self,
        surface: &IOSurface,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, String> {
        let presentation_time = CMTime::new(self.frame_index, self.frame_rate);
        self.frame_index = self.frame_index.saturating_add(1);
        let encoded = self
            .encoder
            .encode_with_properties(
                surface,
                presentation_time,
                FrameProperties::new().with_force_key_frame(force_keyframe),
            )
            .map_err(|e| format!("VideoToolbox H.264 encode: {e}"))?;
        if encoded.data.is_empty() {
            return Ok(None);
        }

        let (length_size, parameter_sets) = encoded
            .cm_sample_buffer()
            .and_then(|sample| sample.format_description())
            .and_then(|description| description.video_parameter_sets().ok())
            .map(|sets| {
                (
                    usize::try_from(sets.nal_unit_header_length).unwrap_or(4),
                    sets.parameter_sets,
                )
            })
            .unwrap_or_else(|| (4, Vec::new()));
        let (data, keyframe) = avcc_to_annex_b(&encoded.data, length_size, &parameter_sets)?;
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame { data, keyframe }))
    }
}

impl EncodePipeline for MacCapturePipeline {
    fn name(&self) -> &'static str {
        "macos-screencapturekit-videotoolbox"
    }

    fn next_frame(
        &mut self,
        wait: Duration,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, String> {
        if let Some(surface) = self.latest.take(wait) {
            self.last_surface = Some(surface);
        } else if !force_keyframe && !self.first_frame {
            return Ok(None);
        }

        let Some(surface) = self.last_surface.as_ref() else {
            return Ok(None);
        };
        let force = force_keyframe || self.first_frame;
        let result = self.encode_surface(surface, force)?;
        if result.as_ref().is_some_and(|frame| frame.keyframe) {
            self.first_frame = false;
        }
        Ok(result)
    }

    fn encode_repeat(&mut self) -> Result<Option<EncodedFrame>, String> {
        let Some(surface) = self.last_surface.as_ref() else {
            return Ok(None);
        };
        self.encode_surface(surface, false)
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) {
        let key = CFString::new("AverageBitRate");
        let value = CFNumber::from_u64(u64::from(bitrate_bps));
        let properties = CFDictionary::from_pairs(&[(&key, &value)]);
        if let Err(error) = self.encoder.set_properties(&properties) {
            tracing::warn!(bitrate_bps, %error, "screenshare: VideoToolbox rejected a live bitrate update");
        } else {
            self.settings.bitrate_bps = bitrate_bps;
        }
    }

    fn content_bitrate(&self) -> Option<u32> {
        Some(self.settings.bitrate_bps)
    }

    fn shutdown(&mut self) {
        let _ = self.stream.stop_capture();
    }
}

impl Drop for MacCapturePipeline {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn output_dimensions(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
    let longest = width.max(height);
    let scale = if max_dimension > 0 && longest > max_dimension {
        f64::from(max_dimension) / f64::from(longest)
    } else {
        1.0
    };
    let out_width = (f64::from(width) * scale).round() as u32;
    let out_height = (f64::from(height) * scale).round() as u32;
    (out_width.max(2) & !1, out_height.max(2) & !1)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn frame_rate(value: f32) -> i32 {
    value.round().clamp(1.0, 120.0) as i32
}

/// Convert VideoToolbox's AVCC length-prefixed H.264 access unit into the
/// Annex-B byte stream expected by the existing WebRTC RTP packetizer.
fn avcc_to_annex_b(
    avcc: &[u8],
    length_size: usize,
    parameter_sets: &[Vec<u8>],
) -> Result<(Vec<u8>, bool), String> {
    if !(1..=4).contains(&length_size) {
        return Err(format!(
            "VideoToolbox returned invalid NAL length size {length_size}"
        ));
    }
    let mut nals: Vec<&[u8]> = Vec::new();
    let mut offset = 0usize;
    while offset < avcc.len() {
        if avcc.len() - offset < length_size {
            return Err("truncated AVCC NAL length".to_owned());
        }
        let mut length = 0usize;
        for byte in &avcc[offset..offset + length_size] {
            length = length
                .checked_mul(256)
                .and_then(|value| value.checked_add(usize::from(*byte)))
                .ok_or_else(|| "AVCC NAL length overflow".to_owned())?;
        }
        offset += length_size;
        if length == 0 || length > avcc.len() - offset {
            return Err("invalid AVCC NAL payload length".to_owned());
        }
        nals.push(&avcc[offset..offset + length]);
        offset += length;
    }

    let has_idr = nals
        .iter()
        .any(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 5));
    let has_sps = nals
        .iter()
        .any(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 7));
    let has_pps = nals
        .iter()
        .any(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 8));

    let mut annex_b =
        Vec::with_capacity(avcc.len() + parameter_sets.iter().map(Vec::len).sum::<usize>() + 32);
    if has_idr {
        for parameter_set in parameter_sets {
            let Some(header) = parameter_set.first() else {
                continue;
            };
            let kind = header & 0x1f;
            if (kind == 7 && !has_sps) || (kind == 8 && !has_pps) {
                annex_b.extend_from_slice(&[0, 0, 0, 1]);
                annex_b.extend_from_slice(parameter_set);
            }
        }
    }
    for nal in nals {
        annex_b.extend_from_slice(&[0, 0, 0, 1]);
        annex_b.extend_from_slice(nal);
    }
    Ok((annex_b, has_idr))
}

#[cfg(test)]
mod tests {
    use super::{avcc_to_annex_b, output_dimensions};

    #[test]
    fn scales_to_the_longest_edge_and_keeps_even_h264_dimensions() {
        assert_eq!(output_dimensions(3840, 2160, 1920), (1920, 1080));
        assert_eq!(output_dimensions(1081, 1921, 0), (1080, 1920));
        assert_eq!(output_dimensions(1280, 720, 1920), (1280, 720));
    }

    #[test]
    fn converts_avcc_idr_and_injects_missing_parameter_sets() {
        let avcc = [0, 0, 0, 2, 0x65, 0xaa];
        let sps = vec![0x67, 0x42, 0x00];
        let pps = vec![0x68, 0xce];
        let (annex_b, keyframe) = avcc_to_annex_b(&avcc, 4, &[sps.clone(), pps.clone()]).unwrap();
        assert!(keyframe);
        assert_eq!(
            annex_b,
            [
                &[0, 0, 0, 1][..],
                sps.as_slice(),
                &[0, 0, 0, 1][..],
                pps.as_slice(),
                &[0, 0, 0, 1][..],
                &[0x65, 0xaa][..],
            ]
            .concat()
        );
    }

    #[test]
    fn does_not_duplicate_parameter_sets_already_in_access_unit() {
        let avcc = [
            0, 0, 0, 2, 0x67, 0x01, 0, 0, 0, 2, 0x68, 0x02, 0, 0, 0, 2, 0x65, 0xaa,
        ];
        let (annex_b, keyframe) =
            avcc_to_annex_b(&avcc, 4, &[vec![0x67, 0x01], vec![0x68, 0x02]]).unwrap();
        assert!(keyframe);
        assert_eq!(annex_b.len(), avcc.len() + 6);
    }

    #[test]
    fn rejects_truncated_or_invalid_avcc_lengths() {
        assert!(avcc_to_annex_b(&[0, 0], 4, &[]).is_err());
        assert!(avcc_to_annex_b(&[0, 0, 0, 0], 4, &[]).is_err());
        assert!(avcc_to_annex_b(&[0, 0, 0, 9, 1], 4, &[]).is_err());
    }
}

/// SCK audio capture used by the existing Opus/WebRTC audio-share worker.
pub(crate) struct MacDesktopAudioCapture {
    stream: SCStream,
}

impl Drop for MacDesktopAudioCapture {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}

struct AudioOutput {
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
}

impl SCStreamOutputTrait for AudioOutput {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio || !sample.is_valid() {
            return;
        }
        match sample_to_stereo_f32(&sample) {
            Ok(samples) if !samples.is_empty() => {
                // Never block ScreenCaptureKit's dispatch queue; a congested
                // Opus worker drops this audio block rather than stalling video.
                let _ = self.tx.try_send(samples);
            }
            Ok(_) => {}
            Err(error) => tracing::debug!("screenshare: unsupported system audio block: {error}"),
        }
    }
}

pub(crate) fn start_desktop_audio(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
) -> Result<MacDesktopAudioCapture, String> {
    let content = SCShareableContent::get()
        .map_err(|e| format!("ScreenCaptureKit audio permissions: {e}"))?;
    let display =
        content.displays().into_iter().next().ok_or_else(|| {
            "ScreenCaptureKit found no display for system audio capture".to_owned()
        })?;
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build()
        .map_err(|e| format!("ScreenCaptureKit audio filter: {e}"))?;
    let interval = CMTime::new(1, 60);
    let configuration = SCStreamConfiguration::new()
        .with_width(display.width().max(2))
        .with_height(display.height().max(2))
        .with_queue_depth(3)
        .with_minimum_frame_interval(&interval)
        .with_captures_audio(true)
        .with_sample_rate(48_000)
        .with_channel_count(2);
    let mut stream = SCStream::new(&filter, &configuration)
        .map_err(|e| format!("ScreenCaptureKit system-audio stream: {e}"))?;
    stream
        .add_output_handler(AudioOutput { tx }, SCStreamOutputType::Audio)
        .map_err(|e| format!("ScreenCaptureKit register audio output: {e}"))?;
    stream
        .start_capture()
        .map_err(|e| format!("ScreenCaptureKit start system audio: {e}"))?;
    tracing::info!("screenshare: ScreenCaptureKit system-audio capture active at 48 kHz stereo");
    Ok(MacDesktopAudioCapture { stream })
}

fn sample_to_stereo_f32(sample: &CMSampleBuffer) -> Result<Vec<f32>, String> {
    let description = sample
        .format_description()
        .ok_or_else(|| "audio sample has no format description".to_owned())?;
    let sample_rate = description.audio_sample_rate().unwrap_or(48_000.0);
    let channels = description.audio_channel_count().unwrap_or(2) as usize;
    let bits = description.audio_bits_per_channel().unwrap_or(32);
    let is_float = description.audio_is_float();
    let big_endian = description.audio_is_big_endian();
    if channels == 0 || channels > 8 {
        return Err(format!("unsupported audio channel count {channels}"));
    }
    let buffers = sample
        .audio_buffer_list()
        .map_err(|status| format!("CMSampleBuffer audio buffer list: OSStatus {status}"))?;
    let decoded: Vec<(usize, Vec<f32>)> = buffers
        .iter()
        .map(|buffer| {
            decode_pcm(buffer.data(), bits, is_float, big_endian)
                .map(|samples| (buffer.number_channels as usize, samples))
        })
        .collect::<Result<_, _>>()?;
    let mut stereo = interleave_stereo(&decoded, channels)?;
    if (sample_rate - 48_000.0).abs() > 1.0 {
        stereo = resample_stereo(&stereo, sample_rate);
    }
    Ok(stereo)
}

fn decode_pcm(
    data: &[u8],
    bits: u32,
    is_float: bool,
    big_endian: bool,
) -> Result<Vec<f32>, String> {
    let sample_bytes = usize::try_from(bits / 8).unwrap_or(0);
    if bits % 8 != 0 || sample_bytes == 0 {
        return Err(format!("unsupported PCM sample size {bits}"));
    }
    let mut output = Vec::with_capacity(data.len() / sample_bytes);
    for sample in data.chunks_exact(sample_bytes) {
        let value = match (is_float, bits, big_endian) {
            (true, 32, false) => f32::from_le_bytes(sample.try_into().map_err(|_| "bad float32")?),
            (true, 32, true) => f32::from_be_bytes(sample.try_into().map_err(|_| "bad float32")?),
            (true, 64, false) => {
                f64::from_le_bytes(sample.try_into().map_err(|_| "bad float64")?) as f32
            }
            (true, 64, true) => {
                f64::from_be_bytes(sample.try_into().map_err(|_| "bad float64")?) as f32
            }
            (false, 16, false) => {
                f32::from(i16::from_le_bytes(
                    sample.try_into().map_err(|_| "bad int16")?,
                )) / 32768.0
            }
            (false, 16, true) => {
                f32::from(i16::from_be_bytes(
                    sample.try_into().map_err(|_| "bad int16")?,
                )) / 32768.0
            }
            (false, 32, false) => {
                (i32::from_le_bytes(sample.try_into().map_err(|_| "bad int32")?) as f64
                    / 2_147_483_648.0) as f32
            }
            (false, 32, true) => {
                (i32::from_be_bytes(sample.try_into().map_err(|_| "bad int32")?) as f64
                    / 2_147_483_648.0) as f32
            }
            _ => {
                return Err(format!(
                    "unsupported PCM format: float={is_float}, bits={bits}"
                ));
            }
        };
        output.push(if value.is_finite() {
            value.clamp(-1.0, 1.0)
        } else {
            0.0
        });
    }
    Ok(output)
}

fn interleave_stereo(buffers: &[(usize, Vec<f32>)], channels: usize) -> Result<Vec<f32>, String> {
    let Some((buffer_channels, data)) = buffers.first() else {
        return Ok(Vec::new());
    };
    if buffers.len() == 1 {
        if *buffer_channels == 0 || data.len() % buffer_channels != 0 {
            return Err("malformed interleaved PCM buffer".to_owned());
        }
        let frames = data.len() / buffer_channels;
        let mut output = Vec::with_capacity(frames * 2);
        for frame in data.chunks_exact(*buffer_channels) {
            let left = frame[0];
            let right = if *buffer_channels == 1 {
                left
            } else {
                frame[1]
            };
            output.extend_from_slice(&[left, right]);
        }
        return Ok(output);
    }

    if buffers.len() >= channels && buffers.iter().all(|(count, _)| *count == 1) {
        let frames = buffers
            .iter()
            .map(|(_, channel)| channel.len())
            .min()
            .unwrap_or(0);
        let mut output = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            let left = buffers[0].1[index];
            let right = buffers.get(1).map_or(left, |buffer| buffer.1[index]);
            output.extend_from_slice(&[left, right]);
        }
        return Ok(output);
    }
    Err(format!(
        "unsupported AudioBufferList layout: {} buffers, {channels} channels",
        buffers.len()
    ))
}

fn resample_stereo(input: &[f32], input_rate: f64) -> Vec<f32> {
    let input_frames = input.len() / 2;
    if input_frames == 0 || !input_rate.is_finite() || input_rate <= 0.0 {
        return Vec::new();
    }
    let output_frames = ((input_frames as f64) * (48_000.0 / input_rate)).round() as usize;
    let mut output = Vec::with_capacity(output_frames.saturating_mul(2));
    for frame in 0..output_frames {
        let position = (frame as f64) * (input_rate / 48_000.0);
        let index = (position.floor() as usize).min(input_frames - 1);
        let next = (index + 1).min(input_frames - 1);
        let fraction = (position - index as f64) as f32;
        for channel in 0..2 {
            let a = input[index * 2 + channel];
            let b = input[next * 2 + channel];
            output.push(a + (b - a) * fraction);
        }
    }
    output
}

#[cfg(test)]
mod audio_tests {
    use super::{decode_pcm, interleave_stereo, resample_stereo};

    #[test]
    fn decodes_little_endian_float32_and_signed_pcm16() {
        let floats = [0.25_f32, -0.5_f32];
        let bytes: Vec<u8> = floats.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(decode_pcm(&bytes, 32, true, false).unwrap(), floats);
        let bytes = [0x00, 0x40, 0x00, 0xc0];
        let decoded = decode_pcm(&bytes, 16, false, false).unwrap();
        assert_eq!(decoded, [0.5, -0.5]);
    }

    #[test]
    fn converts_interleaved_and_planar_pcm_to_stereo() {
        let interleaved = [(2, vec![0.1, -0.1, 0.2, -0.2])];
        assert_eq!(
            interleave_stereo(&interleaved, 2).unwrap(),
            [0.1, -0.1, 0.2, -0.2]
        );
        let planar = [(1, vec![0.1, 0.2]), (1, vec![-0.1, -0.2])];
        assert_eq!(
            interleave_stereo(&planar, 2).unwrap(),
            [0.1, -0.1, 0.2, -0.2]
        );
    }

    #[test]
    fn resamples_stereo_to_48khz() {
        let input = [0.0, 0.0, 1.0, -1.0];
        let output = resample_stereo(&input, 24_000.0);
        assert_eq!(output.len(), 8);
        assert_eq!(&output[..2], &[0.0, 0.0]);
        assert_eq!(&output[6..], &[1.0, -1.0]);
    }
}
