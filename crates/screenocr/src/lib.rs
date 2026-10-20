//! Read the attached host's screen as TEXT, on the device, with no OCR engine.
//!
//! # Why this is the feature that matters
//!
//! Everything else here lets a machine *drive* the host. This is what lets it *check*. Without
//! it, automating firmware is blind keystroke injection: every key is accepted by something,
//! just not necessarily the menu you meant, and a wrong boot order produces no error at any
//! point. With it, a step can declare the text it expects, verify, and refuse to type when the
//! screen is wrong.
//!
//! PiKVM has this (server-side OCR with a region window) and it is the single capability that
//! separates "a KVM you can script" from "a KVM you can script unattended".
//!
//! # Why not Tesseract
//!
//! RV1106 is one Cortex-A7 with 256MB of DDR3, and the media subsystem already claims most of
//! it. Tesseract plus language data does not fit in that budget, and an ML recogniser fits
//! even less. Both would also be enormous overkill: **firmware text is the easiest recognition
//! problem there is** — one monospaced bitmap font, two colours, glyphs on an exact grid, no
//! rotation, no noise, no antialiasing. Template matching is not a compromise here; it is the
//! correct algorithm, and it runs in microseconds instead of seconds.
//!
//! # Why no image decoding either
//!
//! The capture pipeline hands out **YUYV 4:2:2** on its way to the hardware encoder. In YUYV
//! every even byte is luminance, so a grayscale image is already present in the buffer —
//! `Gray::from_yuyv` is a stride, not a decode. No JPEG, no colour conversion, no allocation
//! beyond the crop.
//!
//! # The honest limitation
//!
//! A template matcher can only read a font it has templates for. The built-in atlas is empty;
//! [`Atlas::learn`] builds one from a screen whose text you already know, which takes one
//! calibration step per machine and then works forever on that machine's firmware. That is a
//! real cost and it is stated here rather than discovered later. The alternative — shipping a
//! guessed font and silently misreading an unfamiliar one — is worse, because a misread screen
//! passes a verification step that should have refused.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An 8-bit grayscale image. The only image type here: colour is irrelevant to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

impl Gray {
    pub fn new(w: usize, h: usize, px: Vec<u8>) -> Option<Self> {
        (px.len() == w * h).then_some(Self { w, h, px })
    }

    /// Take the luminance plane straight out of a YUYV 4:2:2 buffer.
    ///
    /// YUYV packs `Y0 U Y1 V`, so every even byte is a luminance sample and the image is
    /// already there — this is a stride, not a decode.
    pub fn from_yuyv(w: usize, h: usize, buf: &[u8]) -> Option<Self> {
        if buf.len() < w * h * 2 {
            return None;
        }
        Some(Self { w, h, px: buf.iter().step_by(2).take(w * h).copied().collect() })
    }

    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.px[y * self.w + x]
    }

    /// Crop, clamped to the image. ⛔ Clamped rather than refused: a caller asking for a
    /// region slightly off the edge of a screen whose resolution just changed wants the
    /// overlap, not an error — and a resolution change mid-session is normal on a KVM.
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Gray {
        let x0 = x.min(self.w);
        let y0 = y.min(self.h);
        let x1 = (x0 + w).min(self.w);
        let y1 = (y0 + h).min(self.h);
        let (cw, ch) = (x1 - x0, y1 - y0);
        let mut px = Vec::with_capacity(cw * ch);
        for row in y0..y1 {
            px.extend_from_slice(&self.px[row * self.w + x0..row * self.w + x1]);
        }
        Gray { w: cw, h: ch, px }
    }

    /// Otsu's method: pick the threshold that best separates two intensity populations.
    ///
    /// Chosen over a fixed threshold because BIOS screens are not uniformly bright — grey text
    /// on blue, white on black, inverted selection bars — and a constant would read a
    /// highlighted menu row as solid ink.
    pub fn otsu(&self) -> u8 {
        let mut hist = [0usize; 256];
        for &p in &self.px {
            hist[p as usize] += 1;
        }
        let total = self.px.len() as f64;
        if total == 0.0 {
            return 128;
        }
        let sum: f64 = (0..256).map(|i| i as f64 * hist[i] as f64).sum();
        let (mut sum_b, mut w_b, mut best, mut best_t) = (0.0f64, 0.0f64, -1.0f64, 128u8);
        for (t, &count) in hist.iter().enumerate() {
            w_b += count as f64;
            if w_b == 0.0 {
                continue;
            }
            let w_f = total - w_b;
            if w_f == 0.0 {
                break;
            }
            sum_b += t as f64 * count as f64;
            let m_b = sum_b / w_b;
            let m_f = (sum - sum_b) / w_f;
            let between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
            if between > best {
                best = between;
                best_t = t as u8;
            }
        }
        best_t
    }

    /// `true` = ink. Polarity is decided by which population is smaller, because text is
    /// always the minority of pixels on a screen — that holds for light-on-dark and
    /// dark-on-light alike, which is why it is inferred rather than configured.
    pub fn binarize(&self) -> Bitmap {
        let t = self.otsu();
        let dark: usize = self.px.iter().filter(|&&p| p <= t).count();
        let ink_is_dark = dark * 2 < self.px.len();
        Bitmap {
            w: self.w,
            h: self.h,
            bits: self.px.iter().map(|&p| if ink_is_dark { p <= t } else { p > t }).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub w: usize,
    pub h: usize,
    pub bits: Vec<bool>,
}

impl Bitmap {
    pub fn at(&self, x: usize, y: usize) -> bool {
        self.bits[y * self.w + x]
    }

    pub fn ink(&self) -> usize {
        self.bits.iter().filter(|b| **b).count()
    }

    /// Cut into fixed glyph cells. Firmware text is on an exact grid, which is the whole
    /// reason this is cheap — no connected-component analysis, no layout inference.
    pub fn cells(&self, cw: usize, ch: usize) -> Vec<Vec<Bitmap>> {
        if cw == 0 || ch == 0 {
            return Vec::new();
        }
        (0..self.h / ch)
            .map(|r| {
                (0..self.w / cw)
                    .map(|c| {
                        let mut bits = Vec::with_capacity(cw * ch);
                        for y in 0..ch {
                            for x in 0..cw {
                                bits.push(self.at(c * cw + x, r * ch + y));
                            }
                        }
                        Bitmap { w: cw, h: ch, bits }
                    })
                    .collect()
            })
            .collect()
    }
}

/// Glyph templates for one machine's firmware font.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Atlas {
    pub cell_w: usize,
    pub cell_h: usize,
    /// character -> cell bitmap, as a run of bools
    glyphs: BTreeMap<char, Vec<bool>>,
}

impl Atlas {
    pub fn new(cell_w: usize, cell_h: usize) -> Self {
        Self { cell_w, cell_h, glyphs: BTreeMap::new() }
    }

    pub fn len(&self) -> usize {
        self.glyphs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    pub fn chars(&self) -> Vec<char> {
        self.glyphs.keys().copied().collect()
    }

    /// Learn glyphs from a region whose text is already known — the calibration step.
    ///
    /// ⛔ A BLANK CELL IS NOT A GLYPH. A space contributes no ink, so learning it as a template
    /// would make every empty cell match whatever character happened to be blank-ish, and the
    /// reader would hallucinate text out of an empty screen. Spaces are recognised by absence
    /// of ink at read time, never by matching.
    pub fn learn(&mut self, region: &Bitmap, known_text: &str) -> usize {
        let rows: Vec<&str> = known_text.lines().collect();
        let cells = region.cells(self.cell_w, self.cell_h);
        let mut learned = 0;
        for (r, row_cells) in cells.iter().enumerate() {
            let Some(line) = rows.get(r) else { break };
            for (c, cell) in row_cells.iter().enumerate() {
                let Some(ch) = line.chars().nth(c) else { break };
                if ch == ' ' || cell.ink() == 0 {
                    continue;
                }
                self.glyphs.entry(ch).or_insert_with(|| cell.bits.clone());
                learned += 1;
            }
        }
        learned
    }

    /// Best match for one cell, with a confidence in `0.0..=1.0`.
    fn match_cell(&self, cell: &Bitmap) -> Option<(char, f64)> {
        let n = (self.cell_w * self.cell_h) as f64;
        let mut best: Option<(char, f64)> = None;
        for (ch, bits) in &self.glyphs {
            if bits.len() != cell.bits.len() {
                continue;
            }
            let same = bits.iter().zip(&cell.bits).filter(|(a, b)| a == b).count() as f64;
            let score = same / n;
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((*ch, score));
            }
        }
        best
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Reading {
    pub text: String,
    /// Mean confidence over the cells that carried ink. ⛔ Blank cells are excluded: averaging
    /// them in makes a mostly-empty screen look highly confident no matter what it read.
    pub confidence: f64,
    pub rows: usize,
    pub cols: usize,
    pub unreadable_cells: usize,
}

/// Read a binarised region as text.
///
/// `min_confidence` is the floor below which a cell is reported as `?` rather than guessed at.
/// ⛔ Guessing is the dangerous failure: a wrong character that *looks* like a successful read
/// will satisfy a verification step that should have refused.
pub fn read(region: &Bitmap, atlas: &Atlas, min_confidence: f64) -> Reading {
    if atlas.is_empty() || atlas.cell_w == 0 || atlas.cell_h == 0 {
        return Reading {
            text: String::new(),
            confidence: 0.0,
            rows: 0,
            cols: 0,
            unreadable_cells: 0,
        };
    }
    let cells = region.cells(atlas.cell_w, atlas.cell_h);
    let (mut text, mut conf_sum, mut conf_n, mut unreadable) = (String::new(), 0.0, 0usize, 0);
    let rows = cells.len();
    let cols = cells.first().map_or(0, Vec::len);

    for (i, row) in cells.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        for cell in row {
            if cell.ink() == 0 {
                text.push(' ');
                continue;
            }
            match atlas.match_cell(cell) {
                Some((ch, score)) if score >= min_confidence => {
                    text.push(ch);
                    conf_sum += score;
                    conf_n += 1;
                }
                _ => {
                    text.push('?');
                    unreadable += 1;
                }
            }
        }
    }
    Reading {
        text,
        confidence: if conf_n == 0 { 0.0 } else { conf_sum / conf_n as f64 },
        rows,
        cols,
        unreadable_cells: unreadable,
    }
}

/// Does this region contain the expected text? The question a verified BIOS step asks.
///
/// Case-insensitive substring, with whitespace collapsed, because a screen's column padding is
/// not something a caller should have to reproduce exactly to assert on it.
pub fn contains(reading: &Reading, expected: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    norm(&reading.text).contains(&norm(expected))
}
