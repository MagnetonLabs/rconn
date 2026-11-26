//! Scrolling and window moves sent as "copy this rectangle" instead of re-encoded pixels
//! (RDP's SurfaceToSurface).
//!
//! The H.264 decoder's reference pictures are the encoder's own, so the viewer can't just
//! move pixels around in the decoded picture: the next P-frame would be predicted from a
//! picture the encoder never had. Instead the H.264 stream stops being the picture and
//! becomes a *canvas* the viewer copies from:
//!
//! * the host keeps the canvas it feeds the encoder, and a model of what the viewer shows
//!   ([`Composer`]). For each new screen picture it finds what moved (DXGI's move rects on
//!   Windows, row hashing everywhere: [`Move`]), then the 16x16 tiles that still differ
//!   once the moves are applied. Only those tiles are written into the canvas, so the
//!   encoder sees a small change (the strip a scroll uncovered), not a whole moved screen.
//! * Each access unit carries its instructions in-band, as an SEI message
//!   ([`FrameOps`]): moves first, then the regions of this decoded picture to show.
//!   In-band, the instructions can never arrive without their picture or out of order
//!   with it; losing one is losing the picture, which the viewer already notices.
//! * The viewer keeps its own picture ([`Compositor`]): it applies the moves to it, then
//!   copies the listed regions of the decoded canvas in. A gap in the sequence numbers
//!   (or a picture without instructions) means it missed something: it freezes and asks
//!   for a keyframe, which comes with `full` (show the decoded picture as it is).
//!
//! Only viewers that ask for it get this stream (`Control::CopyRects`); others get plain
//! pictures. Measured on rendered text scrolling at 1920x1080: see `scroll_bench`.

use crate::video::DecodedFrame;
use crate::yuv::{I420, Rect};
use openh264::formats::YUVSource;
use std::borrow::Cow;
use std::collections::HashMap;

/// Changed areas are re-encoded in squares of this size (an H.264 macroblock).
pub const TILE: usize = 16;
/// Version of the instructions; sent in [`FrameOps`] and asked for by viewers.
pub const VERSION: u16 = 1;
/// Copy instructions are used unless `RCONN_COPY_RECTS=0` (host: not offered; viewer: not
/// asked for).
pub fn enabled() -> bool {
    std::env::var("RCONN_COPY_RECTS").map_or(true, |v| v != "0" && v != "off")
}

/// A move must cover this many rows to be worth an instruction.
const MIN_RUN: usize = 24;
/// Rows of the new picture that must agree on a scroll distance before it is checked.
const MIN_VOTES: u32 = 16;
/// Scroll detection hashes rows in vertical strips this wide, so that a panel that
/// doesn't scroll beside one that does still lets the scrolling one be found.
const STRIP: usize = 64;
/// A strip joins a scrolling area when this many of its rows voted for the distance.
const STRIP_VOTES: u32 = 3;
/// Rows of the old picture with more look-alikes than this don't vote (blank lines).
const MAX_TWINS: usize = 4;
const MAX_MOVES: usize = 16;

/// Copy the `to.w` x `to.h` block at (`from_x`, `from_y`) to `to`, in the picture the
/// viewer shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub from_x: usize,
    pub from_y: usize,
    pub to: Rect,
}

impl Move {
    pub fn src(&self) -> Rect {
        Rect {
            x: self.from_x,
            y: self.from_y,
            ..self.to
        }
    }

    fn offset(&self) -> (isize, isize) {
        (
            self.to.x as isize - self.from_x as isize,
            self.to.y as isize - self.from_y as isize,
        )
    }
}

/// How the viewer builds its picture from one decoded picture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameOps {
    /// Counts sent pictures; a gap means the viewer missed one.
    pub seq: u32,
    /// Show the decoded picture as it is (keyframes, a new size).
    pub full: bool,
    /// The picture's size.
    pub size: (usize, usize),
    /// Applied in order to the picture shown so far...
    pub moves: Vec<Move>,
    /// ...then these regions of the decoded picture are copied over it: whole [`TILE`]s
    /// (clipped to the picture), as [`tile_rects`] makes them.
    pub regions: Vec<Rect>,
}

/// SEI `user_data_unregistered` identifier of [`FrameOps`].
const UUID: [u8; 16] = *b"rconn/copyrect/1";

fn put_varint(out: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(bytes: &mut impl Iterator<Item = u8>) -> Option<usize> {
    let mut v = 0usize;
    for shift in (0..35).step_by(7) {
        let b = bytes.next()?;
        v |= usize::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Rectangles covering the set tiles of a `size` picture's tile grid (row-major, one
/// flag per [`TILE`]): runs along each tile row, joined downwards while a run repeats.
pub fn tile_rects(tiles: &[bool], (w, h): (usize, usize)) -> Vec<Rect> {
    let tiles_x = w.div_ceil(TILE);
    let mut rects: Vec<Rect> = Vec::new();
    // Rects that reach the tile row above, and could grow into this one.
    let mut open: Vec<usize> = Vec::new();
    for (ty, row) in tiles.chunks(tiles_x.max(1)).enumerate() {
        let (y0, y1) = (ty * TILE, ((ty + 1) * TILE).min(h));
        let mut still_open = Vec::new();
        let mut t = 0;
        while t < row.len() {
            if !row[t] {
                t += 1;
                continue;
            }
            let t0 = t;
            while t < row.len() && row[t] {
                t += 1;
            }
            let (x, rw) = (t0 * TILE, (t * TILE).min(w) - t0 * TILE);
            let grow = open
                .iter()
                .copied()
                .find(|&i| rects[i].x == x && rects[i].w == rw && rects[i].y + rects[i].h == y0);
            match grow {
                Some(i) => {
                    rects[i].h += y1 - y0;
                    still_open.push(i);
                }
                None => {
                    still_open.push(rects.len());
                    rects.push(Rect {
                        x,
                        y: y0,
                        w: rw,
                        h: y1 - y0,
                    });
                }
            }
        }
        open = still_open;
    }
    rects
}

impl FrameOps {
    fn payload(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(64 + 12 * self.moves.len());
        p.extend_from_slice(&UUID);
        p.extend_from_slice(&VERSION.to_be_bytes());
        p.push(u8::from(self.full));
        p.extend_from_slice(&self.seq.to_be_bytes());
        let (w, h) = self.size;
        for v in [w, h, self.moves.len()] {
            put_varint(&mut p, v);
        }
        for m in &self.moves {
            for v in [m.from_x, m.from_y, m.to.x, m.to.y, m.to.w, m.to.h] {
                put_varint(&mut p, v);
            }
        }
        // Regions as runs over the tile grid in raster order: clean, dirty, clean, ...
        let (tx, ty) = (w.div_ceil(TILE), h.div_ceil(TILE));
        let mut tiles = vec![false; tx * ty];
        for r in &self.regions {
            for y in r.y / TILE..(r.y + r.h).div_ceil(TILE).min(ty) {
                for x in r.x / TILE..(r.x + r.w).div_ceil(TILE).min(tx) {
                    tiles[y * tx + x] = true;
                }
            }
        }
        let mut runs = Vec::new();
        let (mut state, mut n) = (false, 0);
        for &t in &tiles {
            if t != state {
                runs.push(n);
                (state, n) = (t, 0);
            }
            n += 1;
        }
        if state {
            runs.push(n);
        }
        put_varint(&mut p, runs.len());
        for r in runs {
            put_varint(&mut p, r);
        }
        p
    }

    fn from_payload(p: &[u8]) -> Option<FrameOps> {
        let p = p.strip_prefix(&UUID)?;
        if u16::from_be_bytes(p.get(..2)?.try_into().ok()?) != VERSION {
            return None;
        }
        let full = *p.get(2)? != 0;
        let seq = u32::from_be_bytes(p.get(3..7)?.try_into().ok()?);
        let mut bytes = p.get(7..)?.iter().copied();
        let (w, h, n_moves) = (
            get_varint(&mut bytes)?,
            get_varint(&mut bytes)?,
            get_varint(&mut bytes)?,
        );
        if w > 1 << 15 || h > 1 << 15 || n_moves > MAX_MOVES {
            return None;
        }
        let mut moves = Vec::with_capacity(n_moves);
        for _ in 0..n_moves {
            let mut v = [0usize; 6];
            for x in &mut v {
                *x = get_varint(&mut bytes)?;
            }
            moves.push(Move {
                from_x: v[0],
                from_y: v[1],
                to: Rect {
                    x: v[2],
                    y: v[3],
                    w: v[4],
                    h: v[5],
                },
            });
        }
        let mut tiles = vec![false; w.div_ceil(TILE) * h.div_ceil(TILE)];
        let (mut at, mut dirty) = (0usize, false);
        for _ in 0..get_varint(&mut bytes)? {
            let n = get_varint(&mut bytes)?;
            let end = at.checked_add(n).filter(|&e| e <= tiles.len())?;
            tiles[at..end].fill(dirty);
            (at, dirty) = (end, !dirty);
        }
        Some(FrameOps {
            seq,
            full,
            size: (w, h),
            moves,
            regions: tile_rects(&tiles, (w, h)),
        })
    }

    /// These instructions as an SEI NAL unit, start code included.
    pub fn to_sei(&self) -> Vec<u8> {
        let payload = self.payload();
        let mut rbsp = vec![5u8]; // payload type: user_data_unregistered
        let mut n = payload.len();
        while n >= 255 {
            rbsp.push(255);
            n -= 255;
        }
        rbsp.push(n as u8);
        rbsp.extend_from_slice(&payload);
        rbsp.push(0x80); // rbsp_trailing_bits
        let mut nal = vec![0, 0, 0, 1, 0x06];
        let mut zeros = 0;
        for b in rbsp {
            // Emulation prevention: no start code may appear inside a NAL unit.
            if zeros >= 2 && b <= 3 {
                nal.push(3);
                zeros = 0;
            }
            nal.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        nal
    }

    /// The instructions in an SEI NAL unit (without start code), if it is ours.
    fn from_sei(nal: &[u8]) -> Option<FrameOps> {
        if nal.first()? & 0x1f != 6 {
            return None;
        }
        let mut rbsp = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in &nal[1..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            rbsp.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        let mut bytes = rbsp.iter().copied();
        let mut ff = || {
            let mut v = 0usize;
            loop {
                let b = bytes.next()?;
                v += usize::from(b);
                if b != 255 {
                    return Some(v);
                }
            }
        };
        let (kind, size) = (ff()?, ff()?);
        let at = rbsp.len() - bytes.len();
        if kind != 5 {
            return None;
        }
        Self::from_payload(rbsp.get(at..at + size)?)
    }
}

/// (start code offset, NAL unit offset, end) of each NAL unit of an Annex-B stream.
fn nal_spans(au: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let code = if i > 0 && au[i - 1] == 0 { i - 1 } else { i };
            starts.push((code, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends = starts.iter().skip(1).map(|s| s.0).chain([au.len()]);
    starts
        .iter()
        .zip(ends)
        .map(|(&(code, nal), end)| (code, nal, end))
        .collect()
}

/// Is this NAL unit a picture slice (non-IDR or IDR)?
fn is_slice(nal: &[u8]) -> bool {
    matches!(nal.first().map(|b| b & 0x1f), Some(1 | 5))
}

/// How many pictures an access unit holds (slices that start a picture:
/// `first_mb_in_slice` is 0, whose Exp-Golomb code is a single 1 bit).
pub fn pictures(au: &[u8]) -> usize {
    nal_spans(au)
        .into_iter()
        .map(|(_, s, e)| &au[s..e])
        .filter(|nal| is_slice(nal) && nal.get(1).is_some_and(|b| b & 0x80 != 0))
        .count()
}

/// `au` with `ops` in front of its first picture slice.
pub fn attach(mut au: Vec<u8>, ops: &FrameOps) -> Vec<u8> {
    let at = nal_spans(&au)
        .into_iter()
        .find(|&(_, s, e)| is_slice(&au[s..e]))
        .map_or(au.len(), |(code, _, _)| code);
    au.splice(at..at, ops.to_sei());
    au
}

/// Take our instructions out of an access unit: (what the decoder gets, instructions).
pub fn split(au: &[u8]) -> (Cow<'_, [u8]>, Option<FrameOps>) {
    for (code, s, e) in nal_spans(au) {
        if is_slice(&au[s..e]) {
            break; // SEI comes before the picture's slices
        }
        if let Some(ops) = FrameOps::from_sei(&au[s..e]) {
            let mut rest = au[..code].to_vec();
            rest.extend_from_slice(&au[e..]);
            return (Cow::Owned(rest), Some(ops));
        }
    }
    (Cow::Borrowed(au), None)
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// Builds what the encoder gets from each screen picture, and the instructions that go
/// with it. Use: [`Composer::prepare`], encode what it returns, then [`Composer::sent`]
/// with the output (or [`Composer::no_output`]).
#[derive(Default)]
pub struct Composer {
    /// What the encoder was given.
    canvas: Option<I420>,
    /// What the viewer shows, as screen pixels.
    shown: Option<I420>,
    seq: u32,
    want_full: bool,
    /// Instructions for the prepared picture, and the rows of `shown` it changes.
    prepared: Option<(FrameOps, (usize, usize))>,
    /// The encoder can't be trusted to give one picture per input: every picture is sent
    /// whole (`full`), as without copy instructions.
    passthrough: bool,
    /// Send the next picture without instructions (the viewer then asks for a keyframe).
    withhold_next: bool,
    last: PrepareStats,
}

/// What the last [`Composer::prepare`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PrepareStats {
    /// Share of the picture re-encoded (0..=1).
    pub encoded: f32,
    /// Share of the picture moved by instructions instead.
    pub moved: f32,
    pub moves: usize,
    /// The rows of the returned picture that differ from the one returned before (an
    /// even range), `None` for "any": for [`crate::video::VideoEncoder::encode_yuv`].
    pub rows: Option<(usize, usize)>,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next picture is sent whole (a keyframe was asked for).
    pub fn request_full(&mut self) {
        self.want_full = true;
    }

    /// Every picture whole from now on (see [`Composer::no_output`]).
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    pub fn last(&self) -> PrepareStats {
        self.last
    }

    /// The picture to encode for screen picture `picture`. `hints` are moves the OS
    /// reported since the last prepared picture (checked before use: they may describe
    /// several captures). Encode the result, then call [`Composer::sent`]. Also returns
    /// [`Composer::last`].
    pub fn prepare<'a>(
        &'a mut self,
        picture: &'a I420,
        hints: &[Move],
    ) -> (&'a I420, PrepareStats) {
        let (w, h) = picture.dimensions();
        if self.passthrough {
            self.prepared = Some((full_ops((w, h)), (0, h)));
            self.last = PrepareStats {
                encoded: 1.0,
                ..Default::default()
            };
            return (picture, self.last);
        }
        if self
            .canvas
            .as_ref()
            .is_none_or(|c| c.dimensions() != (w, h))
        {
            self.canvas = Some(I420::new(w, h));
            self.shown = Some(I420::new(w, h));
            self.want_full = true;
        }
        let (Some(canvas), Some(shown)) = (self.canvas.as_mut(), self.shown.as_ref()) else {
            unreachable!("allocated above");
        };
        if self.want_full {
            copy_rect(picture, canvas, whole(w, h));
            self.prepared = Some((full_ops((w, h)), (0, h)));
            self.last = PrepareStats {
                encoded: 1.0,
                ..Default::default()
            };
            return (canvas, self.last);
        }
        let diff = Diff::new(shown, picture);
        let Some(band) = diff.band else {
            self.prepared = Some((
                FrameOps {
                    size: (w, h),
                    ..Default::default()
                },
                (0, 0),
            ));
            self.last = PrepareStats {
                rows: Some((0, 0)),
                ..Default::default()
            };
            return (canvas, self.last);
        };
        let moves = find_moves(shown, picture, &diff, hints);
        let regions = dirty_regions(shown, picture, &diff, &moves);
        for r in &regions {
            copy_rect(picture, canvas, *r);
        }
        let area = |r: &Rect| (r.w * r.h) as f32 / (w * h) as f32;
        self.last = PrepareStats {
            encoded: regions.iter().map(area).sum(),
            moved: moves.iter().map(|m| area(&m.to)).sum(),
            moves: moves.len(),
            rows: Some(
                regions
                    .iter()
                    .map(|r| (r.y, r.y + r.h))
                    .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
                    .unwrap_or((0, 0)),
            ),
        };
        self.prepared = Some((
            FrameOps {
                size: (w, h),
                moves,
                regions,
                ..Default::default()
            },
            band,
        ));
        (canvas, self.last)
    }

    /// The encoder turned the prepared picture into `au`: the access unit to send, with
    /// its instructions. `picture` is the screen picture given to `prepare`.
    pub fn sent(&mut self, picture: &I420, au: Vec<u8>) -> Vec<u8> {
        let Some((mut ops, band)) = self.prepared.take() else {
            return au;
        };
        if pictures(&au) != 1 || std::mem::take(&mut self.withhold_next) {
            // Not exactly this picture (an encoder that delays or merges them): without
            // instructions, the viewer knows it lost track and asks for a keyframe.
            self.want_full = true;
            return au;
        }
        self.seq = self.seq.wrapping_add(1);
        ops.seq = self.seq;
        if ops.full {
            self.want_full = false;
        }
        if let Some(shown) = self.shown.as_mut().filter(|_| !self.passthrough) {
            // `shown` now matches `picture` everywhere (the moved areas were checked).
            copy_rows(picture, shown, band);
        }
        attach(au, &ops)
    }

    /// The encoder gave nothing for the prepared picture. `skipped` says it dropped it
    /// (openh264's rate control). Otherwise it may hand it out later, with a later
    /// picture's instructions: from then on every picture is sent whole.
    pub fn no_output(&mut self, skipped: bool) {
        self.prepared = None;
        if !skipped && !self.passthrough {
            self.passthrough = true;
            self.withhold_next = true;
        }
    }
}

fn full_ops(size: (usize, usize)) -> FrameOps {
    FrameOps {
        full: true,
        size,
        ..Default::default()
    }
}

fn whole(w: usize, h: usize) -> Rect {
    Rect { x: 0, y: 0, w, h }
}

/// Copy rectangle `r` (even coordinates) from `src` to the same place in `dst`.
fn copy_rect(src: &I420, dst: &mut I420, r: Rect) {
    let (w, _) = src.dimensions();
    let (dy, du, dv) = dst.planes_mut();
    for y in r.y..r.y + r.h {
        let o = y * w + r.x;
        dy[o..o + r.w].copy_from_slice(&src.y()[o..o + r.w]);
    }
    let cw = w / 2;
    for cy in r.y / 2..(r.y + r.h).div_ceil(2) {
        let o = cy * cw + r.x / 2;
        let n = r.w.div_ceil(2);
        du[o..o + n].copy_from_slice(&src.u()[o..o + n]);
        dv[o..o + n].copy_from_slice(&src.v()[o..o + n]);
    }
}

fn copy_rows(src: &I420, dst: &mut I420, (r0, r1): (usize, usize)) {
    if r1 > r0 {
        let (w, h) = src.dimensions();
        let r0 = r0 & !1;
        let r1 = r1.next_multiple_of(2).min(h);
        copy_rect(
            src,
            dst,
            Rect {
                x: 0,
                y: r0,
                w,
                h: r1 - r0,
            },
        );
    }
}

/// Which rows of a new picture differ from the old one.
struct Diff {
    /// Per row: the changed columns `[x0, x1)`, or `None` when the row is unchanged.
    rows: Vec<Option<(usize, usize)>>,
    /// The changed rows `[first, last + 1)`.
    band: Option<(usize, usize)>,
}

fn span(a: &[u8], b: &[u8]) -> Option<(usize, usize)> {
    if a == b {
        return None;
    }
    let x0 = a.iter().zip(b).position(|(p, q)| p != q)?;
    let x1 = a.len()
        - a.iter()
            .rev()
            .zip(b.iter().rev())
            .position(|(p, q)| p != q)?;
    Some((x0, x1))
}

fn union(a: Option<(usize, usize)>, b: Option<(usize, usize)>) -> Option<(usize, usize)> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
        (a, b) => a.or(b),
    }
}

impl Diff {
    fn new(old: &I420, new: &I420) -> Diff {
        let (w, h) = new.dimensions();
        let cw = w / 2;
        let mut rows = vec![None; h];
        for cy in 0..h / 2 {
            let c = cy * cw..(cy + 1) * cw;
            let chroma = union(
                span(&old.u()[c.clone()], &new.u()[c.clone()]),
                span(&old.v()[c.clone()], &new.v()[c]),
            )
            .map(|(a, b)| (2 * a, 2 * b));
            for y in [2 * cy, 2 * cy + 1] {
                let l = y * w..(y + 1) * w;
                rows[y] = union(span(&old.y()[l.clone()], &new.y()[l]), chroma);
            }
        }
        let first = rows.iter().position(Option::is_some);
        let last = rows.iter().rposition(Option::is_some);
        Diff {
            band: first.zip(last).map(|(a, b)| (a, b + 1)),
            rows,
        }
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let (words, tail) = bytes.as_chunks::<8>();
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for w in words {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(*w)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    for &b in tail {
        h = (h.rotate_left(5) ^ u64::from(b)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    h
}

fn uniform(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == bytes[0])
}

/// Vertical scroll distances that rows of the changed band agree on, best first, each with
/// the strips that voted for it.
fn vote_vertical(old: &I420, new: &I420, diff: &Diff) -> Vec<(isize, Vec<bool>)> {
    let (w, h) = new.dimensions();
    let Some((r0, r1)) = diff.band else {
        return Vec::new();
    };
    let strips = w / STRIP;
    if strips == 0 || r1 - r0 < MIN_RUN {
        return Vec::new();
    }
    fn segment(plane: &[u8], w: usize, y: usize, s: usize) -> &[u8] {
        &plane[y * w + s * STRIP..][..STRIP]
    }
    let key = |s: usize, seg: &[u8]| hash(seg) ^ (s as u64).wrapping_mul(0x2545_f491_4f6c_dd1d);
    // (strip, hash of the old row's segment) -> how many rows have it, and the first few.
    let mut old_rows: HashMap<u64, (usize, [u32; MAX_TWINS])> =
        HashMap::with_capacity((r1 - r0) * strips);
    for y in r0..r1 {
        for s in 0..strips {
            let seg = segment(old.y(), w, y, s);
            if !uniform(seg) {
                let e = old_rows.entry(key(s, seg)).or_insert((0, [0; MAX_TWINS]));
                if e.0 < MAX_TWINS {
                    e.1[e.0] = y as u32;
                }
                e.0 += 1;
            }
        }
    }
    let mut votes: HashMap<isize, (u32, Vec<u32>)> = HashMap::new();
    for y in (r0..r1).filter(|&y| diff.rows[y].is_some()) {
        for s in 0..strips {
            let seg = segment(new.y(), w, y, s);
            if uniform(seg) {
                continue;
            }
            let Some((n, from)) = old_rows.get(&key(s, seg)) else {
                continue;
            };
            if *n > MAX_TWINS {
                continue;
            }
            for &f in &from[..*n] {
                let dy = y as isize - f as isize;
                if dy != 0 && dy.unsigned_abs() < h {
                    let v = votes.entry(dy).or_insert_with(|| (0, vec![0; strips]));
                    v.0 += 1;
                    v.1[s] += 1;
                }
            }
        }
    }
    let mut best: Vec<_> = votes
        .into_iter()
        .filter(|(_, (n, _))| *n >= MIN_VOTES)
        .collect();
    best.sort_by_key(|(dy, (n, _))| (std::cmp::Reverse(*n), dy.unsigned_abs()));
    best.into_iter()
        .take(2)
        .map(|(dy, (_, per_strip))| (dy, per_strip.iter().map(|&n| n >= STRIP_VOTES).collect()))
        .collect()
}

/// Column ranges of voting strips, joined across gaps of up to two strips.
fn strip_ranges(voted: &[bool], w: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (s, _) in voted.iter().enumerate().filter(|(_, v)| **v) {
        let (x0, x1) = (s * STRIP, ((s + 1) * STRIP).min(w));
        match out.last_mut() {
            Some(last) if x0 <= last.1 + 2 * STRIP => last.1 = x1,
            _ => out.push((x0, x1)),
        }
    }
    out
}

/// Does row `y` of `new` over columns `[x0, x1)` equal `old` shifted by (`dx`, `dy`)?
fn row_moved(
    old: &I420,
    new: &I420,
    y: usize,
    (x0, x1): (usize, usize),
    (dx, dy): (isize, isize),
) -> bool {
    let (w, h) = new.dimensions();
    let (sy, sx0) = (y as isize - dy, x0 as isize - dx);
    if sy < 0 || sy as usize >= h || sx0 < 0 || sx0 as usize + (x1 - x0) > w {
        return false;
    }
    let (sy, sx0) = (sy as usize, sx0 as usize);
    new.y()[y * w + x0..y * w + x1] == old.y()[sy * w + sx0..sy * w + sx0 + (x1 - x0)]
}

/// The moves of offset (`dx`, `dy`) that hold over columns `cols` within rows `rows`:
/// runs of at least [`MIN_RUN`] matching rows, widened while whole columns still match.
/// Ordered so that applying them one after another never reads an area already written.
fn runs(
    old: &I420,
    new: &I420,
    (dx, dy): (isize, isize),
    cols: (usize, usize),
    rows: (usize, usize),
) -> Vec<Move> {
    let (w, _) = new.dimensions();
    let mut out = Vec::new();
    let mut start = None;
    for y in rows.0..=rows.1 {
        let hit = y < rows.1 && row_moved(old, new, y, cols, (dx, dy));
        match (hit, start) {
            (true, None) => start = Some(y),
            (false, Some(s)) => {
                start = None;
                if y - s >= MIN_RUN {
                    out.push((s, y));
                }
            }
            _ => {}
        }
    }
    let column_moves = |cols: (usize, usize), (y0, y1): (usize, usize)| {
        (y0..y1).all(|y| row_moved(old, new, y, cols, (dx, dy)))
    };
    let mut moves: Vec<Move> = out
        .into_iter()
        .map(|(y0, y1)| {
            let (mut x0, mut x1) = cols;
            // A block of columns at a time, then single ones.
            for step in [TILE, 1] {
                while x0 >= step && column_moves((x0 - step, x0), (y0, y1)) {
                    x0 -= step;
                }
                while x1 + step <= w && column_moves((x1, x1 + step), (y0, y1)) {
                    x1 += step;
                }
            }
            Move {
                from_x: (x0 as isize - dx) as usize,
                from_y: (y0 as isize - dy) as usize,
                to: Rect {
                    x: x0,
                    y: y0,
                    w: x1 - x0,
                    h: y1 - y0,
                },
            }
        })
        .collect();
    // Moving down: the lowest first (it reads rows that the ones above it will write).
    if dy > 0 {
        moves.reverse();
    }
    moves
}

/// [`runs`] of a vertical scroll over `cols`, also tried without the first and/or last
/// strip: a strip at the edge of a scrolling area can hold something that moves
/// differently (a scroll bar's thumb), which would cut every run at its rows.
fn scroll_runs(
    old: &I420,
    new: &I420,
    dy: isize,
    (x0, x1): (usize, usize),
    band: (usize, usize),
) -> Vec<Move> {
    let area = |g: &Vec<Move>| g.iter().map(|m| m.to.w * m.to.h).sum::<usize>();
    let mut best = runs(old, new, (0, dy), (x0, x1), band);
    if x1 - x0 > 3 * STRIP {
        for cols in [(x0 + STRIP, x1), (x0, x1 - STRIP), (x0 + STRIP, x1 - STRIP)] {
            let g = runs(old, new, (0, dy), cols, band);
            if area(&g) > area(&best) {
                best = g;
            }
        }
    }
    best
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// What moved between `old` and `new`: OS hints that check out, then vertical scrolls
/// found by row hashing.
fn find_moves(old: &I420, new: &I420, diff: &Diff, hints: &[Move]) -> Vec<Move> {
    let (w, h) = new.dimensions();
    let Some(band) = diff.band else {
        return Vec::new();
    };
    let mut groups: Vec<Vec<Move>> = Vec::new();
    for hint in hints {
        let (to, src) = (hint.to, hint.src());
        let inside = |r: Rect| r.w > 0 && r.h > 0 && r.x + r.w <= w && r.y + r.h <= h;
        if !inside(to) || !inside(src) || hint.offset() == (0, 0) {
            continue;
        }
        groups.push(runs(
            old,
            new,
            hint.offset(),
            (to.x, to.x + to.w),
            (to.y, to.y + to.h),
        ));
    }
    // One group per scrolling area; widening can make two areas of one distance overlap,
    // so they are groups of their own, the largest first.
    let mut scrolls: Vec<Vec<Move>> = vote_vertical(old, new, diff)
        .into_iter()
        .flat_map(|(dy, voted)| {
            strip_ranges(&voted, w)
                .into_iter()
                .map(move |cols| (dy, cols))
        })
        .map(|(dy, cols)| scroll_runs(old, new, dy, cols, band))
        .filter(|g| !g.is_empty())
        .collect();
    let area = |g: &Vec<Move>| g.iter().map(|m| m.to.w * m.to.h).sum::<usize>();
    scrolls.sort_by_key(|g| std::cmp::Reverse(area(g)));
    groups.extend(scrolls);
    // Moves of different groups must not touch each other's areas (their order would
    // matter); within a group, `runs` ordered them.
    let mut moves: Vec<Move> = Vec::new();
    for group in groups {
        let clear = group.iter().all(|m| {
            moves.iter().all(|o| {
                !overlaps(&m.to, &o.to) && !overlaps(&m.src(), &o.to) && !overlaps(&m.to, &o.src())
            })
        });
        if clear && moves.len() + group.len() <= MAX_MOVES {
            moves.extend(group);
        }
    }
    moves
}

/// `[x0, x1)` minus the columns `moves` cover at row `y`.
fn uncovered(moves: &[Move], y: usize, (x0, x1): (usize, usize)) -> Vec<(usize, usize)> {
    let mut cover: Vec<(usize, usize)> = moves
        .iter()
        .filter(|m| (m.to.y..m.to.y + m.to.h).contains(&y))
        .map(|m| (m.to.x, m.to.x + m.to.w))
        .collect();
    cover.sort_unstable();
    let mut out = Vec::new();
    let mut at = x0;
    for (c0, c1) in cover {
        if c0 > at {
            out.push((at, c0.min(x1)));
        }
        at = at.max(c1);
        if at >= x1 {
            break;
        }
    }
    if at < x1 {
        out.push((at, x1));
    }
    out.retain(|(a, b)| b > a);
    out
}

/// The tiles of `new` that differ from what the viewer shows once `moves` are applied
/// (`old` elsewhere), as rectangles.
fn dirty_regions(old: &I420, new: &I420, diff: &Diff, moves: &[Move]) -> Vec<Rect> {
    let (w, h) = new.dimensions();
    let cw = w / 2;
    let tiles_x = w.div_ceil(TILE);
    let mut tiles = vec![false; tiles_x * h.div_ceil(TILE)];
    for (ty, row_tiles) in tiles.chunks_mut(tiles_x).enumerate() {
        let rows = ty * TILE..((ty + 1) * TILE).min(h);
        for (y, changed) in rows.clone().zip(&diff.rows[rows]) {
            let Some(changed) = *changed else {
                continue;
            };
            for (a, b) in uncovered(moves, y, changed) {
                let span = a / TILE..b.div_ceil(TILE);
                for (t, dirty) in span.clone().zip(&mut row_tiles[span]) {
                    if *dirty {
                        continue;
                    }
                    let (x0, x1) = ((t * TILE).max(a), ((t + 1) * TILE).min(b));
                    let luma = new.y()[y * w + x0..y * w + x1] != old.y()[y * w + x0..y * w + x1];
                    let c = (y / 2) * cw + x0 / 2..(y / 2) * cw + x1.div_ceil(2);
                    *dirty = luma
                        || new.u()[c.clone()] != old.u()[c.clone()]
                        || new.v()[c.clone()] != old.v()[c];
                }
            }
        }
    }
    tile_rects(&tiles, (w, h))
}

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

/// The viewer's picture, built from decoded canvases and their [`FrameOps`].
#[derive(Default)]
pub struct Compositor {
    /// RGBA, `dims.0 * 4` bytes per row.
    surface: Vec<u8>,
    dims: (usize, usize),
    /// Sequence number of the last picture applied; `None` while out of step.
    seq: Option<u32>,
    /// The host sends instructions (so a picture without them means one was missed).
    active: bool,
}

/// What to do with a decoded picture after [`Compositor::compose`].
#[derive(Debug, PartialEq, Eq)]
pub enum Composed {
    /// Show it (it now holds the composed picture).
    Show,
    /// Out of step with the host: keep showing the previous picture and ask for a
    /// keyframe.
    Resync,
}

impl Compositor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Video was lost: wait for a whole picture.
    pub fn lost(&mut self) {
        self.seq = None;
    }

    /// Waiting for a whole picture (after a loss)?
    pub fn waiting(&self) -> bool {
        self.active && self.seq.is_none()
    }

    /// Turn decoded `frame` into the picture to show, given the instructions that came
    /// with it (see [`split`]).
    pub fn compose(&mut self, ops: Option<&FrameOps>, frame: &mut DecodedFrame) -> Composed {
        let dims = (frame.width, frame.height);
        let Some(ops) = ops else {
            if self.active {
                self.seq = None;
                return Composed::Resync;
            }
            return Composed::Show;
        };
        self.active = true;
        if frame.rgba.len() != dims.0 * dims.1 * 4 || ops.size != dims {
            self.seq = None;
            return Composed::Resync;
        }
        if ops.full {
            self.surface.clear();
            self.surface.extend_from_slice(&frame.rgba);
            self.dims = dims;
            self.seq = Some(ops.seq);
            return Composed::Show;
        }
        let in_step = self.seq.is_some_and(|s| ops.seq == s.wrapping_add(1))
            && self.dims == dims
            && ops.size == dims;
        if !in_step || !apply(&mut self.surface, &frame.rgba, dims, ops) {
            self.seq = None;
            return Composed::Resync;
        }
        self.seq = Some(ops.seq);
        frame.rgba.copy_from_slice(&self.surface);
        Composed::Show
    }
}

/// Apply `ops` to RGBA `surface` with pixels from `decoded` (same size). `false`, and
/// nothing changed, if an instruction doesn't fit the picture.
fn apply(surface: &mut [u8], decoded: &[u8], (w, h): (usize, usize), ops: &FrameOps) -> bool {
    let fits = |r: &Rect| r.w > 0 && r.h > 0 && r.x + r.w <= w && r.y + r.h <= h;
    if surface.len() != w * h * 4
        || decoded.len() != surface.len()
        || !ops.moves.iter().all(|m| fits(&m.to) && fits(&m.src()))
        || !ops.regions.iter().all(fits)
    {
        return false;
    }
    let stride = w * 4;
    for m in &ops.moves {
        let n = m.to.w * 4;
        let mut copy_row = |i: usize| {
            let s = (m.from_y + i) * stride + m.from_x * 4;
            let d = (m.to.y + i) * stride + m.to.x * 4;
            surface.copy_within(s..s + n, d);
        };
        // Overlapping source and destination: never read a row already written.
        if m.to.y > m.from_y {
            (0..m.to.h).rev().for_each(&mut copy_row);
        } else {
            (0..m.to.h).for_each(&mut copy_row);
        }
    }
    for r in &ops.regions {
        for y in r.y..r.y + r.h {
            let o = y * stride + r.x * 4;
            surface[o..o + r.w * 4].copy_from_slice(&decoded[o..o + r.w * 4]);
        }
    }
    true
}

#[cfg(all(test, not(target_os = "ios")))]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 640;
    const H: usize = 480;

    /// A tall "document": lines of glyph-like marks in a few colours on white.
    fn document(h: usize) -> Vec<u8> {
        let mut bgra = vec![255u8; W * h * 4];
        let mut seed = 0x1234_5679_u32;
        let mut rand = |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % n
        };
        for line in 0..h / 18 {
            let colour = [[30, 30, 30], [200, 60, 20], [20, 120, 40]][rand(3) as usize];
            let end = 140 + rand(480) as usize;
            for y in line * 18 + 3..line * 18 + 14 {
                for x in 130..end {
                    if rand(3) == 0 {
                        let i = (y * W + x) * 4;
                        bgra[i..i + 3].copy_from_slice(&colour);
                    }
                }
            }
        }
        bgra
    }

    /// The screen: a static side panel (x < 120) and the document scrolled to `offset`.
    fn screen(doc: &[u8], offset: usize) -> I420 {
        let mut bgra = vec![0u8; W * H * 4];
        for y in 0..H {
            for x in 0..W {
                let i = (y * W + x) * 4;
                let px = if x < 120 {
                    [(x * 2) as u8, (y / 2) as u8, 90, 255]
                } else {
                    let s = ((y + offset) * W + x) * 4;
                    [doc[s], doc[s + 1], doc[s + 2], 255]
                };
                bgra[i..i + 4].copy_from_slice(&px);
            }
        }
        let mut pic = I420::new(W, H);
        pic.fill_from_bgra(&bgra, W);
        pic
    }

    fn rgba(pic: &I420) -> DecodedFrame {
        let (w, h) = pic.dimensions();
        let mut rgba = vec![0u8; w * h * 4];
        crate::yuv::i420_to_rgba(
            (pic.y(), pic.u(), pic.v()),
            pic.strides(),
            (w, h),
            &mut rgba,
        );
        DecodedFrame {
            width: w,
            height: h,
            rgba,
        }
    }

    const FAKE_AU: [u8; 7] = [0, 0, 0, 1, 0x65, 0x88, 0x84];

    /// Host and viewer with a lossless "codec" (the canvas is decoded exactly): the
    /// instructions sent and the viewer's picture.
    fn stream(
        composer: &mut Composer,
        compositor: &mut Compositor,
        pic: &I420,
        hints: &[Move],
    ) -> (FrameOps, DecodedFrame) {
        let (canvas, _) = composer.prepare(pic, hints);
        let mut decoded = rgba(canvas);
        let au = composer.sent(pic, FAKE_AU.to_vec());
        let (rest, ops) = split(&au);
        assert_eq!(&rest[..], &FAKE_AU, "SEI taken out");
        let ops = ops.expect("instructions attached");
        assert_eq!(compositor.compose(Some(&ops), &mut decoded), Composed::Show);
        (ops, decoded)
    }

    #[test]
    fn instructions_survive_the_bitstream() {
        let size = (1920, 1080);
        // A strip, a column and scattered tiles, touching the clipped last tile row.
        let tiles: Vec<bool> = (0..120 * 68)
            .map(|i| {
                let (x, y) = (i % 120, i / 120);
                y == 67 || (y > 60 && x < 100) || x == 119 || (x * 7 + y * 3) % 11 == 0
            })
            .collect();
        let ops = FrameOps {
            seq: 0x0000_0300,
            full: false,
            size,
            moves: vec![Move {
                from_x: 0,
                from_y: 3,
                to: Rect {
                    x: 0,
                    y: 0,
                    w: 256,
                    h: 1,
                },
            }],
            regions: tile_rects(&tiles, size),
        };
        assert_eq!(ops.regions.last().map(|r| r.y + r.h), Some(1080));
        let sei = ops.to_sei();
        assert!(
            !sei[4..]
                .windows(3)
                .any(|w| w == [0, 0, 1] || w == [0, 0, 0]),
            "no start code inside the NAL unit"
        );
        let au = vec![
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 0x88, 4, 0, 0, 1, 0x65,
            0x08, 5,
        ];
        assert_eq!(pictures(&au), 1, "the second slice continues the picture");
        let with = attach(au.clone(), &ops);
        assert_eq!(&with[..12], &au[..12], "after the parameter sets");
        assert_eq!(pictures(&with), 1);
        let (rest, back) = split(&with);
        assert_eq!(&rest[..], &au[..]);
        assert_eq!(back, Some(ops));
        let (rest, none) = split(&au);
        assert!(matches!(rest, Cow::Borrowed(_)) && none.is_none());
    }

    #[test]
    fn a_scroll_becomes_a_move_and_a_strip() {
        let doc = document(H + 400);
        let mut composer = Composer::new();
        let mut compositor = Compositor::new();
        let (ops, _) = stream(&mut composer, &mut compositor, &screen(&doc, 0), &[]);
        assert!(ops.full, "the first picture is whole");
        let mut odd = false;
        for (offset, dy) in [
            (100isize, -100isize),
            (140, -40),
            (100, 40),
            (137, -37),
            (140, -3),
        ] {
            odd |= dy % 2 != 0;
            let pic = screen(&doc, offset as usize);
            let (ops, shown) = stream(&mut composer, &mut compositor, &pic, &[]);
            assert!(!ops.full);
            let stats = composer.last();
            assert!(!ops.moves.is_empty(), "scroll by {dy} found");
            assert!(ops.moves.iter().all(|m| m.offset() == (0, dy)), "{ops:?}");
            assert!(stats.moved > 0.5, "most of the screen moved: {stats:?}");
            assert!(
                stats.encoded < 0.1 + dy.unsigned_abs() as f32 / H as f32,
                "only the uncovered strip is re-encoded: {stats:?}"
            );
            // The panel never changes: nothing of it moves.
            assert!(ops.moves.iter().all(|m| m.to.x >= 120 && m.from_x >= 120));
            let truth = rgba(&pic);
            if !odd {
                assert!(
                    shown.rgba == truth.rgba,
                    "scrolled by {dy}: picture differs"
                );
            } else {
                // Odd distances move whole pixels whose 4:2:0 chroma was paired
                // differently: right, but not bit-identical to a fresh conversion.
                let off = shown
                    .rgba
                    .iter()
                    .zip(&truth.rgba)
                    .filter(|(a, b)| a.abs_diff(**b) > 60)
                    .count();
                assert!(off < W * H / 50, "scrolled by {dy}: {off} bytes off");
            }
        }
    }

    #[test]
    fn os_hints_carry_window_moves_and_are_checked() {
        let doc = document(H);
        let base = screen(&doc, 0);
        // A 200x160 part of the screen moved right and down (as a dragged window).
        let mut moved = screen(&doc, 0);
        let snapshot = base.y().to_vec();
        let (y_plane, u_plane, v_plane) = moved.planes_mut();
        let (bu, bv) = (base.u().to_vec(), base.v().to_vec());
        for y in 0..160 {
            let (s, d) = ((100 + y) * W + 150, (150 + y) * W + 230);
            y_plane[d..d + 200].copy_from_slice(&snapshot[s..s + 200]);
        }
        for cy in 0..80 {
            let (s, d) = ((50 + cy) * W / 2 + 75, (75 + cy) * W / 2 + 115);
            u_plane[d..d + 100].copy_from_slice(&bu[s..s + 100]);
            v_plane[d..d + 100].copy_from_slice(&bv[s..s + 100]);
        }
        let hint = Move {
            from_x: 150,
            from_y: 100,
            to: Rect {
                x: 230,
                y: 150,
                w: 200,
                h: 160,
            },
        };
        let mut composer = Composer::new();
        let mut compositor = Compositor::new();
        stream(&mut composer, &mut compositor, &base, &[]);
        let (ops, shown) = stream(&mut composer, &mut compositor, &moved, &[hint]);
        assert_eq!(ops.moves, vec![hint]);
        assert!(shown.rgba == rgba(&moved).rgba);
        assert!(composer.last().encoded < 0.1, "{:?}", composer.last());
        // A stale hint (nothing moved that way) is dropped, not trusted.
        let (ops, shown) = stream(&mut composer, &mut compositor, &base, &[hint]);
        assert!(ops.moves.iter().all(|m| *m != hint), "{ops:?}");
        assert!(shown.rgba == rgba(&base).rgba);
    }

    #[test]
    fn a_dropped_picture_is_folded_into_the_next() {
        let doc = document(H + 200);
        let mut composer = Composer::new();
        let mut compositor = Compositor::new();
        stream(&mut composer, &mut compositor, &screen(&doc, 0), &[]);
        let skipped = screen(&doc, 20);
        let _ = composer.prepare(&skipped, &[]);
        composer.no_output(true);
        let pic = screen(&doc, 50);
        let (ops, shown) = stream(&mut composer, &mut compositor, &pic, &[]);
        assert_eq!(ops.seq, 2, "no gap: the dropped picture was never numbered");
        assert!(!ops.moves.is_empty(), "{ops:?}");
        assert!(ops.moves.iter().all(|m| m.offset() == (0, -50)), "{ops:?}");
        assert!(shown.rgba == rgba(&pic).rgba);
    }

    #[test]
    fn an_encoder_that_may_delay_pictures_gets_whole_ones() {
        let doc = document(H + 200);
        let mut composer = Composer::new();
        let mut compositor = Compositor::new();
        stream(&mut composer, &mut compositor, &screen(&doc, 0), &[]);
        let _ = composer.prepare(&screen(&doc, 10), &[]);
        composer.no_output(false);
        assert!(composer.is_passthrough());
        let pic = screen(&doc, 30);
        let (got, _) = composer.prepare(&pic, &[]);
        assert!(std::ptr::eq(got, &pic), "the screen itself is encoded");
        let au = composer.sent(&pic, FAKE_AU.to_vec());
        assert!(split(&au).1.is_none(), "the next picture says nothing...");
        let mut f = rgba(&pic);
        assert_eq!(compositor.compose(None, &mut f), Composed::Resync);
        let _ = composer.prepare(&pic, &[]);
        let au = composer.sent(&pic, FAKE_AU.to_vec());
        let ops = split(&au).1.expect("instructions");
        assert!(ops.full, "...then every picture is whole");
        assert_eq!(compositor.compose(Some(&ops), &mut f), Composed::Show);
    }

    #[test]
    fn an_access_unit_with_two_pictures_gets_no_instructions() {
        let doc = document(H);
        let mut composer = Composer::new();
        let pic = screen(&doc, 0);
        let _ = composer.prepare(&pic, &[]);
        let two = [FAKE_AU, FAKE_AU].concat();
        assert_eq!(pictures(&two), 2);
        assert!(split(&composer.sent(&pic, two)).1.is_none());
        let _ = composer.prepare(&pic, &[]);
        let ops = split(&composer.sent(&pic, FAKE_AU.to_vec())).1.unwrap();
        assert!(ops.full, "whole again after that");
    }

    #[test]
    fn the_viewer_resyncs_after_any_gap() {
        let mut c = Compositor::new();
        let frame = |v: u8| DecodedFrame {
            width: 32,
            height: 32,
            rgba: vec![v; 32 * 32 * 4],
        };
        let ops = |seq, full, regions: Vec<Rect>| FrameOps {
            seq,
            full,
            size: (32, 32),
            moves: Vec::new(),
            regions,
        };
        let tile = Rect {
            x: 16,
            y: 0,
            w: 16,
            h: 16,
        };
        assert_eq!(
            c.compose(None, &mut frame(1)),
            Composed::Show,
            "plain video"
        );
        assert_eq!(
            c.compose(Some(&ops(7, true, vec![])), &mut frame(1)),
            Composed::Show
        );
        let mut f = frame(2);
        assert_eq!(
            c.compose(Some(&ops(8, false, vec![tile])), &mut f),
            Composed::Show
        );
        assert_eq!(
            (f.rgba[0], f.rgba[16 * 4]),
            (1, 2),
            "only the region is new"
        );
        // Picture 9 was lost.
        let mut f = frame(3);
        assert_eq!(
            c.compose(Some(&ops(10, false, vec![tile])), &mut f),
            Composed::Resync
        );
        assert!(c.waiting());
        assert_eq!(
            c.compose(Some(&ops(11, false, vec![tile])), &mut f),
            Composed::Resync
        );
        assert_eq!(
            c.compose(Some(&ops(12, true, vec![])), &mut frame(4)),
            Composed::Show
        );
        // Instructions missing while the host sends them, or not fitting the picture.
        assert_eq!(c.compose(None, &mut frame(5)), Composed::Resync);
        c.compose(Some(&ops(1, true, vec![])), &mut frame(5));
        let outside = Rect { x: 24, ..tile };
        assert_eq!(
            c.compose(Some(&ops(2, false, vec![outside])), &mut frame(5)),
            Composed::Resync
        );
        c.compose(Some(&ops(1, true, vec![])), &mut frame(5));
        c.lost();
        assert_eq!(
            c.compose(Some(&ops(2, false, vec![tile])), &mut frame(6)),
            Composed::Resync
        );
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let se: u64 = a
            .iter()
            .zip(b)
            .map(|(p, q)| u64::from(p.abs_diff(*q)).pow(2))
            .sum();
        let mse = se as f64 / a.len() as f64;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        }
    }

    /// Host to viewer through openh264: scrolling arrives as moves plus small pictures,
    /// the viewer's picture stays right, and a lost picture is noticed and repaired.
    #[cfg(not(target_os = "ios"))]
    #[test]
    fn scrolling_through_a_real_encoder_and_decoder() {
        use crate::video::{EncoderSettings, VideoDecoder, VideoEncoder};
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let settings = EncoderSettings {
            bitrate_bps: 4_000_000,
            ..Default::default()
        };
        let mut enc = VideoEncoder::new(&lib, settings).unwrap();
        let mut dec = VideoDecoder::software(&lib).unwrap();
        let mut composer = Composer::new();
        let mut compositor = Compositor::new();
        let doc = document(H + 600);
        let (mut sent, mut moved, mut resynced) = (0usize, 0.0f32, false);
        for i in 0..40usize {
            let pic = screen(&doc, (i * 13).min(600));
            if i == 25 {
                composer.request_full(); // as a keyframe request after the loss does
                enc.request_keyframe();
            }
            let (canvas, found) = composer.prepare(&pic, &[]);
            let Some(au) = enc.encode_yuv(canvas, found.rows).unwrap() else {
                composer.no_output(true);
                continue;
            };
            moved += found.moved;
            let au = composer.sent(&pic, au);
            sent += au.len();
            if i == 20 {
                continue; // lost on the way
            }
            let (bits, ops) = split(&au);
            let mut f = match dec.decode(&bits) {
                Ok(f) => f.expect("a picture per access unit"),
                Err(e) => {
                    // What the viewer does on a decode error.
                    assert!((21..25).contains(&i), "frame {i}: {e}");
                    compositor.lost();
                    resynced = true;
                    continue;
                }
            };
            let truth = rgba(&pic);
            match compositor.compose(ops.as_ref(), &mut f) {
                Composed::Show => {
                    assert!(!(21..25).contains(&i), "frame {i}: shown after a loss");
                    let q = psnr(&f.rgba, &truth.rgba);
                    assert!(q > 30.0, "frame {i}: {q:.1} dB");
                }
                Composed::Resync => {
                    assert!((21..25).contains(&i), "frame {i}: out of step");
                    resynced = true;
                }
            }
        }
        assert!(resynced, "the loss was noticed");
        assert!(moved / 40.0 > 0.5, "scrolls went as moves: {moved}");
        assert!(sent < 40 * 30_000, "{sent} bytes");
    }

    #[test]
    fn moves_overlapping_themselves_copy_like_memmove() {
        let (w, h) = (8, 8);
        let mut surface: Vec<u8> = (0..w * h * 4).map(|i| (i / 4) as u8).collect();
        let before = surface.clone();
        let decoded = vec![0u8; w * h * 4];
        let down = FrameOps {
            moves: vec![Move {
                from_x: 1,
                from_y: 1,
                to: Rect {
                    x: 2,
                    y: 3,
                    w: 5,
                    h: 5,
                },
            }],
            ..Default::default()
        };
        assert!(apply(&mut surface, &decoded, (w, h), &down));
        for y in 0..5 {
            for x in 0..5 {
                let px = |s: &[u8], x: usize, y: usize| s[(y * w + x) * 4];
                assert_eq!(px(&surface, x + 2, y + 3), px(&before, x + 1, y + 1));
            }
        }
    }
}
