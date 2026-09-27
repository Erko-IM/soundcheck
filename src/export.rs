//! The spectrogram as a PNG: the part in view at full detail, with the time
//! and frequency axes the screen draws around it.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use eframe::egui::{ColorImage, FontDefinitions};

use crate::audio::{self, Info, Source};
use crate::spectrogram::{self, Spec, View};
use crate::views;

/// The fewest columns an export gets, as a short stretch has fewer windows
/// than make a sharp picture, and the most, with the most pixels in all:
/// past these a long stretch comes out coarser rather than too big to open.
const FEWEST_COLUMNS: usize = 4096;
const MOST_COLUMNS: usize = 16_384;
const MOST_PIXELS: usize = 64_000_000;
/// The fewest rows the part of a lane the file holds gets, however few bins
/// its band has, and the most a lane gets, so a band set far past what the
/// file holds does not make a picture mostly of nothing.
const FEWEST_ROWS: usize = 2048;
const MOST_ROWS: usize = 8192;
/// Room around the lanes for the title and the labels, and between lanes,
/// with lanes 1024 rows tall; taller ones get more, see [`Room`].
const LEFT: f32 = 76.0;
const RIGHT: f32 = 24.0;
const TOP: f32 = 40.0;
const BOTTOM: f32 = 58.0;
const GAP: f32 = 6.0;

type Rgb = [u8; 3];
const BACKGROUND: Rgb = [27, 27, 27];
const AXIS: Rgb = [150, 150, 150];
const ELAPSED: Rgb = [220, 220, 220];
const WALL_CLOCK: Rgb = [115, 115, 115];
const TITLE: Rgb = [235, 235, 235];
/// Past what the file holds, as on screen.
const BEYOND: Rgb = [14, 14, 14];
const BEYOND_HATCH: Rgb = [46, 46, 46];
const LIMIT: Rgb = [130, 130, 130];

/// What to export, copied out of the window for the thread that does it.
pub struct Request {
    pub source: Source,
    pub info: Info,
    pub spec: Spec,
    pub range: Range<usize>,
    pub view: View,
    pub gradient: colorous::Gradient,
    /// Hertz labelled for each hertz in the file.
    pub scale: f32,
    /// A name for each lane, when there is more than one.
    pub lanes: Vec<String>,
    /// Time of day at the first sample, when the recording says.
    pub wall_start: Option<f64>,
    pub title: String,
    /// Said where the band runs past what the file holds.
    pub beyond: String,
    pub to: PathBuf,
}

/// The room around an export's lanes, and the size of its lettering and
/// ticks, in proportion to how tall its lanes are, so a large picture reads
/// as the screen does rather than with lettering lost in it.
#[derive(Clone, Copy)]
struct Room {
    scale: f32,
    left: usize,
    right: usize,
    top: usize,
    bottom: usize,
    gap: usize,
}

impl Room {
    fn new(rows: usize) -> Self {
        let scale = (rows as f32 / 1024.0).clamp(1.0, 3.0);
        let px = |v: f32| (v * scale).round() as usize;
        Self {
            scale,
            left: px(LEFT),
            right: px(RIGHT),
            top: px(TOP),
            bottom: px(BOTTOM),
            gap: px(GAP),
        }
    }

    /// A text size, at this size.
    fn size(self, points: f32) -> f32 {
        points * self.scale
    }

    /// A length, at this size, and never under a pixel.
    fn px(self, points: f32) -> usize {
        ((points * self.scale).round() as usize).max(1)
    }
}

/// Where an export of `range` of the file at `path` goes: beside it, named
/// after it and the stretch, and never over a file already there.
pub fn path_for(path: &Path, range: &Range<usize>, rate: f64) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let (from, to) = (range.start as f64 / rate, range.end as f64 / rate);
    let name = format!("{stem} {from:.2}-{to:.2} s");
    let mut to = path.with_file_name(format!("{name}.png"));
    let mut n = 2;
    while to.exists() {
        to = path.with_file_name(format!("{name} {n}.png"));
        n += 1;
    }
    to
}

/// Columns across and rows up each of `lanes` lanes for `len` frames, the
/// file holding a band `bins` bins tall in `share` of each lane from the
/// bottom: a column a quarter window along, and at least
/// [`FEWEST_COLUMNS`]; a row a bin there, and at least [`FEWEST_ROWS`],
/// with the rest of the lane in proportion up to [`MOST_ROWS`].
fn size(len: usize, fft: usize, bins: usize, share: f32, lanes: usize) -> (usize, usize) {
    let held = bins.max(FEWEST_ROWS);
    let rows = if share > 0.0 {
        ((held as f32 / share).ceil() as usize).clamp(held.min(MOST_ROWS), MOST_ROWS)
    } else {
        FEWEST_ROWS
    };
    let columns = len
        .div_ceil((fft / 4).max(1))
        .max(len.min(FEWEST_COLUMNS))
        .min(MOST_COLUMNS)
        .min(MOST_PIXELS / (rows * lanes.max(1)))
        .max(1);
    (columns, rows)
}

/// Writes the PNG, returning where, or `None` once `cancel` is set.
pub fn export(
    r: &Request,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<Option<PathBuf>, String> {
    let rate = r.info.sample_rate;
    let (lo, hi) = spectrogram::band(&r.view, rate, r.spec.fft);
    let held = spectrogram::held(&r.view, rate, r.spec.fft);
    let bins = held.map_or(0, |(from, top)| {
        ((top - from) / (rate as f32 / r.spec.fft as f32)).ceil() as usize
    });
    let share = held.map_or(0.0, |(_, top)| {
        views::freq_t(top, lo, hi, r.view.log).clamp(0.0, 1.0)
    });
    let lanes = r.lanes.len().max(1);
    let (columns, rows) = size(r.range.len(), r.spec.fft, bins, share, lanes);
    let analysis = audio::analyse(
        &r.source,
        &r.info,
        r.spec,
        r.range.clone(),
        columns,
        cancel,
        progress,
    )?;
    let Some(analysis) = analysis else {
        return Ok(None);
    };
    let definitions = FontDefinitions::default();
    let fonts = Fonts::new(&definitions)?;
    let room = Room::new(rows);
    let mut canvas = Canvas::new(
        room.left + analysis.columns + room.right,
        room.top + lanes * rows + (lanes - 1) * room.gap + room.bottom,
    );
    // The rows the file holds, from the bottom of each lane, and above them
    // the rows past its limit.
    let held_rows = (rows as f32 * share).round() as usize;
    let past = rows - held_rows;
    for plane in 0..analysis.planes.len() {
        let top = room.top + plane * (rows + room.gap);
        if held_rows > 0 {
            let image =
                spectrogram::colorize(&analysis, plane, rate, &r.view, r.gradient, held_rows);
            canvas.image(room.left, top + past, &image);
        }
        if past > 0 {
            let across = room.left..room.left + analysis.columns;
            canvas.hatch(across.clone(), top..top + past);
            for line in 0..room.px(1.0).min(past) {
                canvas.hline(across.clone(), top + past - 1 - line, LIMIT);
            }
            canvas.note(
                &fonts.proportional,
                room.size(13.0),
                (room.left, top),
                (analysis.columns, past),
                &r.beyond,
            );
        }
        freq_axis(&mut canvas, &fonts, room, top, rows, (lo, hi), r);
        if let Some(name) = r.lanes.get(plane).filter(|_| lanes > 1) {
            canvas.text(
                &fonts.proportional,
                room.size(13.0),
                (room.left + room.px(6.0), top + room.px(4.0)),
                Align::TopLeft,
                name,
                TITLE,
            );
        }
    }
    time_axis(
        &mut canvas,
        &fonts,
        room,
        analysis.columns,
        room.top + lanes * rows + (lanes - 1) * room.gap,
        r,
    );
    canvas.text(
        &fonts.proportional,
        room.size(17.0),
        (room.left, room.px(10.0)),
        Align::TopLeft,
        &r.title,
        TITLE,
    );
    canvas.write(&r.to)?;
    Ok(Some(r.to.clone()))
}

/// Frequencies up the lane `rows` tall from `top`, as the screen labels
/// them.
fn freq_axis(
    canvas: &mut Canvas,
    fonts: &Fonts,
    room: Room,
    top: usize,
    rows: usize,
    (lo, hi): (f32, f32),
    r: &Request,
) {
    let (lo, hi) = (lo * r.scale, hi * r.scale);
    let bottom = (top + rows) as f32;
    let ticks = views::freq_ticks(lo, hi, r.view.log, rows as f32 / room.scale, 16.0);
    for f in ticks {
        let y = (bottom - views::freq_t(f, lo, hi, r.view.log) * rows as f32).round() as usize;
        for line in 0..room.px(1.0) {
            let y = (y + line).min(top + rows - 1);
            canvas.hline(room.left - room.px(6.0)..room.left, y, AXIS);
        }
        let label = views::hz(f);
        canvas.text(
            &fonts.mono,
            room.size(13.0),
            (room.left - room.px(9.0), y),
            Align::RightCentre,
            &label,
            AXIS,
        );
    }
    let hz = if r.scale == 1.0 { "Hz" } else { "Hz, shifted" };
    canvas.text(
        &fonts.mono,
        room.size(11.0),
        (room.left - room.px(9.0), top.saturating_sub(room.px(4.0))),
        Align::RightBottom,
        hz,
        WALL_CLOCK,
    );
}

/// Elapsed time under the lanes, ending at `bottom`, and the time of day
/// under that when the recording says when it started.
fn time_axis(
    canvas: &mut Canvas,
    fonts: &Fonts,
    room: Room,
    width: usize,
    bottom: usize,
    r: &Request,
) {
    let rate = f64::from(r.info.sample_rate);
    let (start, len) = (r.range.start as f64, r.range.len().max(1) as f64);
    let step = views::time_step(len / rate, width as f32 / room.scale);
    let decimals = views::decimals(step);
    let first = (start / rate / step).ceil() as i64;
    let last = ((start + len) / rate / step).floor() as i64;
    for i in first..=last {
        let t = i as f64 * step;
        let x = room.left + (((t * rate - start) / len) * width as f64).round() as usize;
        let x = x.min(room.left + width - 1);
        for line in 0..room.px(1.0) {
            canvas.vline(
                (x + line).min(room.left + width - 1),
                bottom..bottom + room.px(6.0),
                AXIS,
            );
        }
        let label = views::clock_with(t, decimals);
        canvas.text(
            &fonts.mono,
            room.size(14.0),
            (x, bottom + room.px(8.0)),
            Align::TopCentre,
            &label,
            ELAPSED,
        );
        if let Some(wall) = r.wall_start {
            let label = views::wall_with(wall + t, decimals);
            canvas.text(
                &fonts.mono,
                room.size(11.0),
                (x, bottom + room.px(28.0)),
                Align::TopCentre,
                &label,
                WALL_CLOCK,
            );
        }
    }
}

/// The fonts the window draws with.
struct Fonts<'a> {
    mono: FontRef<'a>,
    proportional: FontRef<'a>,
}

impl<'a> Fonts<'a> {
    fn new(definitions: &'a FontDefinitions) -> Result<Self, String> {
        let load = |name: &str| -> Result<FontRef<'a>, String> {
            let data = definitions
                .font_data
                .get(name)
                .ok_or_else(|| format!("the {name} font is missing"))?;
            FontRef::try_from_slice_and_index(&data.font, data.index)
                .map_err(|e| format!("the {name} font cannot be read: {e}"))
        };
        Ok(Self {
            mono: load("Hack")?,
            proportional: load("Ubuntu-Light")?,
        })
    }
}

#[derive(Clone, Copy)]
enum Align {
    TopLeft,
    TopCentre,
    RightCentre,
    RightBottom,
}

struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: BACKGROUND.repeat(width * height),
        }
    }

    fn image(&mut self, left: usize, top: usize, image: &ColorImage) {
        let [w, h] = image.size;
        for row in 0..h.min(self.height.saturating_sub(top)) {
            let from = &image.pixels[row * w..][..w.min(self.width.saturating_sub(left))];
            let at = ((top + row) * self.width + left) * 3;
            for (i, c) in from.iter().enumerate() {
                self.pixels[at + i * 3..at + i * 3 + 3].copy_from_slice(&[c.r(), c.g(), c.b()]);
            }
        }
    }

    fn blend(&mut self, x: i64, y: i64, color: Rgb, alpha: f32) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        let at = (y as usize * self.width + x as usize) * 3;
        let a = alpha.clamp(0.0, 1.0);
        for (px, c) in self.pixels[at..at + 3].iter_mut().zip(color) {
            *px = (f32::from(*px) * (1.0 - a) + f32::from(c) * a).round() as u8;
        }
    }

    fn hline(&mut self, x: Range<usize>, y: usize, color: Rgb) {
        for x in x {
            self.blend(x as i64, y as i64, color, 1.0);
        }
    }

    fn vline(&mut self, x: usize, y: Range<usize>, color: Rgb) {
        for y in y {
            self.blend(x as i64, y as i64, color, 1.0);
        }
    }

    /// Past what the file holds, as on screen: dark, with lines rising to
    /// the right across it.
    fn hatch(&mut self, x: Range<usize>, y: Range<usize>) {
        for py in y {
            for px in x.clone() {
                let color = if (px + py) % 9 == 0 {
                    BEYOND_HATCH
                } else {
                    BEYOND
                };
                self.blend(px as i64, py as i64, color, 1.0);
            }
        }
    }

    /// `text`, `size` pixels high, on a dark ground in the middle of the
    /// stretch `width` by `height` from `(x, y)`, where it fits.
    fn note(
        &mut self,
        font: &FontRef<'_>,
        size: f32,
        (x, y): (usize, usize),
        (width, height): (usize, usize),
        text: &str,
    ) {
        let scaled = font.as_scaled(PxScale::from(size));
        let wide: f32 = text
            .chars()
            .map(|c| scaled.h_advance(scaled.glyph_id(c)))
            .sum();
        let tall = scaled.ascent() - scaled.descent();
        let (pad_x, pad_y) = (
            (size * 0.45).round() as usize,
            (size * 0.23).round() as usize,
        );
        let (w, h) = (
            wide.ceil() as usize + 2 * pad_x,
            tall.ceil() as usize + 2 * pad_y,
        );
        if w > width || h > height {
            return;
        }
        let (left, top) = (x + (width - w) / 2, y + (height - h) / 2);
        for py in top..top + h {
            for px in left..left + w {
                self.blend(px as i64, py as i64, [0, 0, 0], 0.82);
            }
        }
        self.text(
            font,
            size,
            (left + pad_x, top + pad_y),
            Align::TopLeft,
            text,
            AXIS,
        );
    }

    fn text(
        &mut self,
        font: &FontRef<'_>,
        size: f32,
        (x, y): (usize, usize),
        align: Align,
        text: &str,
        color: Rgb,
    ) {
        let scaled = font.as_scaled(PxScale::from(size));
        let width: f32 = text
            .chars()
            .map(|c| scaled.h_advance(scaled.glyph_id(c)))
            .sum();
        let (x, y) = (x as f32, y as f32);
        let left = match align {
            Align::TopLeft => x,
            Align::TopCentre => x - width / 2.0,
            Align::RightCentre | Align::RightBottom => x - width,
        };
        // Moved in from an edge it would run past, as on screen.
        let left = left.min(self.width as f32 - width).max(0.0);
        let baseline = match align {
            Align::TopLeft | Align::TopCentre => y + scaled.ascent(),
            Align::RightCentre => y + (scaled.ascent() + scaled.descent()) / 2.0,
            Align::RightBottom => y + scaled.descent(),
        };
        let mut pen = left;
        for c in text.chars() {
            let id = scaled.glyph_id(c);
            let glyph = id.with_scale_and_position(size, point(pen, baseline));
            pen += scaled.h_advance(id);
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i64 + i64::from(gx);
                    let py = bounds.min.y as i64 + i64::from(gy);
                    self.blend(px, py, color, coverage);
                });
            }
        }
    }

    /// Writes the image to `to` in one step: through a file beside it, so a
    /// failed export leaves nothing half written.
    fn write(&self, to: &Path) -> Result<(), String> {
        let name = to.file_name().unwrap_or_default().to_string_lossy();
        let temp = to.with_file_name(format!(".{name}.soundcheck-export"));
        let written = (|| -> Result<(), String> {
            let file = File::create(&temp).map_err(|e| format!("cannot write the PNG: {e}"))?;
            let mut out = BufWriter::new(file);
            let mut encoder = png::Encoder::new(&mut out, self.width as u32, self.height as u32);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
            writer
                .write_image_data(&self.pixels)
                .map_err(|e| e.to_string())?;
            writer.finish().map_err(|e| e.to_string())?;
            out.flush()
                .map_err(|e| format!("cannot write the PNG: {e}"))
        })();
        let placed = written.and_then(|()| {
            fs::rename(&temp, to).map_err(|e| format!("cannot put the PNG in place: {e}"))
        });
        if placed.is_err() {
            let _ = fs::remove_file(&temp);
        }
        placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{idle, temp_file};
    use crate::spectrogram::Channels;
    use crate::wav;

    #[test]
    fn a_short_view_gets_the_fewest_columns_and_a_long_one_a_column_a_step() {
        assert_eq!(size(4_000, 2048, 1025, 1.0, 1), (4000, FEWEST_ROWS));
        assert_eq!(
            size(10 * 48_000, 2048, 1025, 1.0, 1),
            (FEWEST_COLUMNS, FEWEST_ROWS)
        );
        assert_eq!(size(60 * 48_000, 2048, 1025, 1.0, 1), (5625, FEWEST_ROWS));
        assert_eq!(size(3600 * 48_000, 2048, 1025, 1.0, 1).0, MOST_COLUMNS);
        // More bins than the fewest rows: a row each.
        assert_eq!(size(60 * 48_000, 8192, 4097, 1.0, 1).1, 4097);
    }

    #[test]
    fn a_narrow_band_still_gets_rows_and_many_lanes_stay_within_the_pixels() {
        assert_eq!(size(60 * 48_000, 2048, 90, 1.0, 1).1, FEWEST_ROWS);
        let (columns, rows) = size(3600 * 48_000, 8192, 4097, 1.0, 8);
        assert!(columns * rows * 8 <= MOST_PIXELS && columns > 1000);
    }

    #[test]
    fn a_band_past_what_the_file_holds_gets_rows_in_proportion_up_to_a_limit() {
        // Up to 48 kHz on a 48 kHz file: the file fills half the lane.
        assert_eq!(size(48_000, 2048, 1025, 0.5, 1).1, 2 * FEWEST_ROWS);
        assert_eq!(size(48_000, 2048, 1025, 0.001, 1).1, MOST_ROWS);
        assert_eq!(size(48_000, 2048, 0, 0.0, 1).1, FEWEST_ROWS);
    }

    #[test]
    fn exports_go_beside_the_recording_and_never_over_another_file() {
        let dir = std::env::temp_dir().join("soundcheck-export-names");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("bats.wav");
        let first = path_for(&wav, &(4_800..163_200), 48_000.0);
        assert_eq!(first, dir.join("bats 0.10-3.40 s.png"));
        fs::write(&first, b"taken").unwrap();
        assert_eq!(
            path_for(&wav, &(4_800..163_200), 48_000.0),
            dir.join("bats 0.10-3.40 s 2.png")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_export_is_a_png_of_the_part_in_view_with_room_for_its_axes() {
        let samples: Vec<i16> = (0..48_000)
            .map(|i| ((i as f32 * 0.2).sin() * 12_000.0) as i16)
            .collect();
        let path = temp_file("export.wav", &wav::tests::build(false, &[], &samples));
        let opened = audio::open(&path).unwrap();
        let to = path.with_extension("png");
        let request = Request {
            source: opened.source,
            info: opened.info,
            spec: Spec {
                fft: 1024,
                channels: Channels::Mix,
            },
            range: 12_000..36_000,
            view: View {
                brightness: 0.0,
                contrast: 90.0,
                f_min: 0.0,
                f_max: 24_000.0,
                log: false,
            },
            gradient: colorous::VIRIDIS,
            scale: 1.0,
            lanes: vec!["Mix".into()],
            wall_start: Some(3600.0),
            title: "export.wav  0:00.25 to 0:00.75".into(),
            beyond: "Nothing past 24.0 kHz".into(),
            to: to.clone(),
        };
        let (cancel, progress) = idle();
        assert_eq!(
            export(&request, &cancel, &progress).unwrap(),
            Some(to.clone())
        );
        let read = |to: &Path| {
            let decoder = png::Decoder::new(std::io::BufReader::new(File::open(to).unwrap()));
            let mut reader = decoder.read_info().unwrap();
            let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut pixels).unwrap();
            (info.width as usize, info.height as usize, pixels)
        };
        let (width, height, _) = read(&to);
        // 24 000 frames at a column every 256, which is fewer than the
        // fewest, and a band of 512 bins, fewer than the fewest rows: twice
        // the lanes a 1024-row one has, and twice its room.
        let room = Room::new(FEWEST_ROWS);
        assert_eq!(room.left, 2 * LEFT as usize);
        assert_eq!(
            (width, height),
            (
                room.left + FEWEST_COLUMNS + room.right,
                room.top + FEWEST_ROWS + room.bottom
            )
        );
        fs::remove_file(&to).unwrap();

        // Up to 96 kHz: the file's 24 kHz fills the bottom quarter, as tall
        // as before, and the rest is hatched.
        let past = Request {
            view: View {
                f_max: 96_000.0,
                ..request.view
            },
            ..request
        };
        export(&past, &cancel, &progress).unwrap();
        let (width, height, pixels) = read(&to);
        let room = Room::new(4 * FEWEST_ROWS);
        assert_eq!(height, room.top + 4 * FEWEST_ROWS + room.bottom);
        let at = |x: usize, y: usize| &pixels[(y * width + x) * 3..][..3];
        let (x, y) = (room.left + 100, room.top + 10);
        let hatched = (0..9).any(|dx| at(x + dx, y) == BEYOND_HATCH)
            && (0..9).any(|dx| at(x + dx, y) == BEYOND);
        assert!(hatched, "{:?}", at(x, y));
        assert_eq!(at(x, room.top + 3 * FEWEST_ROWS - 1), LIMIT);
        fs::remove_file(&to).unwrap();
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn labels_come_out_as_ink() {
        let definitions = FontDefinitions::default();
        let fonts = Fonts::new(&definitions).unwrap();
        let mut canvas = Canvas::new(120, 40);
        canvas.text(
            &fonts.mono,
            14.0,
            (10, 10),
            Align::TopLeft,
            "12.5k",
            ELAPSED,
        );
        let lit = canvas.pixels.chunks(3).filter(|p| p[0] > 120).count();
        assert!(lit > 40, "{lit} pixels lit");
    }
}
