//! What `camera_stop` shows the agent inline, so it rarely has to open
//! frame files one by one: the frames themselves when a session kept few
//! ([`INLINE_FRAMES_MAX`]), otherwise up to [`SHEETS_MAX`] overview sheets,
//! grids of small frames each labelled with its number and time.
//!
//! While a session records, [`Collector`] keeps a small JPEG of each of the
//! first frames and a thumbnail of evenly spaced frames: when it holds more
//! than [`KEEP_MAX`] thumbnails it keeps every other one, so memory stays
//! bounded however long the session runs. The last frame is always kept.

use super::{encode_jpeg, CameraError, Frame};

/// Up to this many frames are sent as they are.
pub(crate) const INLINE_FRAMES_MAX: usize = 12;
/// Beyond this many frames the agent is told to use sub-agents.
pub(crate) const SUBAGENT_FRAMES_MIN: usize = 100;
const INLINE_WIDTH: u32 = 512;
const INLINE_QUALITY: u8 = 65;
/// Sheet cells: [`COLUMNS`] × [`ROWS`] of this size, [`GAP`] apart.
pub(crate) const CELL_WIDTH: u32 = 256;
pub(crate) const CELL_HEIGHT: u32 = 144;
pub(crate) const COLUMNS: usize = 4;
pub(crate) const ROWS: usize = 4;
const GAP: u32 = 4;
pub(crate) const SHEETS_MAX: usize = 3;
const SHEET_QUALITY: u8 = 70;
/// Thumbnails held while recording.
const KEEP_MAX: usize = 96;
/// Label glyphs are 5×7 pixels, drawn this many times larger.
const LABEL_SCALE: u32 = 2;
const BACKGROUND: [u8; 3] = [24, 24, 24];

/// A frame shrunk to a sheet cell, with what its label says.
pub(crate) struct Thumb {
    pub number: usize,
    pub offset_ms: u64,
    pub cell: Frame,
}

/// One image `camera_stop` sends inline.
#[derive(Clone, Debug)]
pub(crate) struct OverviewImage {
    /// The sheet's file in the session's directory (`None` for a frame
    /// sent as it is: its own file is the frame's).
    pub file: Option<String>,
    /// The frame numbers it shows, in order.
    pub frames: Vec<usize>,
    pub jpeg: Vec<u8>,
}

/// What a saved frame contributes, made off the async threads.
pub(crate) struct Extras {
    pub cell: Frame,
    /// A small JPEG, for the first [`INLINE_FRAMES_MAX`] frames.
    pub small: Option<Vec<u8>>,
}

/// The overview's share of one frame: a cell thumbnail and, when `small`,
/// a small JPEG.
pub(crate) fn extras(frame: &Frame, small: bool) -> Result<Extras, CameraError> {
    Ok(Extras {
        cell: fit(frame, CELL_WIDTH, CELL_HEIGHT),
        small: if small {
            Some(encode_jpeg(frame, INLINE_WIDTH, INLINE_QUALITY)?)
        } else {
            None
        },
    })
}

#[derive(Default)]
pub(crate) struct Collector {
    small: Vec<(usize, Vec<u8>)>,
    thumbs: Vec<Thumb>,
    /// Thumbnails are kept for frame numbers `1, 1 + stride, ...`.
    stride: usize,
    /// The newest frame's thumbnail when it is not among `thumbs`.
    last: Option<Thumb>,
    count: usize,
}

impl Collector {
    /// Frame `number` (1-based, in order) was saved.
    pub(crate) fn add(&mut self, number: usize, offset_ms: u64, extras: Extras) {
        self.count = number;
        if self.stride == 0 {
            self.stride = 1;
        }
        match extras.small {
            Some(jpeg) if number <= INLINE_FRAMES_MAX => self.small.push((number, jpeg)),
            // Too many to send as they are: the sheets stand in.
            _ => self.small.clear(),
        }
        let thumb = Thumb {
            number,
            offset_ms,
            cell: extras.cell,
        };
        if !(number - 1).is_multiple_of(self.stride) {
            self.last = Some(thumb);
            return;
        }
        self.last = None;
        self.thumbs.push(thumb);
        if self.thumbs.len() > KEEP_MAX {
            self.stride *= 2;
            let stride = self.stride;
            self.thumbs
                .retain(|thumb| (thumb.number - 1).is_multiple_of(stride));
        }
    }

    /// The thumbnails to show: evenly spread over what was kept, the first
    /// and the last frame included, at most `limit`.
    fn chosen(&self, limit: usize) -> Vec<&Thumb> {
        let mut all: Vec<&Thumb> = self.thumbs.iter().collect();
        if let Some(last) = &self.last {
            all.push(last);
        }
        if all.len() <= limit {
            return all;
        }
        (0..limit)
            .map(|index| all[index * (all.len() - 1) / (limit - 1)])
            .collect()
    }

    /// The inline images: the frames themselves when few, else sheets.
    pub(crate) fn finish(&self) -> Result<Vec<OverviewImage>, CameraError> {
        if self.count == 0 {
            return Ok(Vec::new());
        }
        if self.count <= INLINE_FRAMES_MAX && self.small.len() == self.count {
            return Ok(self
                .small
                .iter()
                .map(|(number, jpeg)| OverviewImage {
                    file: None,
                    frames: vec![*number],
                    jpeg: jpeg.clone(),
                })
                .collect());
        }
        let per_sheet = COLUMNS * ROWS;
        self.chosen(per_sheet * SHEETS_MAX)
            .chunks(per_sheet)
            .enumerate()
            .map(|(index, cells)| {
                let sheet = sheet(cells);
                Ok(OverviewImage {
                    file: Some(format!("overview_{}.jpg", index + 1)),
                    frames: cells.iter().map(|thumb| thumb.number).collect(),
                    jpeg: encode_jpeg(&sheet, sheet.width, SHEET_QUALITY)?,
                })
            })
            .collect()
    }
}

/// `frame` scaled to fit `width` × `height` (box filter), centered on the
/// background.
pub(crate) fn fit(frame: &Frame, width: u32, height: u32) -> Frame {
    let scale = (f64::from(width) / f64::from(frame.width.max(1)))
        .min(f64::from(height) / f64::from(frame.height.max(1)));
    let inner_width = ((f64::from(frame.width) * scale).round() as u32).clamp(1, width);
    let inner_height = ((f64::from(frame.height) * scale).round() as u32).clamp(1, height);
    let left = (width - inner_width) / 2;
    let top = (height - inner_height) / 2;
    let mut out = filled(width, height);
    for y in 0..inner_height {
        let y0 = y * frame.height / inner_height;
        let y1 = ((y + 1) * frame.height / inner_height).clamp(y0 + 1, frame.height);
        for x in 0..inner_width {
            let x0 = x * frame.width / inner_width;
            let x1 = ((x + 1) * frame.width / inner_width).clamp(x0 + 1, frame.width);
            let mut sums = [0u64; 3];
            for sy in y0..y1 {
                let row = (sy * frame.width) as usize * 4;
                for sx in x0..x1 {
                    let pixel = row + sx as usize * 4;
                    for (sum, value) in sums.iter_mut().zip(&frame.bgra[pixel..pixel + 3]) {
                        *sum += u64::from(*value);
                    }
                }
            }
            let count = u64::from((y1 - y0) * (x1 - x0));
            let at = (((top + y) * width + left + x) * 4) as usize;
            for (channel, sum) in sums.iter().enumerate() {
                out.bgra[at + channel] = (sum / count) as u8;
            }
        }
    }
    out
}

fn filled(width: u32, height: u32) -> Frame {
    let [r, g, b] = BACKGROUND;
    Frame {
        width,
        height,
        bgra: [b, g, r, 255].repeat((width * height) as usize),
    }
}

/// A grid of labelled cells, [`COLUMNS`] wide.
fn sheet(cells: &[&Thumb]) -> Frame {
    let rows = cells.len().div_ceil(COLUMNS).max(1) as u32;
    let width = COLUMNS as u32 * CELL_WIDTH + (COLUMNS as u32 + 1) * GAP;
    let height = rows * CELL_HEIGHT + (rows + 1) * GAP;
    let mut out = filled(width, height);
    for (index, thumb) in cells.iter().enumerate() {
        let left = GAP + (index % COLUMNS) as u32 * (CELL_WIDTH + GAP);
        let top = GAP + (index / COLUMNS) as u32 * (CELL_HEIGHT + GAP);
        blit(&mut out, &thumb.cell, left, top);
        label(
            &mut out,
            left,
            top,
            &label_text(thumb.number, thumb.offset_ms),
        );
    }
    out
}

/// `#12 01:03.4`: the frame number, then `mm:ss.s` since the start.
pub(crate) fn label_text(number: usize, offset_ms: u64) -> String {
    format!("#{number} {}", super::engine::clock(offset_ms))
}

fn blit(out: &mut Frame, cell: &Frame, left: u32, top: u32) {
    for y in 0..cell.height.min(out.height.saturating_sub(top)) {
        let width = cell.width.min(out.width.saturating_sub(left)) as usize * 4;
        let from = (y * cell.width) as usize * 4;
        let to = (((top + y) * out.width) + left) as usize * 4;
        out.bgra[to..to + width].copy_from_slice(&cell.bgra[from..from + width]);
    }
}

/// White text on a black box at the cell's top-left corner.
fn label(out: &mut Frame, left: u32, top: u32, text: &str) {
    let advance = 6 * LABEL_SCALE;
    let pad = 2 * LABEL_SCALE;
    let box_width = text.chars().count() as u32 * advance + pad;
    let box_height = 7 * LABEL_SCALE + 2 * pad;
    fill_rect(out, left, top, box_width, box_height, [0, 0, 0]);
    for (index, glyph) in text.chars().map(glyph).enumerate() {
        let x0 = left + pad + index as u32 * advance;
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5u32 {
                if bits & (0b10000 >> column) != 0 {
                    fill_rect(
                        out,
                        x0 + column * LABEL_SCALE,
                        top + pad + row as u32 * LABEL_SCALE,
                        LABEL_SCALE,
                        LABEL_SCALE,
                        [255, 255, 255],
                    );
                }
            }
        }
    }
}

fn fill_rect(out: &mut Frame, left: u32, top: u32, width: u32, height: u32, [r, g, b]: [u8; 3]) {
    for y in top..(top + height).min(out.height) {
        for x in left..(left + width).min(out.width) {
            let at = ((y * out.width + x) * 4) as usize;
            out.bgra[at..at + 3].copy_from_slice(&[b, g, r]);
        }
    }
}

/// 5×7 glyphs of the label characters, one row per byte (high bit left).
fn glyph(character: char) -> [u8; 7] {
    match character {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        ':' => [
            0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        '#' => [
            0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
        ],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, value: u8) -> Frame {
        Frame {
            width,
            height,
            bgra: [value, value, value, 255].repeat((width * height) as usize),
        }
    }

    fn collect(count: usize) -> Collector {
        let mut collector = Collector::default();
        for number in 1..=count {
            let source = frame(64, 36, (number % 200) as u8);
            let extras = extras(&source, number <= INLINE_FRAMES_MAX).unwrap();
            collector.add(number, number as u64 * 500, extras);
        }
        collector
    }

    #[test]
    fn few_frames_are_sent_as_they_are() {
        let images = collect(INLINE_FRAMES_MAX).finish().unwrap();
        assert_eq!(images.len(), INLINE_FRAMES_MAX);
        assert!(images.iter().all(|image| image.file.is_none()));
        assert_eq!(images[3].frames, vec![4]);
        assert_eq!(&images[0].jpeg[..2], &[0xFF, 0xD8]);
        assert!(collect(0).finish().unwrap().is_empty());
    }

    #[test]
    fn more_frames_become_labelled_sheets() {
        let images = collect(INLINE_FRAMES_MAX + 1).finish().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].file.as_deref(), Some("overview_1.jpg"));
        assert_eq!(
            images[0].frames,
            (1..=INLINE_FRAMES_MAX + 1).collect::<Vec<_>>()
        );
        // Plenty: at most three full sheets, first and last frame included.
        let images = collect(1000).finish().unwrap();
        assert_eq!(images.len(), SHEETS_MAX);
        let shown: Vec<usize> = images
            .iter()
            .flat_map(|image| image.frames.clone())
            .collect();
        assert_eq!(shown.len(), SHEETS_MAX * COLUMNS * ROWS);
        assert_eq!(shown[0], 1);
        assert_eq!(*shown.last().unwrap(), 1000);
        assert!(shown.windows(2).all(|pair| pair[0] < pair[1]), "{shown:?}");
        // Evenly spread: no gap far beyond the average.
        let widest = shown
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .max()
            .unwrap();
        assert!(widest <= 2 * 1000 / shown.len() + 16, "{shown:?}");
    }

    #[test]
    fn memory_stays_bounded_however_long_it_records() {
        let collector = collect(2000);
        assert!(collector.thumbs.len() <= KEEP_MAX);
        assert!(collector.small.is_empty());
    }

    #[test]
    fn sheets_hold_a_grid_with_labels() {
        let thumbs: Vec<Thumb> = (1..=6)
            .map(|number| Thumb {
                number,
                offset_ms: 1000,
                cell: fit(&frame(320, 180, 128), CELL_WIDTH, CELL_HEIGHT),
            })
            .collect();
        let cells: Vec<&Thumb> = thumbs.iter().collect();
        let grid = sheet(&cells);
        assert_eq!(grid.width, 4 * CELL_WIDTH + 5 * GAP);
        assert_eq!(grid.height, 2 * CELL_HEIGHT + 3 * GAP);
        // The label box is black with white glyphs; the rest of the cell grey.
        let pixel = |x: u32, y: u32| grid.bgra[((y * grid.width + x) * 4) as usize];
        let label_pixels: Vec<u8> = (GAP..GAP + 60)
            .flat_map(|x| (GAP..GAP + 18).map(move |y| (x, y)))
            .map(|(x, y)| pixel(x, y))
            .collect();
        assert!(label_pixels.contains(&255) && label_pixels.contains(&0));
        assert_eq!(pixel(GAP + CELL_WIDTH / 2, GAP + CELL_HEIGHT / 2), 128);
        assert_eq!(label_text(7, 83_250), "#7 01:23.2");
    }

    #[test]
    fn frames_fit_their_cell_keeping_the_aspect() {
        // 4:3 in a 16:9 cell: bars left and right.
        let cell = fit(&frame(400, 300, 200), CELL_WIDTH, CELL_HEIGHT);
        assert_eq!((cell.width, cell.height), (CELL_WIDTH, CELL_HEIGHT));
        assert_eq!(cell.bgra[0], BACKGROUND[2]);
        let middle = ((CELL_HEIGHT / 2 * CELL_WIDTH + CELL_WIDTH / 2) * 4) as usize;
        assert_eq!(cell.bgra[middle], 200);
    }
}
