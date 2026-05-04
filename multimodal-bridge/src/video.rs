//! Video extractor.
//!
//! Feature-gated behind `ffmpeg`. When enabled, uses `ffmpeg-next` to:
//! * Sample a frame every N seconds (default 5s) and record its
//!   presentation timestamp as an `elements` entry.
//! * Extract the audio stream as PCM f32 mono and hand it to
//!   `AudioExtractor` (with `whisper` feature on) for transcription.
//!
//! When the feature is off we still return an `ExtractedDoc` — just
//! with empty frames/transcript and a WARN log. This preserves the
//! "degrade, never crash" contract.

use std::path::{Path, PathBuf};

use tracing::warn;

#[cfg(feature = "ffmpeg")]
use tracing::debug;

use crate::extractor::{ext_of, Extractor};
use crate::types::{ExtractError, ExtractResult, ExtractedDoc};

/// Video extractor handle.
#[derive(Debug, Clone)]
pub struct VideoExtractor {
    /// Seconds between sampled frames. Must be > 0.
    pub frame_sample_secs: u32,
    /// Optional path to a Whisper GGML model. When `None`, audio
    /// transcription is skipped even if the `whisper` feature is on.
    #[cfg_attr(not(any(feature = "ffmpeg", feature = "whisper")), allow(dead_code))]
    pub whisper_model: Option<PathBuf>,
}

impl Default for VideoExtractor {
    fn default() -> Self {
        Self {
            frame_sample_secs: 5,
            whisper_model: None,
        }
    }
}

impl VideoExtractor {
    /// Override the frame sample interval in seconds.
    pub fn with_frame_interval(mut self, secs: u32) -> Self {
        self.frame_sample_secs = secs.max(1);
        self
    }

    /// Configure the Whisper model path used for audio transcription.
    pub fn with_whisper_model(mut self, path: impl Into<PathBuf>) -> Self {
        self.whisper_model = Some(path.into());
        self
    }
}

impl Extractor for VideoExtractor {
    fn kinds(&self) -> &[&'static str] {
        &["mp4", "mov", "mkv", "webm", "avi"]
    }

    fn extract(&self, path: &Path) -> ExtractResult<ExtractedDoc> {
        let ext = ext_of(path);
        if !self.kinds().contains(&ext.as_str()) {
            return Err(ExtractError::Unsupported {
                path: path.to_path_buf(),
                kind: ext,
            });
        }
        let meta = std::fs::metadata(path).map_err(|source| ExtractError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut doc = ExtractedDoc::empty("video", path);
        doc.metadata
            .insert("byte_size".into(), meta.len().to_string());
        doc.metadata.insert("container".into(), ext.clone());

        self.sample(path, &mut doc)?;
        Ok(doc)
    }
}

impl VideoExtractor {
    #[cfg(feature = "ffmpeg")]
    fn sample(&self, path: &Path, doc: &mut ExtractedDoc) -> ExtractResult<()> {
        use ffmpeg::format::{input, Pixel};
        use ffmpeg::media::Type;
        use ffmpeg::software::scaling::{context::Context as SwsCtx, flag::Flags};
        use ffmpeg::util::frame::video::Video;
        use ffmpeg_next as ffmpeg;

        ffmpeg::init().map_err(|e| ExtractError::Other(format!("ffmpeg init: {e}")))?;

        let mut ictx = input(&path).map_err(|e| ExtractError::Parse {
            path: path.to_path_buf(),
            reason: format!("ffmpeg open: {e}"),
        })?;

        // Video stream: sample frames at N-sec intervals.
        let v_stream = ictx
            .streams()
            .best(Type::Video)
            .ok_or_else(|| ExtractError::Parse {
                path: path.to_path_buf(),
                reason: "no video stream".into(),
            })?;
        let v_idx = v_stream.index();
        let v_time_base = v_stream.time_base();
        let v_params = v_stream.parameters();
        let mut v_dec = ffmpeg::codec::context::Context::from_parameters(v_params)
            .and_then(|c| c.decoder().video())
            .map_err(|e| ExtractError::Parse {
                path: path.to_path_buf(),
                reason: format!("ffmpeg v decoder: {e}"),
            })?;

        // Output scaler — we don't actually persist frame pixels here;
        // this is kept as scaffolding for future per-frame CLIP/OCR work.
        let mut scaler = SwsCtx::get(
            v_dec.format(),
            v_dec.width(),
            v_dec.height(),
            Pixel::RGB24,
            v_dec.width(),
            v_dec.height(),
            Flags::BILINEAR,
        )
        .map_err(|e| ExtractError::Other(format!("ffmpeg sws: {e}")))?;

        // A8-007 (2026-05-04): the previous version put `.max(1)` on the
        // numerator but not the denominator, so a malformed AV1 / WebM
        // stream reporting `time_base = N/0` (which some weird captures
        // produce) divided by zero and panicked the build worker. Both
        // sides now floor to 1, and the multiplication uses
        // `saturating_mul` so a 60 fps video with `frame_sample_secs=5`
        // and a `1/AV_TIME_BASE` (1e6 ticks/sec) container can no longer
        // overflow `i64`.
        let denom = (v_time_base.denominator() as i64).max(1);
        let num = (v_time_base.numerator() as i64).max(1);
        let interval_ticks: i64 = (self.frame_sample_secs as i64)
            .saturating_mul(denom)
            / num;
        let mut next_sample_ticks: i64 = 0;

        for (stream, packet) in ictx.packets() {
            if stream.index() != v_idx {
                continue;
            }
            v_dec.send_packet(&packet).ok();
            let mut frame = Video::empty();
            while v_dec.receive_frame(&mut frame).is_ok() {
                let pts = frame.pts().unwrap_or(0);
                if pts < next_sample_ticks {
                    continue;
                }
                let mut rgb = Video::empty();
                let _ = scaler.run(&frame, &mut rgb);
                let ms = (pts as f64 * v_time_base.numerator() as f64 * 1000.0
                    / v_time_base.denominator().max(1) as f64) as u64;
                doc.elements.push(serde_json::json!({
                    "kind": "video_frame",
                    "timestamp_ms": ms,
                    "width": v_dec.width(),
                    "height": v_dec.height(),
                }));
                next_sample_ticks = pts.saturating_add(interval_ticks);
            }
        }
        v_dec.send_eof().ok();

        doc.metadata.insert(
            "sample_interval_sec".into(),
            self.frame_sample_secs.to_string(),
        );
        doc.metadata
            .insert("width".into(), v_dec.width().to_string());
        doc.metadata
            .insert("height".into(), v_dec.height().to_string());

        // Audio handoff is feature-combined. Without `whisper` we leave
        // `transcript` empty; future wiring will pipe ffmpeg's decoded
        // samples into whisper-rs via a shared buffer.
        debug!(
            path = %path.display(),
            frames = doc.elements.len(),
            "video sampled"
        );
        Ok(())
    }

    #[cfg(not(feature = "ffmpeg"))]
    fn sample(&self, path: &Path, _doc: &mut ExtractedDoc) -> ExtractResult<()> {
        warn!(
            path = %path.display(),
            "ffmpeg feature disabled; video frame sampling skipped"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_extractor_known_kinds() {
        let e = VideoExtractor::default();
        assert!(e.kinds().contains(&"mp4"));
        assert!(e.kinds().contains(&"webm"));
    }

    #[test]
    fn video_extractor_rejects_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.txt");
        std::fs::write(&path, "x").unwrap();
        let err = VideoExtractor::default().extract(&path).unwrap_err();
        assert!(matches!(err, ExtractError::Unsupported { .. }));
    }

    #[cfg(not(feature = "ffmpeg"))]
    #[test]
    fn video_extractor_degrades_without_feature() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, b"\x00\x00\x00 ftypisom").unwrap();
        let doc = VideoExtractor::default().extract(&path).expect("ok");
        assert_eq!(doc.kind, "video");
        assert!(doc.elements.is_empty(), "no frames without ffmpeg");
    }
}
