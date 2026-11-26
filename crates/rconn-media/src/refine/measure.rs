//! Test pictures and picture quality for refinement: a text screen, and how close the
//! viewer's picture is to it.

use crate::video::{DecodedFrame, Frame, PixelLayout};
use crate::yuv::I420;
use openh264::formats::YUVSource;
use std::time::Instant;

/// A text-heavy screen: lines of glyph-like 5x9 bitmaps in 7x14 cells, with an
/// anti-aliasing halo, in several colours on white (left half) and in light colours on a
/// dark editor pane (right half). `typed` glyphs of line 3 are shown (typing); the text
/// is scrolled up by `scroll` pixels.
pub(crate) fn text_screen(w: usize, h: usize, typed: usize, scroll: usize) -> Frame {
    let hash = |a: usize, b: usize| {
        let mut x = (a as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (b as u64);
        x ^= x >> 29;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^ (x >> 32)
    };
    let ink_on_white = [
        [20, 20, 20],
        [160, 40, 30],
        [30, 30, 170],
        [20, 120, 40],
        [120, 30, 140],
    ];
    let ink_on_dark = [
        [220, 220, 220],
        [240, 200, 90],
        [120, 200, 250],
        [200, 130, 220],
    ];
    let mut bgra = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let ty = y + scroll;
            let (line, col) = (ty / 14, x / 7);
            let (gx, gy) = (x % 7, ty % 14);
            let dark = x >= w / 2;
            let bg = if dark { [30, 30, 36] } else { [250, 250, 250] };
            // Words of 2..6 glyphs separated by spaces; lines of varied length.
            let word = hash(line, col / 6);
            let blank = (col % 6) >= 2 + (word % 5) as usize
                || (col * 7 % (w / 2)) > (w / 2) * (3 + (hash(line, 99) % 5) as usize) / 8
                || (line == 3 && col >= typed && !dark);
            let palette: &[[u8; 3]] = if dark { &ink_on_dark } else { &ink_on_white };
            let ink = palette[(word >> 8) as usize % palette.len()];
            let glyph = hash(line * 1000 + col, 7);
            let on = |dx: isize, dy: isize| {
                let (px, py) = (gx as isize - 1 + dx, gy as isize - 3 + dy);
                (0..5).contains(&px)
                    && (0..9).contains(&py)
                    && (glyph >> ((px * 9 + py) % 64)) & 1 == 1
            };
            let c = if blank {
                bg
            } else if on(0, 0) {
                ink
            } else if on(1, 0) || on(-1, 0) || on(0, 1) || on(0, -1) {
                // Anti-aliased edge: a third of the way to the ink.
                std::array::from_fn(|i| ((2 * bg[i] as u16 + ink[i] as u16) / 3) as u8)
            } else {
                bg
            };
            let o = (y * w + x) * 4;
            bgra[o..o + 4].copy_from_slice(&[c[2], c[1], c[0], 255]);
        }
    }
    Frame {
        width: w,
        height: h,
        bgra,
        captured_at: Instant::now(),
    }
}

fn psnr(mse: f64) -> f64 {
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// The viewer's picture against the screen's: (luma PSNR, PSNR over all three planes,
/// largest luma error).
pub(crate) fn quality(src: &I420, out: &DecodedFrame) -> (f64, f64, u8) {
    assert_eq!(out.layout, PixelLayout::I420);
    let se = |a: &[u8], b: &[u8]| -> (f64, u8) {
        a.iter().zip(b).fold((0.0, 0), |(s, m), (&p, &q)| {
            let d = p.abs_diff(q);
            (s + f64::from(d) * f64::from(d), m.max(d))
        })
    };
    let (y, max) = se(src.y(), out.y());
    let (u, _) = se(src.u(), out.chroma1());
    let (v, _) = se(src.v(), out.chroma2());
    let n = src.y().len() as f64;
    (psnr(y / n), psnr((y + u + v) / (n * 1.5)), max)
}
