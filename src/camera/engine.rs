//! The change engine: how much the picture moves, as a bitrate, and when a
//! session keeps a frame.
//!
//! Every sample is shrunk to a small grayscale thumbnail and subtracted
//! from the previous one; the difference is JPEG-encoded and its size, less
//! the size of an empty difference, is what a video encoder would spend on
//! that change. Bits per second over a short window is the **change
//! bitrate** (kbps). A still scene costs almost nothing; motion costs more
//! the more of the picture it covers and the faster it is.
//!
//! [`Pacer`] turns that into kept frames: [`CaptureMode::Fixed`] keeps one
//! every `1 / fps` seconds whatever happens; [`CaptureMode::Dynamic`] keeps
//! none while the scene is still and more the more it changes, from
//! [`MIN_DYNAMIC_FPS`] at the sensitivity's threshold up to
//! [`MAX_DYNAMIC_FPS`].

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// How often the camera is sampled.
pub(crate) const SAMPLE_INTERVAL_MS: u64 = 200;
/// Thumbnail width the change is measured on.
const THUMB_WIDTH: u32 = 160;
/// JPEG quality of the difference image: low enough that sensor noise
/// mostly quantizes away.
const DIFF_QUALITY: u8 = 60;
/// The change bitrate averages over this long: smooth enough to ignore a
/// single odd frame, short enough that capture reacts within a second.
const WINDOW_MS: u64 = 1_000;
/// The first this long only measures the scene's noise floor.
pub(crate) const BASELINE_MS: u64 = 1_000;
/// Dynamic capture keeps going this long after the change drops below the
/// threshold, so a pause in a motion does not cut it off.
pub(crate) const HOLD_MS: u64 = 3_000;
pub(crate) const MIN_DYNAMIC_FPS: f64 = 0.5;
pub(crate) const MAX_DYNAMIC_FPS: f64 = 2.0;
/// Fixed capture bounds.
pub(crate) const MIN_FIXED_FPS: f64 = 0.2;
pub(crate) const MAX_FIXED_FPS: f64 = 5.0;
/// Change bitrate (kbps above the noise floor) that starts dynamic capture
/// at the default sensitivity. Calibrated with [`calibration`] (1280×720):
/// sensor noise, even in low light, and exposure drift measure ~0; a slow
/// hand-sized object ~5; a small or finger-sized object ~1; a large fast
/// one ~15. So the default catches a hand, sensitivity 100 (0.5) a small
/// object, sensitivity 1 (~8) only large, fast motion.
const THRESHOLD_AT_DEFAULT_KBPS: f64 = 2.0;
pub(crate) const DEFAULT_SENSITIVITY: u8 = 50;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CaptureMode {
    /// Evenly spaced frames at a set rate.
    Fixed,
    /// Frames only while the picture changes, denser the more it changes.
    #[default]
    Dynamic,
}

/// How a session picks its frames.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureSettings {
    #[serde(default)]
    pub mode: CaptureMode,
    /// Frames per second ([`CaptureMode::Fixed`]).
    #[serde(default = "default_fps")]
    pub fps: f64,
    /// 1 (only big changes) to 100 (the slightest change)
    /// ([`CaptureMode::Dynamic`]).
    #[serde(default = "default_sensitivity")]
    pub sensitivity: u8,
}

fn default_fps() -> f64 {
    1.0
}

fn default_sensitivity() -> u8 {
    DEFAULT_SENSITIVITY
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            mode: CaptureMode::Dynamic,
            fps: default_fps(),
            sensitivity: default_sensitivity(),
        }
    }
}

impl CaptureSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.fps.is_finite() || !(MIN_FIXED_FPS..=MAX_FIXED_FPS).contains(&self.fps) {
            return Err(format!(
                "fps must be between {MIN_FIXED_FPS} and {MAX_FIXED_FPS}"
            ));
        }
        if !(1..=100).contains(&self.sensitivity) {
            return Err("sensitivity must be between 1 and 100".to_owned());
        }
        Ok(())
    }

    /// One line for prompts, results and the manifest.
    pub(crate) fn describe(&self) -> String {
        match self.mode {
            CaptureMode::Fixed => format!("fixed, {} frame(s) per second", trim(self.fps)),
            CaptureMode::Dynamic => format!("dynamic, sensitivity {}", self.sensitivity),
        }
    }
}

fn trim(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Change bitrate (kbps above the noise floor) at which dynamic capture
/// starts: halves for every 25 points of sensitivity above the default.
pub(crate) fn threshold_kbps(sensitivity: u8) -> f64 {
    let sensitivity = f64::from(sensitivity.clamp(1, 100));
    THRESHOLD_AT_DEFAULT_KBPS * 2f64.powf((f64::from(DEFAULT_SENSITIVITY) - sensitivity) / 25.0)
}

/// A grayscale thumbnail [`THUMB_WIDTH`] wide (box filtered) of a BGRA
/// frame.
pub(crate) fn thumbnail(bgra: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let thumb_width = THUMB_WIDTH.min(width).max(1);
    let thumb_height =
        ((u64::from(height) * u64::from(thumb_width)) / u64::from(width.max(1))).max(1) as u32;
    let mut out = vec![0u8; (thumb_width * thumb_height) as usize];
    for ty in 0..thumb_height {
        let y0 = ty * height / thumb_height;
        let y1 = ((ty + 1) * height / thumb_height).max(y0 + 1).min(height);
        for tx in 0..thumb_width {
            let x0 = tx * width / thumb_width;
            let x1 = ((tx + 1) * width / thumb_width).max(x0 + 1).min(width);
            let mut sum = 0u64;
            let mut count = 0u64;
            for y in y0..y1 {
                let row = (y * width) as usize * 4;
                for x in x0..x1 {
                    let pixel = row + x as usize * 4;
                    let (b, g, r) = (
                        u64::from(bgra[pixel]),
                        u64::from(bgra[pixel + 1]),
                        u64::from(bgra[pixel + 2]),
                    );
                    // BT.601 luma, in 1/256ths.
                    sum += (77 * r + 150 * g + 29 * b) >> 8;
                    count += 1;
                }
            }
            out[(ty * thumb_width + tx) as usize] = (sum / count.max(1)) as u8;
        }
    }
    (out, thumb_width, thumb_height)
}

fn encode_luma(pixels: &[u8], width: u32, height: u32, quality: u8) -> usize {
    let mut jpeg = Vec::new();
    match jpeg_encoder::Encoder::new(&mut jpeg, quality).encode(
        pixels,
        width as u16,
        height as u16,
        jpeg_encoder::ColorType::Luma,
    ) {
        Ok(()) => jpeg.len(),
        Err(_) => 0,
    }
}

/// Measures the change bitrate of a stream of thumbnails.
#[derive(Default)]
pub(crate) struct ChangeMeter {
    previous: Option<(Vec<u8>, u32, u32)>,
    /// Encoded size of an all-zero difference at the thumbnail size.
    empty_bytes: usize,
    /// (time, bits) of the recent samples.
    window: VecDeque<(u64, u64)>,
    /// Noise floor (kbps) measured over [`BASELINE_MS`].
    baseline: Option<f64>,
    baseline_samples: Vec<f64>,
}

impl ChangeMeter {
    /// Adds the thumbnail taken at `at_ms` (since the session started);
    /// returns the change bitrate above the noise floor, in kbps.
    pub(crate) fn sample(&mut self, at_ms: u64, thumb: (Vec<u8>, u32, u32)) -> f64 {
        let (pixels, width, height) = &thumb;
        let bits = match &self.previous {
            Some((previous, previous_width, previous_height))
                if previous_width == width && previous_height == height =>
            {
                let difference: Vec<u8> = previous
                    .iter()
                    .zip(pixels.iter())
                    .map(|(a, b)| a.abs_diff(*b))
                    .collect();
                if self.empty_bytes == 0 {
                    self.empty_bytes =
                        encode_luma(&vec![0u8; difference.len()], *width, *height, DIFF_QUALITY);
                }
                let encoded = encode_luma(&difference, *width, *height, DIFF_QUALITY);
                (encoded.saturating_sub(self.empty_bytes) * 8) as u64
            }
            // First sample, or the camera changed size: nothing to compare.
            _ => {
                self.empty_bytes = 0;
                0
            }
        };
        self.previous = Some(thumb);
        self.window.push_back((at_ms, bits));
        while self
            .window
            .front()
            .is_some_and(|(at, _)| at_ms.saturating_sub(*at) >= WINDOW_MS)
        {
            self.window.pop_front();
        }
        let span_ms =
            (self.window.len() as u64 * SAMPLE_INTERVAL_MS).clamp(SAMPLE_INTERVAL_MS, WINDOW_MS);
        let raw = self.window.iter().map(|(_, bits)| *bits).sum::<u64>() as f64
            / (span_ms as f64 / 1000.0)
            / 1000.0;
        match self.baseline {
            Some(floor) => (raw - floor).max(0.0),
            None => {
                if at_ms >= SAMPLE_INTERVAL_MS {
                    self.baseline_samples.push(raw);
                }
                if at_ms >= BASELINE_MS && !self.baseline_samples.is_empty() {
                    let mut samples = self.baseline_samples.clone();
                    samples.sort_by(f64::total_cmp);
                    // The median: a hand passing through the first second
                    // must not raise the floor.
                    self.baseline = Some(samples[samples.len() / 2]);
                }
                0.0
            }
        }
    }

    /// The measured noise floor (kbps), once known.
    #[cfg(test)]
    pub(crate) fn baseline(&self) -> Option<f64> {
        self.baseline
    }
}

/// Why a frame was kept.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum KeepReason {
    /// The session's first frame.
    First,
    /// Evenly spaced ([`CaptureMode::Fixed`]).
    Interval,
    /// The picture changed past the threshold.
    Change,
    /// Shortly after a change, while it may resume.
    Hold,
    /// The session's last frame.
    Last,
}

/// Decides which samples a session keeps. Each sample earns `rate × time
/// since the previous sample` frames of credit and a frame is kept per
/// whole frame earned, so the average rate is exact although samples come
/// every [`SAMPLE_INTERVAL_MS`]. A change starting after a still stretch is
/// kept at once: the start of a motion matters most.
pub(crate) struct Pacer {
    settings: CaptureSettings,
    threshold: f64,
    previous_at: Option<u64>,
    credit: f64,
    /// Dynamic capture keeps holding until then.
    active_until: Option<u64>,
}

/// One decision: keep the sample, and the capture rate in effect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Decision {
    pub keep: Option<KeepReason>,
    pub rate_fps: f64,
}

impl Pacer {
    pub(crate) fn new(settings: CaptureSettings) -> Self {
        Self {
            threshold: threshold_kbps(settings.sensitivity),
            settings,
            previous_at: None,
            credit: 0.0,
            active_until: None,
        }
    }

    /// The change bitrate (kbps) dynamic capture starts at.
    pub(crate) fn threshold(&self) -> f64 {
        self.threshold
    }

    /// `change_kbps` from [`ChangeMeter::sample`] for the sample at `at_ms`.
    pub(crate) fn decide(&mut self, at_ms: u64, change_kbps: f64) -> Decision {
        let was_active = self.active_until.is_some_and(|until| at_ms < until);
        let (rate, reason) = match self.settings.mode {
            CaptureMode::Fixed => (self.settings.fps, KeepReason::Interval),
            CaptureMode::Dynamic => {
                if at_ms >= BASELINE_MS && change_kbps >= self.threshold {
                    self.active_until = Some(at_ms + HOLD_MS);
                    let rate = (MIN_DYNAMIC_FPS * change_kbps / self.threshold)
                        .clamp(MIN_DYNAMIC_FPS, MAX_DYNAMIC_FPS);
                    (rate, KeepReason::Change)
                } else if was_active {
                    (MIN_DYNAMIC_FPS, KeepReason::Hold)
                } else {
                    (0.0, KeepReason::Change)
                }
            }
        };
        let elapsed = self
            .previous_at
            .map_or(0, |previous| at_ms.saturating_sub(previous));
        let first = self.previous_at.is_none();
        self.previous_at = Some(at_ms);
        let keep = if first {
            self.credit = 0.0;
            Some(KeepReason::First)
        } else if rate <= 0.0 {
            self.credit = 0.0;
            None
        } else if self.settings.mode == CaptureMode::Dynamic && !was_active {
            // A change after a still stretch.
            self.credit = 0.0;
            Some(reason)
        } else {
            self.credit += rate * elapsed as f64 / 1000.0;
            // Float sums of 0.2 s steps land a hair under whole frames.
            if self.credit >= 1.0 - 1e-6 {
                self.credit = (self.credit - 1.0).max(0.0);
                Some(reason)
            } else {
                None
            }
        };
        Decision {
            keep,
            rate_fps: rate,
        }
    }
}

/// `frame_0007_t00m12s500.jpg`: the frame number, then the time since the
/// session started. Names sort in time order.
pub(crate) fn frame_name(number: usize, offset_ms: u64) -> String {
    let minutes = offset_ms / 60_000;
    let seconds = (offset_ms / 1000) % 60;
    let millis = offset_ms % 1000;
    format!("frame_{number:04}_t{minutes:02}m{seconds:02}s{millis:03}.jpg")
}

/// `mm:ss.s`, for result text.
pub(crate) fn clock(offset_ms: u64) -> String {
    format!(
        "{:02}:{:02}.{}",
        offset_ms / 60_000,
        (offset_ms / 1000) % 60,
        (offset_ms % 1000) / 100
    )
}

/// A stretch of time the picture kept changing.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Activity {
    pub from_ms: u64,
    pub to_ms: u64,
    pub peak_kbps: f64,
}

/// Collects [`Activity`] spans from samples over a threshold; gaps shorter
/// than [`HOLD_MS`] join two spans.
#[derive(Default)]
pub(crate) struct ActivityLog {
    spans: Vec<Activity>,
}

impl ActivityLog {
    pub(crate) fn sample(&mut self, at_ms: u64, change_kbps: f64, threshold: f64) {
        if at_ms < BASELINE_MS || change_kbps < threshold {
            return;
        }
        match self.spans.last_mut() {
            Some(span) if at_ms.saturating_sub(span.to_ms) <= HOLD_MS => {
                span.to_ms = at_ms;
                span.peak_kbps = span.peak_kbps.max(change_kbps);
            }
            _ => self.spans.push(Activity {
                from_ms: at_ms,
                to_ms: at_ms,
                peak_kbps: change_kbps,
            }),
        }
    }

    pub(crate) fn spans(&self) -> &[Activity] {
        &self.spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, fill: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut bgra = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let value = fill(x, y);
                bgra.extend_from_slice(&[value, value, value, 255]);
            }
        }
        bgra
    }

    /// A deterministic pseudo-random byte per (x, y, seed).
    fn noise(x: u32, y: u32, seed: u32) -> u8 {
        let mut hash = x.wrapping_mul(374_761_393)
            ^ y.wrapping_mul(668_265_263)
            ^ seed.wrapping_mul(2_246_822_519);
        hash = (hash ^ (hash >> 13)).wrapping_mul(1_274_126_177);
        (hash >> 24) as u8
    }

    /// Change bitrates for `samples` frames drawn by `scene(sample)`.
    fn measure(samples: u64, scene: impl Fn(u64) -> Vec<u8>) -> Vec<f64> {
        let mut meter = ChangeMeter::default();
        (0..samples)
            .map(|sample| {
                let bgra = scene(sample);
                meter.sample(sample * SAMPLE_INTERVAL_MS, thumbnail(&bgra, 320, 180))
            })
            .collect()
    }

    #[test]
    fn thumbnails_average_to_luma() {
        let bgra = frame(320, 180, |x, _| if x < 160 { 0 } else { 200 });
        let (thumb, width, height) = thumbnail(&bgra, 320, 180);
        assert_eq!((width, height), (160, 90));
        assert_eq!(thumb[0], 0);
        assert!((thumb[159] as i32 - 199).abs() <= 1, "{}", thumb[159]);
    }

    #[test]
    fn a_still_scene_measures_no_change_and_motion_does() {
        let still = measure(20, |_| frame(320, 180, |x, y| (x + y) as u8));
        assert!(still.iter().all(|kbps| *kbps == 0.0), "{still:?}");
        // A bright square moving across the picture after the baseline.
        let moving = measure(30, |sample| {
            let left = (sample * 12) as u32;
            frame(320, 180, |x, y| {
                if sample >= 10 && (left..left + 60).contains(&x) && (40..120).contains(&y) {
                    250
                } else {
                    20
                }
            })
        });
        assert!(moving[..10].iter().all(|kbps| *kbps == 0.0));
        // Past the default threshold within a second of starting.
        assert!(
            moving[15] > threshold_kbps(DEFAULT_SENSITIVITY),
            "{moving:?}"
        );
    }

    #[test]
    fn the_noise_floor_is_subtracted() {
        // Sensor-like noise on a still scene.
        let noisy = measure(30, |sample| {
            frame(320, 180, |x, y| 100 + noise(x, y, sample as u32) / 32)
        });
        let mut meter = ChangeMeter::default();
        for sample in 0..10u64 {
            meter.sample(
                sample * SAMPLE_INTERVAL_MS,
                thumbnail(
                    &frame(320, 180, |x, y| 100 + noise(x, y, sample as u32) / 32),
                    320,
                    180,
                ),
            );
        }
        assert!(meter.baseline().is_some());
        let after = &noisy[10..];
        let mean = after.iter().sum::<f64>() / after.len() as f64;
        assert!(
            mean < threshold_kbps(100),
            "noise reads as {mean} kbps: {noisy:?}"
        );
    }

    #[test]
    fn higher_sensitivity_means_a_lower_threshold() {
        assert_eq!(threshold_kbps(DEFAULT_SENSITIVITY), 2.0);
        assert!(threshold_kbps(100) < threshold_kbps(75));
        assert!(threshold_kbps(75) < threshold_kbps(50));
        assert!(threshold_kbps(50) < threshold_kbps(1));
        assert!((threshold_kbps(100) - 0.5).abs() < 1e-9);
    }

    fn kept(
        settings: CaptureSettings,
        change: impl Fn(u64) -> f64,
        until_ms: u64,
    ) -> Vec<(u64, KeepReason)> {
        let mut pacer = Pacer::new(settings);
        (0..=until_ms / SAMPLE_INTERVAL_MS)
            .map(|sample| sample * SAMPLE_INTERVAL_MS)
            .filter_map(|at| pacer.decide(at, change(at)).keep.map(|reason| (at, reason)))
            .collect()
    }

    #[test]
    fn fixed_capture_is_evenly_spaced_whatever_changes() {
        let settings = CaptureSettings {
            mode: CaptureMode::Fixed,
            fps: 1.0,
            ..CaptureSettings::default()
        };
        let frames = kept(settings, |at| if at > 3000 { 500.0 } else { 0.0 }, 10_000);
        let times: Vec<u64> = frames.iter().map(|(at, _)| *at).collect();
        assert_eq!(
            times,
            (0..=10).map(|second| second * 1000).collect::<Vec<_>>()
        );
        let fast = kept(
            CaptureSettings {
                fps: 5.0,
                ..settings
            },
            |_| 0.0,
            2000,
        );
        assert_eq!(fast.len(), 11);
        let slow = kept(
            CaptureSettings {
                fps: 0.5,
                ..settings
            },
            |_| 0.0,
            10_000,
        );
        assert_eq!(slow.len(), 6);
    }

    #[test]
    fn dynamic_capture_follows_the_change() {
        let settings = CaptureSettings::default();
        let threshold = threshold_kbps(settings.sensitivity);
        // Still, then a big change from 5 s to 10 s, then still again.
        let frames = kept(
            settings,
            |at| {
                if (5000..10_000).contains(&at) {
                    threshold * 4.0
                } else {
                    0.0
                }
            },
            30_000,
        );
        assert_eq!(frames[0], (0, KeepReason::First));
        // Nothing while still.
        assert!(frames.iter().all(|(at, _)| *at == 0 || *at >= 5000));
        let change: Vec<u64> = frames
            .iter()
            .filter(|(_, reason)| *reason == KeepReason::Change)
            .map(|(at, _)| *at)
            .collect();
        // Four times the threshold: the top rate, two per second, starting
        // with the onset itself.
        assert_eq!(change[0], 5000);
        assert!((9..=11).contains(&change.len()), "{frames:?}");
        // A hold after it, then nothing more.
        let hold: Vec<u64> = frames
            .iter()
            .filter(|(_, reason)| *reason == KeepReason::Hold)
            .map(|(at, _)| *at)
            .collect();
        assert!(
            !hold.is_empty() && hold.iter().all(|at| *at < 10_000 + HOLD_MS),
            "{frames:?}"
        );
        assert!(frames.iter().all(|(at, _)| *at < 10_000 + HOLD_MS));
    }

    #[test]
    fn bigger_changes_and_higher_sensitivity_keep_more() {
        let count = |sensitivity: u8, kbps: f64| {
            kept(
                CaptureSettings {
                    sensitivity,
                    ..CaptureSettings::default()
                },
                |at| if at >= BASELINE_MS { kbps } else { 0.0 },
                20_000,
            )
            .len()
        };
        let threshold = threshold_kbps(50);
        assert!(count(50, threshold * 1.5) < count(50, threshold * 3.0));
        // Below the default threshold but above a sensitive one.
        assert_eq!(count(50, threshold * 0.75), 1);
        assert!(count(90, threshold * 0.75) > 1);
    }

    #[test]
    fn nothing_triggers_during_the_baseline() {
        let frames = kept(
            CaptureSettings::default(),
            |_| 1000.0,
            BASELINE_MS - SAMPLE_INTERVAL_MS,
        );
        assert_eq!(frames, vec![(0, KeepReason::First)]);
    }

    #[test]
    fn frame_names_carry_the_time_and_sort_in_order() {
        assert_eq!(frame_name(7, 12_500), "frame_0007_t00m12s500.jpg");
        assert_eq!(frame_name(120, 3_599_999), "frame_0120_t59m59s999.jpg");
        let mut names = vec![frame_name(3, 2_000), frame_name(1, 0), frame_name(2, 500)];
        names.sort();
        assert_eq!(
            names,
            vec![frame_name(1, 0), frame_name(2, 500), frame_name(3, 2_000)]
        );
        assert_eq!(clock(83_250), "01:23.2");
    }

    #[test]
    fn activity_spans_join_short_gaps() {
        let mut log = ActivityLog::default();
        for at in [2000, 2200, 2400, 4000, 9000, 9200] {
            log.sample(at, 50.0, 20.0);
        }
        log.sample(500, 999.0, 20.0);
        log.sample(9400, 5.0, 20.0);
        assert_eq!(
            log.spans(),
            &[
                Activity {
                    from_ms: 2000,
                    to_ms: 4000,
                    peak_kbps: 50.0
                },
                Activity {
                    from_ms: 9000,
                    to_ms: 9200,
                    peak_kbps: 50.0
                },
            ]
        );
    }

    #[test]
    fn settings_are_validated() {
        assert!(CaptureSettings::default().validate().is_ok());
        assert!(CaptureSettings {
            fps: 0.1,
            ..CaptureSettings::default()
        }
        .validate()
        .is_err());
        assert!(CaptureSettings {
            fps: f64::NAN,
            ..CaptureSettings::default()
        }
        .validate()
        .is_err());
        assert!(CaptureSettings {
            sensitivity: 0,
            ..CaptureSettings::default()
        }
        .validate()
        .is_err());
        assert_eq!(
            CaptureSettings::default().describe(),
            "dynamic, sensitivity 50"
        );
        assert_eq!(
            CaptureSettings {
                mode: CaptureMode::Fixed,
                fps: 0.5,
                ..CaptureSettings::default()
            }
            .describe(),
            "fixed, 0.5 frame(s) per second"
        );
    }
}

/// Prints the change bitrate of synthetic 1280×720 scenes (still, noisy,
/// drifting exposure, objects of several sizes and speeds) to recalibrate
/// [`THRESHOLD_AT_DEFAULT_KBPS`]:
/// `cargo test --release --bin todex-agentd camera::engine::calibration -- --ignored --nocapture`.
#[cfg(test)]
mod calibration {
    use super::*;

    fn noise(x: u32, y: u32, seed: u32) -> u8 {
        let mut hash = x.wrapping_mul(374_761_393)
            ^ y.wrapping_mul(668_265_263)
            ^ seed.wrapping_mul(2_246_822_519);
        hash = (hash ^ (hash >> 13)).wrapping_mul(1_274_126_177);
        (hash >> 24) as u8
    }

    fn run(name: &str, scene: impl Fn(u64, u32, u32) -> u8) {
        let (w, h) = (1280u32, 720u32);
        let mut meter = ChangeMeter::default();
        let mut values = Vec::new();
        for sample in 0..25u64 {
            let mut bgra = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    let v = scene(sample, x, y);
                    bgra.extend_from_slice(&[v, v, v, 255]);
                }
            }
            values.push(meter.sample(sample * SAMPLE_INTERVAL_MS, thumbnail(&bgra, w, h)));
        }
        let tail = &values[10..];
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        let max = tail.iter().cloned().fold(0.0, f64::max);
        eprintln!(
            "{name:40} mean {mean:7.1} max {max:7.1} baseline {:?}",
            meter.baseline().map(|b| (b * 10.0).round() / 10.0)
        );
    }

    #[test]
    #[ignore = "prints calibration levels"]
    fn print_levels() {
        let background =
            |x: u32, y: u32| 60 + ((x / 64 + y / 64) % 2) as u8 * 40 + (noise(x, y, 0) / 16);
        let lownoise = |s: u64, x: u32, y: u32| {
            background(x, y).saturating_add(noise(x, y, s as u32 + 1) / 64)
        };
        run("still, normal light", lownoise);
        run("still, low light (heavy noise)", |s, x, y| {
            background(x, y).saturating_add(noise(x, y, s as u32 + 1) / 8)
        });
        run("exposure drift +1/sample", |s, x, y| {
            lownoise(s, x, y).saturating_add(s as u8)
        });
        let object = |s: u64,
                      x: u32,
                      y: u32,
                      size: u32,
                      speed: u32,
                      textured: bool,
                      from: u64|
         -> Option<u8> {
            if s < from {
                return None;
            }
            let left = 200 + ((s - from) as u32 * speed) % 800;
            ((left..left + size).contains(&x) && (200..200 + size).contains(&y)).then(|| {
                if textured {
                    120 + noise(x - left, y, 99) / 2
                } else {
                    220
                }
            })
        };
        // Speeds are pixels per sample (200 ms).
        for (label, size, speed, textured) in [
            ("hand-size textured, fast (240px, 40px)", 240, 40, true),
            ("hand-size textured, slow (240px, 8px)", 240, 8, true),
            ("hand-size flat, fast (240px, 40px)", 240, 40, false),
            ("small textured, slow (64px, 4px)", 64, 4, true),
            ("finger-size textured (32px, 10px)", 32, 10, true),
            ("big textured, fast (480px, 60px)", 480, 60, true),
        ] {
            run(label, move |s, x, y| {
                object(s, x, y, size, speed, textured, 6).unwrap_or_else(|| lownoise(s, x, y))
            });
        }
    }
}
