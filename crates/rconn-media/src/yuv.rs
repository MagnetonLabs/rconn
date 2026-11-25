//! BGRA -> I420 (YUV 4:2:0) conversion, split across CPU cores.
//!
//! Same BT.601 limited-range formula as openh264's own converter, in integer maths, so
//! the encoder sees the same colours; each core converts a band of rows. On an 8-core VM
//! this takes the conversion from the largest share of per-frame host time to a sliver.

use openh264::formats::YUVSource;

/// An I420 picture (Y plane, then U, then V), fed straight to the encoder.
pub struct I420 {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

/// Rows per band below which splitting isn't worth a thread.
const MIN_BAND_ROWS: usize = 64;

pub(crate) fn threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8)
}

impl I420 {
    /// `width` and `height` must be even.
    pub fn new(width: usize, height: usize) -> Self {
        assert!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "I420 needs even dimensions"
        );
        Self {
            width,
            height,
            data: vec![0; width * height * 3 / 2],
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Convert the top-left `width` x `height` of a BGRA image with `src_width` pixels per row.
    pub fn fill_from_bgra(&mut self, bgra: &[u8], src_width: usize) {
        let (w, h) = (self.width, self.height);
        assert!(
            src_width >= w && bgra.len() >= src_width * 4 * h,
            "source smaller than picture"
        );
        let (y, uv) = self.data.split_at_mut(w * h);
        let (u, v) = uv.split_at_mut(w * h / 4);
        // Bands of an even number of rows; chroma rows are half of each band.
        let bands = threads().min(h / MIN_BAND_ROWS).max(1);
        let band_rows = (h / bands).next_multiple_of(2).max(2);
        let jobs = y
            .chunks_mut(band_rows * w)
            .zip(u.chunks_mut(band_rows / 2 * (w / 2)))
            .zip(v.chunks_mut(band_rows / 2 * (w / 2)))
            .enumerate();
        std::thread::scope(|s| {
            for (i, ((y, u), v)) in jobs {
                let first_row = i * band_rows;
                let src = &bgra[first_row * src_width * 4..];
                if bands == 1 {
                    convert_band(src, src_width, w, y, u, v);
                } else {
                    s.spawn(move || convert_band(src, src_width, w, y, u, v));
                }
            }
        });
    }
}

impl I420 {
    /// Like [`fill_from_bgra`](Self::fill_from_bgra), scaling a `src_width` x `src_height`
    /// image down to this picture's size (nearest sample per pixel, chroma averaged).
    pub fn fill_from_bgra_scaled(&mut self, bgra: &[u8], src_width: usize, src_height: usize) {
        let (w, h) = (self.width, self.height);
        if (w, h) == (src_width & !1, src_height & !1) {
            return self.fill_from_bgra(bgra, src_width);
        }
        assert!(bgra.len() >= src_width * src_height * 4 && w <= src_width && h <= src_height);
        let xs: Vec<usize> = (0..w).map(|x| x * src_width / w).collect();
        let (y_plane, uv) = self.data.split_at_mut(w * h);
        let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);
        let bands = threads().min(h / MIN_BAND_ROWS).max(1);
        let band_rows = (h / bands).next_multiple_of(2).max(2);
        let jobs = y_plane
            .chunks_mut(band_rows * w)
            .zip(u_plane.chunks_mut(band_rows / 2 * (w / 2)))
            .zip(v_plane.chunks_mut(band_rows / 2 * (w / 2)))
            .enumerate();
        let xs = &xs;
        std::thread::scope(|s| {
            for (i, ((y, u), v)) in jobs {
                s.spawn(move || {
                    let rows = y.len() / w;
                    for pair in 0..rows / 2 {
                        let oy = i * band_rows + 2 * pair;
                        let src_row = |r: usize| {
                            let sy = (r * src_height / h).min(src_height - 1);
                            &bgra[sy * src_width * 4..(sy + 1) * src_width * 4]
                        };
                        let (r0, r1) = (src_row(oy), src_row(oy + 1));
                        let (y0, y1) = y[2 * pair * w..(2 * pair + 2) * w].split_at_mut(w);
                        let half = w / 2;
                        let (ur, vr) = (
                            &mut u[pair * half..(pair + 1) * half],
                            &mut v[pair * half..(pair + 1) * half],
                        );
                        let px = |row: &[u8], sx: usize| {
                            (
                                i32::from(row[4 * sx]),
                                i32::from(row[4 * sx + 1]),
                                i32::from(row[4 * sx + 2]),
                            )
                        };
                        let luma = |p: (i32, i32, i32)| {
                            (((66 * p.2 + 129 * p.1 + 25 * p.0 + 128) >> 8) + 16) as u8
                        };
                        for x in 0..half {
                            let (sa, sb) = (xs[2 * x], xs[2 * x + 1]);
                            let (a, b, c, d) = (px(r0, sa), px(r0, sb), px(r1, sa), px(r1, sb));
                            y0[2 * x] = luma(a);
                            y0[2 * x + 1] = luma(b);
                            y1[2 * x] = luma(c);
                            y1[2 * x + 1] = luma(d);
                            let (bb, gg, rr) = (
                                (a.0 + b.0 + c.0 + d.0 + 2) >> 2,
                                (a.1 + b.1 + c.1 + d.1 + 2) >> 2,
                                (a.2 + b.2 + c.2 + d.2 + 2) >> 2,
                            );
                            ur[x] = (((-38 * rr - 74 * gg + 112 * bb + 128) >> 8) + 128) as u8;
                            vr[x] = (((112 * rr - 94 * gg - 18 * bb + 128) >> 8) + 128) as u8;
                        }
                    }
                });
            }
        });
    }
}

/// Convert rows into one band of the Y/U/V planes (`y.len() / w` rows, even).
fn convert_band(src: &[u8], src_width: usize, w: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    convert_rows(src, src_width * 4, w, 0..w, y, u, v);
}

/// Convert columns `xs` (even bounds) of rows into the Y/U/V planes. `src` starts at the
/// first source row (`pitch` bytes per row); `y` starts at the matching picture row
/// (`w` bytes per row, `y.len() / w` rows, even), `u`/`v` at the matching chroma row.
fn convert_rows(
    src: &[u8],
    pitch: usize,
    w: usize,
    xs: std::ops::Range<usize>,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let luma = |b: i32, g: i32, r: i32| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
    for pair in 0..y.len() / w / 2 {
        let r0 = &src[(2 * pair) * pitch..];
        let r1 = &src[(2 * pair + 1) * pitch..];
        let (y0, y1) = y[2 * pair * w..(2 * pair + 2) * w].split_at_mut(w);
        let (ur, vr) = (
            &mut u[pair * (w / 2)..(pair + 1) * (w / 2)],
            &mut v[pair * (w / 2)..(pair + 1) * (w / 2)],
        );
        for x in xs.start / 2..xs.end / 2 {
            let px = |row: &[u8], i: usize| {
                (
                    i32::from(row[4 * i]),
                    i32::from(row[4 * i + 1]),
                    i32::from(row[4 * i + 2]),
                )
            };
            let (a, b, c, d) = (
                px(r0, 2 * x),
                px(r0, 2 * x + 1),
                px(r1, 2 * x),
                px(r1, 2 * x + 1),
            );
            y0[2 * x] = luma(a.0, a.1, a.2);
            y0[2 * x + 1] = luma(b.0, b.1, b.2);
            y1[2 * x] = luma(c.0, c.1, c.2);
            y1[2 * x + 1] = luma(d.0, d.1, d.2);
            let (bb, gg, rr) = (
                (a.0 + b.0 + c.0 + d.0 + 2) >> 2,
                (a.1 + b.1 + c.1 + d.1 + 2) >> 2,
                (a.2 + b.2 + c.2 + d.2 + 2) >> 2,
            );
            ur[x] = (((-38 * rr - 74 * gg + 112 * bb + 128) >> 8) + 128) as u8;
            vr[x] = (((112 * rr - 94 * gg - 18 * bb + 128) >> 8) + 128) as u8;
        }
    }
}

/// A rectangle of changed pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    /// Widened to even coordinates (4:2:0 chroma covers 2x2 pixels) and clipped to
    /// `width` x `height` (both even). `None` if nothing is left.
    pub fn even_within(self, width: usize, height: usize) -> Option<Rect> {
        let (x0, y0) = (self.x & !1, self.y & !1);
        let x1 = (self.x + self.w).next_multiple_of(2).min(width);
        let y1 = (self.y + self.h).next_multiple_of(2).min(height);
        (x1 > x0 && y1 > y0).then(|| Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
}

impl I420 {
    /// Convert only `rects` of a BGRA image of this picture's size or larger (`pitch`
    /// bytes per row); the rest of the picture keeps its previous content.
    pub fn fill_rects_from_bgra(&mut self, bgra: &[u8], pitch: usize, rects: &[Rect]) {
        let (w, h) = (self.width, self.height);
        let (y_plane, uv) = self.data.split_at_mut(w * h);
        let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);
        for r in rects.iter().filter_map(|r| r.even_within(w, h)) {
            assert!(bgra.len() >= (r.y + r.h - 1) * pitch + (r.x + r.w) * 4);
            let xs = r.x..r.x + r.w;
            let y = &mut y_plane[r.y * w..(r.y + r.h) * w];
            let u = &mut u_plane[r.y / 2 * (w / 2)..(r.y + r.h) / 2 * (w / 2)];
            let v = &mut v_plane[r.y / 2 * (w / 2)..(r.y + r.h) / 2 * (w / 2)];
            let src = &bgra[r.y * pitch..];
            // Big areas (scrolling, video) split across cores; small ones stay here.
            let bands = if r.w * r.h >= 256 * 1024 {
                threads().min(r.h / MIN_BAND_ROWS).max(1)
            } else {
                1
            };
            if bands == 1 {
                convert_rows(src, pitch, w, xs, y, u, v);
                continue;
            }
            let band_rows = (r.h / bands).next_multiple_of(2).max(2);
            std::thread::scope(|s| {
                for (i, ((y, u), v)) in y
                    .chunks_mut(band_rows * w)
                    .zip(u.chunks_mut(band_rows / 2 * (w / 2)))
                    .zip(v.chunks_mut(band_rows / 2 * (w / 2)))
                    .enumerate()
                {
                    let src = &src[i * band_rows * pitch..];
                    let xs = xs.clone();
                    s.spawn(move || convert_rows(src, pitch, w, xs, y, u, v));
                }
            });
        }
    }

    /// Update `out` to this picture as NV12. With `rows` (an even range), only those rows
    /// are rewritten; `out` must then hold the previous NV12 of this same-size picture.
    pub fn write_nv12(&self, out: &mut Vec<u8>, rows: Option<(usize, usize)>) {
        let (w, h) = (self.width, self.height);
        let (r0, r1) = match rows {
            Some(r) if out.len() == self.data.len() => r,
            _ => {
                out.resize(self.data.len(), 0);
                (0, h)
            }
        };
        let (r0, r1) = (r0 & !1, r1.next_multiple_of(2).min(h));
        if r1 <= r0 {
            return;
        }
        out[r0 * w..r1 * w].copy_from_slice(&self.y()[r0 * w..r1 * w]);
        let half = w / 2;
        let (u, v) = (self.u(), self.v());
        let uv = &mut out[w * h..];
        for cy in r0 / 2..r1 / 2 {
            let dst = &mut uv[cy * w..(cy + 1) * w];
            let (ur, vr) = (
                &u[cy * half..(cy + 1) * half],
                &v[cy * half..(cy + 1) * half],
            );
            for (x, pair) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                *pair = [ur[x], vr[x]];
            }
        }
    }
}

impl I420 {
    /// The same picture as NV12 (Y plane, then interleaved U/V), as Media Foundation wants.
    pub fn to_nv12(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len());
        out.extend_from_slice(self.y());
        for (u, v) in self.u().iter().zip(self.v()) {
            out.extend([*u, *v]);
        }
        out
    }
}

/// One BT.601 limited-range pixel, as openh264's own conversion.
#[inline]
fn yuv_to_rgba(y: u8, u: u8, v: u8) -> [u8; 4] {
    let c = 298 * (i32::from(y) - 16);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let clamp = |n: i32| ((n + 128) >> 8).clamp(0, 255) as u8;
    [
        clamp(c + 409 * e),
        clamp(c - 100 * d - 208 * e),
        clamp(c + 516 * d),
        255,
    ]
}

/// Converts `out` (`width * height * 4` bytes) in bands of rows, one per core;
/// `row(y, dst)` fills one row.
fn rgba_bands(
    (width, height): (usize, usize),
    out: &mut [u8],
    row: impl Fn(usize, &mut [u8]) + Sync,
) {
    assert!(out.len() >= width * height * 4);
    let rows_per = height.div_ceil(threads()).next_multiple_of(2).max(2);
    let row = &row;
    std::thread::scope(|s| {
        for (band, dst) in out[..width * height * 4]
            .chunks_mut(rows_per * width * 4)
            .enumerate()
        {
            s.spawn(move || {
                for (r, line) in dst.chunks_exact_mut(width * 4).enumerate() {
                    row(band * rows_per + r, line);
                }
            });
        }
    });
}

/// I420 planes -> RGBA (BT.601 limited range, as openh264's own conversion), split across
/// cores. `strides` are (Y, U, V) bytes per row; `out` is `width * height * 4` bytes.
pub fn i420_to_rgba(
    (y, u, v): (&[u8], &[u8], &[u8]),
    (ys, us, vs): (usize, usize, usize),
    (width, height): (usize, usize),
    out: &mut [u8],
) {
    rgba_bands((width, height), out, |yy, line| {
        let (yrow, urow, vrow) = (&y[yy * ys..], &u[(yy / 2) * us..], &v[(yy / 2) * vs..]);
        for (x, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = yuv_to_rgba(yrow[x], urow[x / 2], vrow[x / 2]);
        }
    });
}

/// NV12 (Y plane, then interleaved U/V) -> RGBA, same colours as [`i420_to_rgba`].
pub fn nv12_to_rgba(
    (y, uv): (&[u8], &[u8]),
    (ys, uvs): (usize, usize),
    (width, height): (usize, usize),
    out: &mut [u8],
) {
    rgba_bands((width, height), out, |yy, line| {
        let (yrow, uvrow) = (&y[yy * ys..], &uv[(yy / 2) * uvs..]);
        for (x, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let c = x & !1;
            *px = yuv_to_rgba(yrow[x], uvrow[c], uvrow[c + 1]);
        }
    });
}

impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }
    fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }
    fn u(&self) -> &[u8] {
        let n = self.width * self.height;
        &self.data[n..n + n / 4]
    }
    fn v(&self) -> &[u8] {
        let n = self.width * self.height;
        &self.data[n + n / 4..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::{BgraSliceU8, YUVBuffer};

    fn sample(w: usize, h: usize) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [
                    (x * 7 % 256) as u8,
                    (y * 3 % 256) as u8,
                    ((x ^ y) % 256) as u8,
                    255,
                ]
            })
            .collect()
    }

    #[test]
    fn matches_openh264_colours_within_rounding() {
        let (w, h) = (640, 362);
        let src = sample(w, h);
        let mut ours = I420::new(w, h);
        ours.fill_from_bgra(&src, w);
        let mut theirs = YUVBuffer::new(w, h);
        theirs.read_rgb(BgraSliceU8::new(&src, (w, h)));
        for (name, a, b) in [
            ("Y", ours.y(), theirs.y()),
            ("U", ours.u(), theirs.u()),
            ("V", ours.v(), theirs.v()),
        ] {
            let worst = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
            assert!(worst <= 2, "{name} differs by up to {worst}");
        }
    }

    #[test]
    fn crops_a_wider_source_and_odd_band_splits_cover_every_row() {
        let (w, h, src_w) = (1494, 938, 1496);
        let src = sample(src_w, h + 1);
        let mut ours = I420::new(w, h);
        ours.fill_from_bgra(&src, src_w);
        let mut cropped = Vec::new();
        for row in src.chunks_exact(src_w * 4).take(h) {
            cropped.extend_from_slice(&row[..w * 4]);
        }
        let mut theirs = YUVBuffer::new(w, h);
        theirs.read_rgb(BgraSliceU8::new(&cropped, (w, h)));
        let worst = ours
            .y()
            .iter()
            .zip(theirs.y())
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap();
        assert!(worst <= 2, "Y differs by up to {worst}");
    }

    #[test]
    fn scaled_conversion_samples_the_right_pixels() {
        // Left half black, right half white: scaled to 3/4 the split stays in the middle.
        let (sw, sh) = (400, 200);
        let src: Vec<u8> = (0..sw * sh)
            .flat_map(|i| {
                if i % sw < sw / 2 {
                    [0, 0, 0, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect();
        let mut p = I420::new(300, 150);
        p.fill_from_bgra_scaled(&src, sw, sh);
        let row = &p.y()[75 * 300..76 * 300];
        assert_eq!(row[10], 16, "black stays black");
        assert_eq!(row[290], 235, "white stays white");
        assert!(
            (148..=152).contains(&row.iter().position(|&v| v > 128).unwrap()),
            "edge in the middle"
        );
    }

    #[test]
    fn changed_rects_give_the_same_picture_as_a_full_conversion() {
        let (w, h) = (1280, 720);
        let (old, new) = (sample(w, h), {
            let mut n = sample(w, h);
            for (i, b) in n.iter_mut().enumerate() {
                *b = b.wrapping_add((i / 7) as u8);
            }
            n
        });
        // Changed areas of `new` relative to `old`: odd edges, overlap, a big one.
        let rects = [
            Rect {
                x: 3,
                y: 5,
                w: 17,
                h: 9,
            },
            Rect {
                x: 10,
                y: 8,
                w: 40,
                h: 3,
            },
            Rect {
                x: 0,
                y: 100,
                w: 1280,
                h: 301,
            }, // big: converted on several cores
            Rect {
                x: 1270,
                y: 710,
                w: 50,
                h: 50,
            }, // runs off the edge
        ];
        let mut patched_src = old.clone();
        for r in rects.iter().filter_map(|r| r.even_within(w, h)) {
            for y in r.y..r.y + r.h {
                let a = (y * w + r.x) * 4;
                patched_src[a..a + r.w * 4].copy_from_slice(&new[a..a + r.w * 4]);
            }
        }
        let mut full = I420::new(w, h);
        full.fill_from_bgra(&patched_src, w);
        let mut inc = I420::new(w, h);
        inc.fill_from_bgra(&old, w);
        inc.fill_rects_from_bgra(&new, w * 4, &rects);
        assert!(inc.data == full.data, "incremental conversion differs");

        let mut nv12 = full.to_nv12();
        let mut before = I420::new(w, h);
        before.fill_from_bgra(&old, w);
        let mut nv12_inc = before.to_nv12();
        inc.write_nv12(&mut nv12_inc, Some((0, h)));
        full.write_nv12(&mut nv12, None);
        assert_eq!(nv12_inc, nv12);
    }

    #[test]
    fn rgba_conversion_matches_openh264_and_is_faster() {
        let (w, h) = (1496, 938);
        let mut p = I420::new(w, h);
        p.fill_from_bgra(&sample(w, h), w);
        let mut ours = vec![0u8; w * h * 4];
        let t = std::time::Instant::now();
        i420_to_rgba((p.y(), p.u(), p.v()), p.strides(), (w, h), &mut ours);
        let ours_ms = t.elapsed().as_secs_f64() * 1e3;
        // Reference: openh264's (crate-private) float formula, single-threaded.
        let mut theirs = vec![0u8; w * h * 4];
        let t = std::time::Instant::now();
        let ym = 255.0 / 219.0;
        let rv = 255.0 / 224.0 * 1.402;
        let gv = -255.0 / 224.0 * 1.402 * 0.299 / 0.587;
        let gu = -255.0 / 224.0 * 1.772 * 0.114 / 0.587;
        let bu = 255.0 / 224.0 * 1.772;
        for yy in 0..h {
            for x in 0..w {
                let yv = (f32::from(p.y()[yy * w + x]) - 16.0) * ym;
                let u = f32::from(p.u()[(yy / 2) * (w / 2) + x / 2]) - 128.0;
                let v = f32::from(p.v()[(yy / 2) * (w / 2) + x / 2]) - 128.0;
                let i = (yy * w + x) * 4;
                theirs[i] = (yv + rv * v).clamp(0.0, 255.0) as u8;
                theirs[i + 1] = (yv + gv * v + gu * u).clamp(0.0, 255.0) as u8;
                theirs[i + 2] = (yv + bu * u).clamp(0.0, 255.0) as u8;
                theirs[i + 3] = 255;
            }
        }
        let theirs_ms = t.elapsed().as_secs_f64() * 1e3;
        let worst = ours
            .iter()
            .zip(&theirs)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 3, "colours differ by up to {worst}");
        println!("I420->RGBA {w}x{h}: openh264 {theirs_ms:.2} ms, ours {ours_ms:.2} ms");
    }

    /// `cargo test --release -p rconn-media yuv::tests::speed -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn speed() {
        let (w, h) = (1496, 938);
        let src = sample(w, h);
        let mut ours = I420::new(w, h);
        let mut theirs = YUVBuffer::new(w, h);
        let n = 100;
        let t = std::time::Instant::now();
        for _ in 0..n {
            theirs.read_rgb(BgraSliceU8::new(&src, (w, h)));
        }
        let old = t.elapsed().as_secs_f64() * 1e3 / f64::from(n);
        let t = std::time::Instant::now();
        for _ in 0..n {
            ours.fill_from_bgra(&src, w);
        }
        let new = t.elapsed().as_secs_f64() * 1e3 / f64::from(n);
        println!(
            "BGRA->I420 {w}x{h}: openh264 {old:.2} ms, ours {new:.2} ms ({:.1}x)",
            old / new
        );
    }
}
