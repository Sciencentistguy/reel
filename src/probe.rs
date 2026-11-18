//! Media information via `ffprobe`.

use std::collections::HashMap;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::model::{AudioStream, MediaInfo, SubtitleStream};

#[derive(Deserialize)]
struct Output {
    #[serde(default)]
    streams: Vec<Stream>,
    format: Option<Format>,
}

#[derive(Deserialize)]
struct Stream {
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    bits_per_raw_sample: Option<String>,
    color_space: Option<String>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    #[serde(default)]
    tags: Tags,
    #[serde(default)]
    disposition: Disposition,
    #[serde(default)]
    side_data_list: Vec<Value>,
}

#[derive(Deserialize, Default)]
struct Tags {
    #[serde(alias = "TITLE")]
    title: Option<String>,
    #[serde(alias = "LANGUAGE")]
    language: Option<String>,
}

#[derive(Deserialize, Default)]
struct Disposition {
    #[serde(default)]
    attached_pic: u8,
    #[serde(default)]
    forced: u8,
    #[serde(default)]
    hearing_impaired: u8,
}

#[derive(Deserialize)]
struct Format {
    duration: Option<String>,
    #[serde(default)]
    tags: Tags,
}

#[derive(Deserialize)]
struct Frames {
    #[serde(default)]
    frames: Vec<Frame>,
}

#[derive(Deserialize)]
struct Frame {
    #[serde(default)]
    side_data_list: Vec<Value>,
}

const PQ: &str = "smpte2084";
const HLG: &str = "arib-std-b67";

fn ffprobe(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json"])
        .args(args)
        .arg(path)
        .output()
        .context("cannot run ffprobe (is ffmpeg installed?)")?;
    if !output.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Just the running time in seconds, which is much cheaper than a full probe.
pub fn duration(path: &Path) -> Result<Option<f64>> {
    Ok(parse_streams(&ffprobe(path, &["-show_format"])?)?.duration)
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    let streams = ffprobe(path, &["-show_format", "-show_streams"])?;
    let mut info = parse_streams(&streams)?;
    // HDR10+ and the Dolby Vision enhancement-layer type are only visible in per-frame metadata.
    let dual_layer_dv = info.dv_profile.as_deref() == Some("7");
    if info.transfer.as_deref() == Some(PQ) || dual_layer_dv {
        let frame_args = [
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#1",
            "-show_frames",
        ];
        match ffprobe(path, &frame_args) {
            Ok(frames) => apply_frame_metadata(&mut info, &frames)?,
            Err(err) => eprintln!("warning: {err:#}"),
        }
    }
    Ok(info)
}

/// Probes several files on background threads, since each probe mostly waits on ffprobe starting
/// up and on disk reads. Files are probed roughly in order so the earliest are ready first, and
/// dropping this stops any probes that have not yet started.
pub struct Background {
    results: Receiver<(usize, Result<MediaInfo>)>,
    ready: HashMap<usize, Result<MediaInfo>>,
    cancelled: Arc<AtomicBool>,
}

impl Background {
    pub fn start(paths: Vec<PathBuf>) -> Self {
        const MAX_WORKERS: usize = 8;
        let workers = thread::available_parallelism()
            .map_or(1, NonZero::get)
            .min(MAX_WORKERS)
            .min(paths.len());
        let paths = Arc::new(paths);
        let next = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, results) = mpsc::channel();
        for _ in 0..workers {
            let (paths, next, cancelled, sender) = (
                paths.clone(),
                next.clone(),
                cancelled.clone(),
                sender.clone(),
            );
            thread::spawn(move || {
                while !cancelled.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index) else { break };
                    if sender.send((index, probe(path))).is_err() {
                        break;
                    }
                }
            });
        }
        Background {
            results,
            ready: HashMap::new(),
            cancelled,
        }
    }

    /// Waits for the probe of the `index`th path.
    pub fn get(&mut self, index: usize) -> Result<MediaInfo> {
        loop {
            if let Some(result) = self.ready.remove(&index) {
                return result;
            }
            let (done, result) = self
                .results
                .recv()
                .map_err(|_| anyhow!("probing stopped before reaching file {index}"))?;
            self.ready.insert(done, result);
        }
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn side_data<'a>(list: &'a [Value], kind: &str) -> Option<&'a Value> {
    list.iter().find(|entry| {
        entry["side_data_type"]
            .as_str()
            .is_some_and(|name| name.contains(kind))
    })
}

fn none_if_unknown(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty() && v != "unknown" && v != "unspecified")
}

fn bit_depth(stream: &Stream) -> Option<u32> {
    stream
        .bits_per_raw_sample
        .as_deref()
        .and_then(|bits| bits.parse().ok())
        .filter(|bits| *bits > 0)
        .or_else(|| {
            let pix_fmt = stream.pix_fmt.as_deref()?;
            Some(match pix_fmt {
                f if f.contains("p16") => 16,
                f if f.contains("p12") => 12,
                f if f.contains("p10") => 10,
                _ => 8,
            })
        })
}

fn hdr_formats(dolby_vision: bool, hdr10_plus: bool, transfer: Option<&str>) -> String {
    let formats: Vec<&str> = [
        (dolby_vision, "DV"),
        (hdr10_plus, "HDR10+"),
        (transfer == Some(PQ), "HDR10"),
        (transfer == Some(HLG), "HLG"),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect();
    if formats.is_empty() {
        "SDR".to_string()
    } else {
        formats.join("/")
    }
}

fn has_format(info: &MediaInfo, format: &str) -> bool {
    info.hdr
        .as_deref()
        .is_some_and(|hdr| hdr.split('/').any(|f| f == format))
}

fn audio_stream(stream: &Stream) -> AudioStream {
    let title = stream.tags.title.clone();
    let mentions_atmos = |text: &Option<String>| {
        text.as_deref()
            .is_some_and(|t| t.to_lowercase().contains("atmos"))
    };
    AudioStream {
        atmos: mentions_atmos(&stream.profile) || mentions_atmos(&title),
        title,
        language: none_if_unknown(stream.tags.language.clone()).filter(|l| l != "und"),
        codec: stream.codec_name.clone(),
        profile: none_if_unknown(stream.profile.clone()),
        channels: stream.channels,
        layout: none_if_unknown(stream.channel_layout.clone()),
        index: stream.index,
    }
}

fn subtitle_stream(stream: &Stream) -> SubtitleStream {
    let title = stream.tags.title.clone();
    let title_mentions = |word: &str| {
        title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(word))
    };
    SubtitleStream {
        forced: stream.disposition.forced != 0 || title_mentions("forced"),
        sdh: stream.disposition.hearing_impaired != 0 || title_mentions("sdh"),
        title,
        language: none_if_unknown(stream.tags.language.clone()).filter(|l| l != "und"),
        codec: stream.codec_name.clone(),
        index: stream.index,
    }
}

fn parse_streams(json: &[u8]) -> Result<MediaInfo> {
    let output: Output = serde_json::from_slice(json).context("unexpected ffprobe output")?;
    let of_type = |kind: &'static str| {
        output.streams.iter().filter(move |s| {
            s.codec_type.as_deref() == Some(kind) && s.disposition.attached_pic == 0
        })
    };
    let mut info = MediaInfo {
        duration: output
            .format
            .as_ref()
            .and_then(|f| f.duration.as_deref())
            .and_then(|d| d.parse().ok()),
        title: output
            .format
            .as_ref()
            .and_then(|f| f.tags.title.clone())
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty()),
        audio: of_type("audio").map(audio_stream).collect(),
        subtitles: of_type("subtitle").map(subtitle_stream).collect(),
        ..MediaInfo::default()
    };
    let Some(video) = of_type("video").next() else {
        return Ok(info);
    };

    info.width = video.width;
    info.height = video.height;
    if let (Some(width), Some(height)) = (video.width, video.height) {
        info.resolution = Some(resolution_label(width, height));
    }
    info.video_codec = video.codec_name.clone();
    info.bit_depth = bit_depth(video);
    info.colour_space = none_if_unknown(video.color_space.clone());
    info.colour_primaries = none_if_unknown(video.color_primaries.clone());
    info.transfer = none_if_unknown(video.color_transfer.clone());

    let dovi = side_data(&video.side_data_list, "DOVI configuration record");
    info.dv_profile = dovi.and_then(|record| {
        let profile = record["dv_profile"].as_u64()?;
        // Profile 8 is subdivided by its base-layer compatibility (8.1 = HDR10, 8.4 = HLG).
        match record["dv_bl_signal_compatibility_id"].as_u64() {
            Some(compatibility) if profile == 8 && compatibility > 0 => {
                Some(format!("{profile}.{compatibility}"))
            }
            _ => Some(profile.to_string()),
        }
    });
    info.hdr = Some(hdr_formats(dovi.is_some(), false, info.transfer.as_deref()));
    Ok(info)
}

/// Classifies a profile 7 enhancement layer from the RPU of one frame. A minimal enhancement
/// layer (MEL) carries no residual, which shows up as zeroed non-linear quantisation parameters.
fn enhancement_layer(metadata: &Value) -> Option<&'static str> {
    let truthy = |value: &Value| value.as_u64() == Some(1) || value.as_bool() == Some(true);
    if truthy(&metadata["disable_residual_flag"]) {
        return Some("MEL");
    }
    let components = metadata["components"].as_array()?;
    let mut saw_nlq = false;
    for component in components {
        if component.get("nlq_offset").is_none() {
            continue;
        }
        saw_nlq = true;
        let residual = [
            "nlq_offset",
            "linear_deadzone_slope",
            "linear_deadzone_threshold",
        ]
        .iter()
        .any(|key| component[key].as_f64().is_some_and(|v| v != 0.0));
        if residual {
            return Some("FEL");
        }
    }
    saw_nlq.then_some("MEL")
}

fn apply_frame_metadata(info: &mut MediaInfo, json: &[u8]) -> Result<()> {
    let frames: Frames = serde_json::from_slice(json).context("unexpected ffprobe output")?;
    let Some(frame) = frames.frames.first() else {
        return Ok(());
    };
    let hdr10_plus = side_data(&frame.side_data_list, "HDR10+").is_some();
    let dolby_vision_metadata = side_data(&frame.side_data_list, "Dolby Vision Metadata");
    let dolby_vision = has_format(info, "DV") || dolby_vision_metadata.is_some();
    info.hdr = Some(hdr_formats(
        dolby_vision,
        hdr10_plus,
        info.transfer.as_deref(),
    ));
    if info.dv_profile.as_deref() == Some("7")
        && let Some(layer) = dolby_vision_metadata.and_then(enhancement_layer)
    {
        info.dv_profile = Some(format!("7 {layer}"));
    }
    Ok(())
}

/// Conventional label for a frame size, tolerant of cropped (letterboxed) encodes.
pub fn resolution_label(width: u32, height: u32) -> String {
    let label = match (width, height) {
        (w, h) if w >= 3800 || h >= 2000 => "2160p",
        (w, h) if w >= 2500 || h >= 1400 => "1440p",
        (w, h) if w >= 1900 || h >= 1000 => "1080p",
        (w, h) if w >= 1260 || h >= 700 => "720p",
        (_, h) if h >= 560 => "576p",
        (_, h) if h >= 400 => "480p",
        _ => "SD",
    };
    label.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAMS: &[u8] = br#"{
        "streams": [
            {"codec_type": "video", "codec_name": "mjpeg", "width": 600, "height": 900,
             "disposition": {"attached_pic": 1}},
            {"codec_type": "video", "codec_name": "hevc", "width": 3840, "height": 1608,
             "pix_fmt": "yuv420p10le", "color_space": "bt2020nc", "color_transfer": "smpte2084",
             "color_primaries": "bt2020", "disposition": {"attached_pic": 0},
             "side_data_list": [{"side_data_type": "DOVI configuration record", "dv_profile": 7,
                                 "dv_level": 6, "rpu_present_flag": 1, "el_present_flag": 1,
                                 "bl_present_flag": 1, "dv_bl_signal_compatibility_id": 6}]},
            {"codec_type": "audio", "codec_name": "truehd",
             "profile": "Dolby TrueHD + Dolby Atmos", "channels": 8, "channel_layout": "7.1",
             "tags": {"language": "eng", "title": "TrueHD Atmos 7.1"}},
            {"codec_type": "audio", "codec_name": "ac3", "profile": "unknown", "channels": 6,
             "channel_layout": "5.1(side)", "tags": {"language": "und"}},
            {"codec_type": "subtitle", "codec_name": "hdmv_pgs_subtitle",
             "disposition": {"forced": 1}, "tags": {"language": "eng"}},
            {"codec_type": "subtitle", "codec_name": "subrip",
             "tags": {"language": "fre", "title": "French (SDH)"}}
        ],
        "format": {"duration": "7265.432000", "tags": {"title": " Heat "}}
    }"#;

    fn frame(components: &str) -> Vec<u8> {
        format!(
            r#"{{"frames": [{{"side_data_list": [
                {{"side_data_type": "Mastering display metadata"}},
                {{"side_data_type": "HDR Dynamic Metadata SMPTE2094-40 (HDR10+)"}},
                {{"side_data_type": "Dolby Vision Metadata", "disable_residual_flag": 0,
                  "components": {components}}}
            ]}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_streams() {
        let info = parse_streams(STREAMS).unwrap();
        assert_eq!((info.width, info.height), (Some(3840), Some(1608)));
        assert_eq!(info.resolution.as_deref(), Some("2160p"));
        assert_eq!(info.video_codec.as_deref(), Some("hevc"));
        assert_eq!(info.bit_depth, Some(10));
        assert_eq!(info.colour_space.as_deref(), Some("bt2020nc"));
        assert_eq!(info.colour_primaries.as_deref(), Some("bt2020"));
        assert_eq!(info.transfer.as_deref(), Some("smpte2084"));
        assert_eq!(info.hdr.as_deref(), Some("DV/HDR10"));
        assert_eq!(info.dv_profile.as_deref(), Some("7"));
        assert_eq!(info.duration, Some(7265.432));
        assert_eq!(info.title.as_deref(), Some("Heat"));

        assert_eq!(info.audio.len(), 2);
        let atmos = &info.audio[0];
        assert!(atmos.atmos);
        assert_eq!(atmos.codec.as_deref(), Some("truehd"));
        assert_eq!(atmos.channels, Some(8));
        assert_eq!(atmos.layout.as_deref(), Some("7.1"));
        assert_eq!(atmos.language.as_deref(), Some("eng"));
        assert_eq!(atmos.title.as_deref(), Some("TrueHD Atmos 7.1"));
        let plain = &info.audio[1];
        assert!(!plain.atmos);
        assert_eq!(
            (plain.profile.as_deref(), plain.language.as_deref()),
            (None, None)
        );

        let [forced, sdh] = &info.subtitles[..] else {
            panic!("expected two subtitle streams");
        };
        assert_eq!(forced.codec.as_deref(), Some("hdmv_pgs_subtitle"));
        assert_eq!((forced.forced, forced.sdh), (true, false));
        assert_eq!(sdh.language.as_deref(), Some("fre"));
        assert_eq!((sdh.forced, sdh.sdh), (false, true));
    }

    #[test]
    fn frame_metadata_refines_hdr() {
        let mut info = parse_streams(STREAMS).unwrap();
        let fel = r#"[{"nlq_offset": 512, "vdr_in_max": 1048576, "linear_deadzone_slope": 2048,
                      "linear_deadzone_threshold": 0}]"#;
        apply_frame_metadata(&mut info, &frame(fel)).unwrap();
        assert_eq!(info.hdr.as_deref(), Some("DV/HDR10+/HDR10"));
        assert_eq!(info.dv_profile.as_deref(), Some("7 FEL"));

        let mut info = parse_streams(STREAMS).unwrap();
        let mel = r#"[{"nlq_offset": 0, "vdr_in_max": 1048576, "linear_deadzone_slope": 0,
                      "linear_deadzone_threshold": 0}]"#;
        apply_frame_metadata(&mut info, &frame(mel)).unwrap();
        assert_eq!(info.dv_profile.as_deref(), Some("7 MEL"));

        let mut info = parse_streams(STREAMS).unwrap();
        apply_frame_metadata(&mut info, &frame("[{}]")).unwrap();
        assert_eq!(info.dv_profile.as_deref(), Some("7"));
    }

    #[test]
    fn profile_8_and_sdr() {
        let json = br#"{"streams": [{"codec_type": "video", "codec_name": "hevc",
            "width": 1920, "height": 1080, "bits_per_raw_sample": "10",
            "color_transfer": "arib-std-b67",
            "side_data_list": [{"side_data_type": "DOVI configuration record",
                                "dv_profile": 8, "dv_bl_signal_compatibility_id": 4}]}]}"#;
        let info = parse_streams(json).unwrap();
        assert_eq!(info.dv_profile.as_deref(), Some("8.4"));
        assert_eq!(info.hdr.as_deref(), Some("DV/HLG"));

        let json = br#"{"streams": [{"codec_type": "video", "codec_name": "h264",
            "width": 1280, "height": 720, "pix_fmt": "yuv420p", "color_space": "bt709"}]}"#;
        let info = parse_streams(json).unwrap();
        assert_eq!(info.hdr.as_deref(), Some("SDR"));
        assert_eq!(info.bit_depth, Some(8));
        assert_eq!(info.dv_profile, None);
    }

    #[test]
    fn labels() {
        assert_eq!(resolution_label(3840, 1600), "2160p");
        assert_eq!(resolution_label(1920, 804), "1080p");
        assert_eq!(resolution_label(1280, 720), "720p");
        assert_eq!(resolution_label(720, 576), "576p");
        assert_eq!(resolution_label(640, 480), "480p");
    }
}
