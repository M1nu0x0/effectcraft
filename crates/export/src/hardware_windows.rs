//! Fork only (`videotoolbox` branch): H.264 and HEVC export through NVIDIA NVENC on Windows, using the
//! safe API of FilmCraft's platform crate (the driver's `nvEncodeAPI64.dll`; no CUDA toolkit or SDK).
//! A job NVENC cannot honour, or a machine without an NVIDIA GPU and driver, stays on the software
//! encoder; a session that fails in the middle of an export stops it with an error, like any encoder.

use std::sync::atomic::{AtomicBool, Ordering};

use effectcraft_project::render_queue::{CodecProfile, OutputFormat, OutputModule, RateControlMode};
use effectcraft_time::FrameRate;
use filmcraft_isobmff::{AvcConfig, SampleEntry};
use filmcraft_platform::nvenc::{Codec, Config, Nvenc, Packet as NvPacket, Profile, Signal};

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
        // RGBA input keeps the 4:2:0 conversion on the GPU; a driver that refuses it takes the render
        // threads' 4:2:0 instead (`takes_yuv420`)
        Nvenc::with_rgba_input(&cfg, &Signal::default()).or_else(|why| {
            log::info!("{format:?} export: NVENC RGBA input declined ({why}), converting to 4:2:0 on the CPU");
            Nvenc::new(&cfg)
        })
    });
    match session {
        Ok(encoder) => {
            log::info!("{format:?} export: NVENC hardware encoder");
            Some(Box::new(Nv { encoder, w, h, codes, emitted: 0 }))
        }
        Err(why) => {
            log::warn!("{format:?} export: software encoder, NVENC declined: {why}");
            None
        }
    }
}

/// The session for a job, or why NVENC cannot honour it.
fn config(format: OutputFormat, w: u32, h: u32, rate: FrameRate, output: &OutputModule, codes: Codes) -> std::result::Result<Config, String> {
    let hevc = format == OutputFormat::Hevc;
    let profile = if hevc {
        // the export hands 8-bit pictures over, and FilmCraft's NVENC has no constant-quality mode
        if output.codec.profile == CodecProfile::Main10 {
            return Err("HEVC Main 10".into());
        }
        if output.codec.rate_control == RateControlMode::Quality {
            return Err("constant quality".into());
        }
        Profile::HevcMain
    } else {
        Profile::High
    };
    if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err("odd picture size".into());
    }
    // FilmCraft's NVENC H.264 is BT.709 only; HEVC is kept the same so both formats decline alike
    if (codes.primaries, codes.transfer, codes.matrix) != (1, 1, 1) {
        return Err("an output colour space other than BT.709".into());
    }
    let (Ok(num), Ok(den)) = (u32::try_from(rate.num), u32::try_from(rate.den)) else {
        return Err("invalid frame rate".into());
    };
    if num == 0 || den == 0 {
        return Err("invalid frame rate".into());
    }
    let kbps = output.bitrate_kbps.max(100);
    Ok(Config {
        width: w,
        height: h,
        fps: (num, den),
        bitrate_kbps: kbps,
        // the software H.264 encoder's peak (`H264::new`) and FilmCraft's NVENC default
        max_bitrate_kbps: kbps.saturating_add(kbps / 2),
        cbr: false,
        keyint: output.keyint(rate.as_f64()),
        profile,
        // the level is × 10 here as in NVENC's config; the software H.264 encoder ignores it, so does this one
        level: if hevc { output.codec.level } else { None },
        sar: None,
        // shortcut: no B-frames, so pts = dts and no edit list; turn them on (cto, media_start) if quality per bit matters
        bframes: false,
    })
}

fn nv(e: String) -> ExportError {
    ExportError::Encode(format!("NVENC: {e}"))
}

struct Nv {
    encoder: Nvenc,
    w: u32,
    h: u32,
    codes: Codes,
    emitted: i64,
}

impl Nv {
    /// The muxer writes one frame duration per packet, so a dropped or reordered frame must be an
    /// error rather than a movie that silently drifts from its audio. NVENC counts time in frames.
    fn packets(&mut self, packets: Vec<NvPacket>) -> Result<Vec<Packet>> {
        let mut out = Vec::with_capacity(packets.len());
        for p in packets {
            if p.pts != self.emitted || p.dts != self.emitted {
                return Err(nv(format!("returned pts {}, dts {}, expected {}", p.pts, p.dts, self.emitted)));
            }
            self.emitted += 1;
            out.push(Packet { data: p.data, key: p.key, cto: 0 });
        }
        Ok(out)
    }
}

impl VideoEncoder for Nv {
    fn sample_entry(&self) -> SampleEntry {
        let (w, h) = (self.w as u16, self.h as u16);
        let mut entry = match self.encoder.codec() {
            Codec::H264 => {
                let (sps, pps) = self.encoder.parameter_sets();
                SampleEntry::avc(AvcConfig::new(vec![sps.to_vec()], vec![pps.to_vec()], 4), w, h)
            }
            // `Nvenc` built and checked the record when it opened
            Codec::Hevc => SampleEntry::hevc(self.encoder.hevc_config().cloned().unwrap_or_default(), w, h),
        };
        tag_nclx_limited(&mut entry, self.codes);
        entry
    }

    fn encode(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>> {
        if !self.encoder.takes_rgba() {
            return self.encode_yuv420(&Yuv420::from_rgba(rgba, self.w as usize, self.h as usize), index);
        }
        let packets = self.encoder.encode_rgba(rgba, index).map_err(nv)?;
        self.packets(packets)
    }

    fn takes_yuv420(&self) -> bool {
        !self.encoder.takes_rgba()
    }

    fn encode_yuv420(&mut self, frame: &Yuv420, index: u64) -> Result<Vec<Packet>> {
        let (y, u, v) = frame.slices(self.w as usize, self.h as usize);
        let packets = self.encoder.encode(y, u, v, index).map_err(nv)?;
        self.packets(packets)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let packets = self.encoder.flush().map_err(nv)?;
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
        for declined in [&main10, &quality] {
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
    fn the_hevc_level_passes_through_and_h264_ignores_it() {
        let mut om = OutputModule::default();
        om.codec.level = Some(41);
        assert_eq!(config(OutputFormat::Hevc, 320, 180, RATE, &om, BT709).map(|c| c.level), Ok(Some(41)));
        assert_eq!(config(OutputFormat::H264, 320, 180, RATE, &om, BT709).map(|c| c.level), Ok(None));
    }

    #[test]
    fn hardware_session_emits_every_frame_with_parameter_sets() {
        let (w, h) = (320usize, 180usize);
        let om = OutputModule::default();
        for format in [OutputFormat::H264, OutputFormat::Hevc] {
            // a machine without an NVIDIA GPU and driver declines, which is the fallback path
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
