//! The read path, end to end, against a synthetic firmware font.
//!
//! The font is generated rather than shipped: each character gets a deterministic, distinct
//! bit pattern, so a learn-then-read round trip exercises exactly the real pipeline —
//! YUYV → grayscale → Otsu → binarise → segment → match — without needing a real BIOS screen.
//!
//! ⛔ The tests that matter are the ones about what it REFUSES to read. A matcher that guesses
//! produces a wrong character that looks like a successful read, and a wrong successful read
//! satisfies a verification step that should have refused — which is worse than no OCR at all.

use screenocr::{contains, read, Atlas, Bitmap, Gray};

const CW: usize = 6;
const CH: usize = 8;

/// A deterministic glyph for `ch`, guaranteed distinct for distinct characters.
///
/// ⛔ THE FIRST VERSION OF THIS HELPER WAS A HASH, AND 'B' AND 'S' CAME OUT BYTE-IDENTICAL.
/// Six tests failed and the matcher looked broken; it was reading an ambiguous font exactly
/// as it should. The code-point bits are now written literally into the first two rows, which
/// makes injectivity a property of the construction rather than a hope about a hash — the
/// rest is filler so the cells still look like glyphs.
fn glyph(ch: char) -> Vec<bool> {
    let code = ch as u32;
    let mut bits = vec![false; CW * CH];
    for (i, b) in bits.iter_mut().enumerate().take(CW * 2) {
        *b = (code >> i) & 1 == 1;               // rows 0-1: the code point, verbatim
    }
    for (i, b) in bits.iter_mut().enumerate().skip(CW * 2) {
        let v = code                             // the rest: deterministic filler
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add((i as u32).wrapping_mul(0x85EB_CA6B));
        *b = (v ^ (v >> 15)).is_multiple_of(3);
    }
    bits
}

#[test]
fn the_test_font_itself_has_no_colliding_glyphs() {
    // ⛔ Guards the harness, not the library. Without this the suite can fail for a reason
    // that has nothing to do with the code under test — which is exactly what happened.
    let chars: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ".chars().collect();
    for &a in &chars {
        for &b in &chars {
            if a != b {
                assert_ne!(glyph(a), glyph(b), "test font collides: {a:?} vs {b:?}");
            }
        }
    }
}

/// Render lines of text into a bitmap using that font.
fn render(lines: &[&str]) -> Bitmap {
    let cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let (w, h) = (cols * CW, lines.len() * CH);
    let mut bits = vec![false; w * h];
    for (r, line) in lines.iter().enumerate() {
        for (c, ch) in line.chars().enumerate() {
            if ch == ' ' {
                continue;
            }
            let g = glyph(ch);
            for y in 0..CH {
                for x in 0..CW {
                    bits[(r * CH + y) * w + c * CW + x] = g[y * CW + x];
                }
            }
        }
    }
    Bitmap { w, h, bits }
}

fn trained(lines: &[&str]) -> Atlas {
    let mut a = Atlas::new(CW, CH);
    a.learn(&render(lines), &lines.join("\n"));
    a
}

// --- the frame path: no decode, just a stride ------------------------------------------

#[test]
fn yuyv_luminance_is_taken_without_decoding_anything() {
    // Y0 U Y1 V ... — every even byte is luminance.
    let buf: Vec<u8> = vec![10, 200, 20, 201, 30, 202, 40, 203];
    let g = Gray::from_yuyv(4, 1, &buf).unwrap();
    assert_eq!(g.px, vec![10, 20, 30, 40], "chroma leaked into the grayscale plane");
}

#[test]
fn a_short_yuyv_buffer_is_refused_rather_than_read_past() {
    assert!(Gray::from_yuyv(100, 100, &[0u8; 16]).is_none());
}

#[test]
fn a_crop_is_clamped_to_the_screen_not_refused() {
    // ⛔ A resolution change mid-session is normal on a KVM; a caller asking for a region
    // slightly off the new edge wants the overlap, not an error.
    let g = Gray::new(4, 4, (0..16).collect()).unwrap();
    let c = g.crop(2, 2, 99, 99);
    assert_eq!((c.w, c.h), (2, 2));
    assert_eq!(c.px, vec![10, 11, 14, 15]);
}

// --- binarisation: polarity is inferred, not configured ----------------------------------

#[test]
fn light_text_on_a_dark_screen_binarises_text_as_ink() {
    let mut px = vec![16u8; 100]; // dark background
    px[42] = 240;                 // one bright pixel of text
    let b = Gray::new(10, 10, px).unwrap().binarize();
    assert_eq!(b.ink(), 1, "bright-on-dark text was not read as ink");
    assert!(b.at(2, 4));
}

#[test]
fn dark_text_on_a_light_screen_also_binarises_text_as_ink() {
    // ⛔ Both polarities occur on real firmware — white-on-black POST, dark-on-grey setup
    // UIs, inverted selection bars. Text is always the MINORITY of pixels, which is how
    // polarity is inferred rather than asked for.
    let mut px = vec![240u8; 100];
    px[42] = 16;
    let b = Gray::new(10, 10, px).unwrap().binarize();
    assert_eq!(b.ink(), 1, "dark-on-light text was not read as ink");
    assert!(b.at(2, 4));
}

// --- the read path ------------------------------------------------------------------------

#[test]
fn text_learned_once_is_read_back_exactly() {
    let lines = ["BOOT OPTIONS", "SAVE AND EXIT"];
    let atlas = trained(&lines);
    let r = read(&render(&lines), &atlas, 0.95);
    assert_eq!(r.text, "BOOT OPTIONS \nSAVE AND EXIT", "got {:?}", r.text);
    assert!(r.confidence > 0.99, "confidence {}", r.confidence);
    assert_eq!(r.unreadable_cells, 0);
}

#[test]
fn a_blank_cell_reads_as_a_space_and_is_never_matched() {
    // ⛔ A space has no ink, so learning it as a template would make every empty cell match
    // it — and the reader would hallucinate text out of an empty screen.
    let atlas = trained(&["AB CD"]);
    assert!(!atlas.chars().contains(&' '), "the space was learned as a glyph");
    let r = read(&render(&["AB CD"]), &atlas, 0.95);
    assert_eq!(r.text, "AB CD");
}

#[test]
fn an_empty_screen_reads_as_blank_not_as_invented_text() {
    let atlas = trained(&["HELLO"]);
    let blank = Bitmap { w: 5 * CW, h: CH, bits: vec![false; 5 * CW * CH] };
    let r = read(&blank, &atlas, 0.95);
    assert_eq!(r.text.trim(), "", "invented text from an empty screen: {:?}", r.text);
    assert_eq!(r.confidence, 0.0);
}

#[test]
fn an_unknown_glyph_is_reported_as_unreadable_rather_than_guessed() {
    // ⛔ The most important refusal here. A guessed character looks like a successful read,
    // and a wrong successful read satisfies a verification step that should have refused.
    let atlas = trained(&["ABC"]);              // never saw 'Z'
    let r = read(&render(&["ABZ"]), &atlas, 0.95);
    assert!(r.text.contains('?'), "an unseen glyph was guessed at: {:?}", r.text);
    assert_eq!(r.unreadable_cells, 1);
}

#[test]
fn an_empty_atlas_reads_nothing_and_says_so() {
    let r = read(&render(&["ANYTHING"]), &Atlas::new(CW, CH), 0.9);
    assert_eq!(r.text, "");
    assert_eq!(r.confidence, 0.0);
}

#[test]
fn confidence_excludes_blank_cells() {
    // Averaging blanks in would make a mostly-empty screen look highly confident whatever
    // it read.
    let atlas = trained(&["AB"]);
    let r = read(&render(&["A        B"]), &atlas, 0.95);
    assert!(r.confidence > 0.99, "blank cells dragged confidence to {}", r.confidence);
}

#[test]
fn a_lower_confidence_floor_accepts_more_and_the_caller_chooses() {
    let atlas = trained(&["ABC"]);
    let strict = read(&render(&["ABZ"]), &atlas, 0.99);
    let loose = read(&render(&["ABZ"]), &atlas, 0.10);
    assert_eq!(strict.unreadable_cells, 1);
    assert_eq!(loose.unreadable_cells, 0, "a floor of 0.1 should accept the nearest match");
}

// --- the question a verified BIOS step actually asks ---------------------------------------

#[test]
fn contains_ignores_column_padding_and_case() {
    let atlas = trained(&["BOOT   OPTIONS"]);
    let r = read(&render(&["BOOT   OPTIONS"]), &atlas, 0.95);
    assert!(contains(&r, "boot options"), "read {:?}", r.text);
    assert!(!contains(&r, "save and exit"));
}

// --- the whole pipeline, from a camera buffer ------------------------------------------------

#[test]
fn a_yuyv_frame_is_cropped_and_read_end_to_end() {
    let lines = ["POST OK"];
    let atlas = trained(&lines);
    let rendered = render(&lines);

    // paint the rendered text into a larger dark screen at (16, 8), as YUYV
    let (sw, sh) = (256usize, 64usize);
    let mut yuyv = vec![0u8; sw * sh * 2];
    for i in 0..sw * sh {
        yuyv[i * 2 + 1] = 128; // neutral chroma
    }
    for y in 0..rendered.h {
        for x in 0..rendered.w {
            if rendered.at(x, y) {
                let idx = ((y + 8) * sw + (x + 16)) * 2;
                yuyv[idx] = 255;
            }
        }
    }

    let gray = Gray::from_yuyv(sw, sh, &yuyv).unwrap();
    let region = gray.crop(16, 8, rendered.w, rendered.h).binarize();
    let r = read(&region, &atlas, 0.95);
    assert!(contains(&r, "POST OK"), "end-to-end read gave {:?}", r.text);
}
