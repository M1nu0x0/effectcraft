//! Fork only (`videotoolbox` branch): H.264 and HEVC export through VideoToolbox, using the safe API
//! of FilmCraft's platform crate. A job the hardware path cannot honour stays on the software encoder;
//! a session that fails in the middle of an export stops it with an error, like any encoder.

use std::sync::atomic::{AtomicBool, Ordering};

use effectcraft_project::render_queue::{CodecProfile, OutputFormat, OutputModule, RateControlMode};
use effectcraft_time::FrameRate;
use filmcraft_isobmff::{AvcConfig, HevcConfig, SampleEntry};
use filmcraft_platform::videotoolbox_encode::{ParamSets, VtConfig, VtEncoder, VtPacket, VtProfile, VtRate};

use crate::encode::{Codes, Packet, VideoEncoder, Yuv420, tag_nclx_limited};
use crate::{ExportError, Result};

/// Off until a frontend asks: the software encoders' output is byte-identical on every machine,
/// which this crate's tests pin.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// [`open`], when a frontend turned hardware encoding on.
pub(crate) fn for_job(format: OutputFormat, w: u32, h: u32, rate: FrameRate, output: &OutputModule, codes: Codes) -> Option<Box<dyn VideoEncoder>> {
    if ENABLED.load(Ordering::Relaxed) { open(format, w, h, rate, output, codes) } else { None }
}

/// A hardware encoder for this job, or `None` (reason logged) to use the software one.
fn open(format: OutputFormat, w: u32, h: u32, rate: FrameRate, output: &OutputModule, codes: Codes) -> Option<Box<dyn VideoEncoder>> {
    if !matches!(format, OutputFormat::H264 | OutputFormat::Hevc) {
        return None;
    }
    let session = config(format, w, h, rate, output, codes).and_then(|cfg| {
        let encoder = VtEncoder::new(cfg)?;
        if encoder.uses_hardware() { Ok(encoder) } else { Err("VideoToolbox did not create a hardware session".to_string()) }
    });
    match session {
        Ok(encoder) => {
            log::info!("{format:?} export: VideoToolbox hardware encoder");
            let frame_duration = i64::from(encoder.config().frame_duration);
            Some(Box::new(Vt { encoder, format, w, h, codes, frame_duration, emitted: 0 }))
        }
        Err(why) => {
            log::warn!("{format:?} export: software encoder, VideoToolbox declined: {why}");
            None
        }
    }
}

/// The session for a job, or why VideoToolbox cannot honour it.
fn config(format: OutputFormat, w: u32, h: u32, rate: FrameRate, output: &OutputModule, codes: Codes) -> std::result::Result<VtConfig, String> {
    let profile = if format == OutputFormat::Hevc {
        // VtProfile has no Main 10, and the session has no quality or level property
        if output.codec.profile == CodecProfile::Main10 {
            return Err("HEVC Main 10".into());
        }
        if output.codec.rate_control == RateControlMode::Quality {
            return Err("constant quality".into());
        }
        if output.codec.level.is_some() {
            return Err("an explicit HEVC level".into());
        }
        VtProfile::HevcMain
    } else {
        VtProfile::High
    };
    if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err("odd picture size".into());
    }
    // the session tags its stream BT.709, which would contradict any other output space
    if (codes.primaries, codes.transfer, codes.matrix) != (1, 1, 1) {
        return Err("an output colour space other than BT.709".into());
    }
    let timescale = u32::try_from(rate.num).ok().filter(|n| *n > 0).ok_or("invalid frame rate")?;
    let frame_duration = u32::try_from(rate.den).ok().filter(|n| *n > 0).ok_or("invalid frame rate")?;
    let kbps = output.bitrate_kbps.max(100);
    Ok(VtConfig {
        width: w,
        height: h,
        timescale,
        frame_duration,
        keyframe_interval: output.keyint(rate.as_f64()),
        profile,
        rate: VtRate::Vbr { target_kbps: kbps, max_kbps: kbps.saturating_mul(3) / 2 },
    })
}

/// The `hvcC` record of a stream, checked: the muxer writes 4-byte NAL lengths.
fn hevc_config(params: &ParamSets) -> std::result::Result<HevcConfig, String> {
    let record = params.hvcc.as_deref().ok_or("VideoToolbox wrote no hvcC record")?;
    let cfg = HevcConfig::parse(record).map_err(|e| format!("unreadable hvcC record: {e}"))?;
    if cfg.vps().is_empty() || cfg.sps().is_empty() || cfg.pps().is_empty() || cfg.length_size != 4 {
        return Err("the hvcC record lacks a parameter set or does not use 4-byte NAL lengths".into());
    }
    Ok(cfg)
}

struct Vt {
    encoder: VtEncoder,
    format: OutputFormat,
    w: u32,
    h: u32,
    codes: Codes,
    frame_duration: i64,
    emitted: i64,
}

impl Vt {
    /// The muxer writes one frame duration per packet, so a dropped or reordered frame must be an
    /// error rather than a movie that silently drifts from its audio.
    fn packets(&mut self, packets: Vec<VtPacket>) -> Result<Vec<Packet>> {
        let mut out = Vec::with_capacity(packets.len());
        for p in packets {
            let expected = self.emitted.checked_mul(self.frame_duration).ok_or_else(|| ExportError::Encode("VideoToolbox timestamp overflow".into()))?;
            if p.pts != expected || p.dts != expected {
                return Err(ExportError::Encode(format!("VideoToolbox returned pts {}, dts {}, expected {expected}", p.pts, p.dts)));
            }
            self.emitted += 1;
            out.push(Packet { data: p.data, key: p.key, cto: 0 });
        }
        Ok(out)
    }
}

impl VideoEncoder for Vt {
    fn sample_entry(&self) -> SampleEntry {
        let params = self.encoder.parameter_sets();
        let (w, h) = (self.w as u16, self.h as u16);
        let mut entry = if self.format == OutputFormat::H264 {
            let (sps, pps) = params.map(|p| (p.sps, p.pps)).unwrap_or_default();
            SampleEntry::avc(AvcConfig::new(sps, pps, 4), w, h)
        } else {
            // `encode_yuv420` refuses to go on without a usable record
            SampleEntry::hevc(params.and_then(|p| hevc_config(&p).ok()).unwrap_or_default(), w, h)
        };
        tag_nclx_limited(&mut entry, self.codes);
        entry
    }

    fn encode(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>> {
        self.encode_yuv420(&Yuv420::from_rgba(rgba, self.w as usize, self.h as usize), index)
    }

    fn takes_yuv420(&self) -> bool {
        true
    }

    fn encode_yuv420(&mut self, frame: &Yuv420, index: u64) -> Result<Vec<Packet>> {
        let (y, u, v) = frame.slices(self.w as usize, self.h as usize);
        let packets = self.encoder.encode(index, y, u, v).map_err(ExportError::Encode)?;
        if index == 0 {
            let params = self.encoder.parameter_sets().ok_or_else(|| ExportError::Encode("VideoToolbox returned no parameter sets".into()))?;
            if params.sps.is_empty() || params.pps.is_empty() {
                return Err(ExportError::Encode("VideoToolbox returned incomplete parameter sets".into()));
            }
            if self.format == OutputFormat::Hevc {
                hevc_config(&params).map_err(ExportError::Encode)?;
            }
        }
        self.packets(packets)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let packets = self.encoder.flush().map_err(ExportError::Encode)?;
        self.packets(packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BT709: Codes = Codes { primaries: 1, transfer: 1, matrix: 1 };
    const RATE: FrameRate = FrameRate { num: 30000, den: 1001 };

    #[test]
    fn jobs_the_session_cannot_honour_stay_in_software() {
        let om = OutputModule::default();
        let mut main10 = om.clone();
        main10.codec.profile = CodecProfile::Main10;
        let mut quality = om.clone();
        quality.codec.rate_control = RateControlMode::Quality;
        let mut level = om.clone();
        level.codec.level = Some(41);
        for declined in [&main10, &quality, &level] {
            assert!(config(OutputFormat::Hevc, 320, 180, RATE, declined, BT709).is_err());
        }
        // H.264's software encoder ignores these HEVC options, so they do not decline H.264
        assert!(config(OutputFormat::H264, 320, 180, RATE, &main10, BT709).is_ok());
        assert!(config(OutputFormat::H264, 321, 180, RATE, &om, BT709).is_err());
        assert!(config(OutputFormat::H264, 320, 180, RATE, &om, Codes { primaries: 9, transfer: 1, matrix: 1 }).is_err());
        assert!(config(OutputFormat::H264, 320, 180, FrameRate { num: 0, den: 1 }, &om, BT709).is_err());
        assert!(open(OutputFormat::ProRes, 320, 180, RATE, &om, BT709).is_none());
    }

    #[test]
    fn hardware_session_emits_every_frame_with_parameter_sets() {
        let (w, h) = (320usize, 180usize);
        let om = OutputModule::default();
        for format in [OutputFormat::H264, OutputFormat::Hevc] {
            // a machine or VM without a hardware encoder declines, which is the fallback path
            let Some(mut enc) = open(format, w as u32, h as u32, RATE, &om, BT709) else { continue };
            let mut packets = vec![];
            for k in 0..3u8 {
                let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, k.wrapping_mul(40), 128, 255]).collect();
                packets.extend(enc.encode(&rgba, u64::from(k)).unwrap());
            }
            packets.extend(enc.flush().unwrap());
            assert_eq!(packets.len(), 3, "{format:?}");
            assert!(packets[0].key, "{format:?}");
            assert!(enc.sample_entry().video.is_some_and(|v| v.color.is_some()), "{format:?}");
        }
    }
}
