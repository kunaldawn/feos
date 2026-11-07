//! Intra sample prediction (spec 8.3), written directly into the picture, using
//! the macroblock's own stride to reach its neighbours. That holds in MBAFF as
//! well, where left and upper neighbours are at the same physical lines.

use super::pixel::Pixel;

/// Which neighbours of a block may be used.
#[derive(Clone, Copy, Default)]
pub(crate) struct Avail {
    pub left: bool,
    pub top: bool,
    pub top_right: bool,
    pub top_left: bool,
}

#[inline(always)]
fn clip<P: Pixel>(value: i32, max: i32) -> P {
    P::new(value.clamp(0, max))
}

/// Intra 4x4 prediction of the block at `at`. Modes needing an unavailable
/// neighbour, which only broken streams ask for, read whatever is there, as
/// the block always has a picture around it or falls back to DC.
pub(crate) fn pred4x4<P: Pixel>(
    d: &mut [P],
    at: usize,
    s: usize,
    mode: u8,
    av: Avail,
    bit_depth: u32,
) {
    let mut mode = mode;
    if (matches!(mode, 0 | 3 | 7) && !av.top)
        || (matches!(mode, 1 | 8) && !av.left)
        || (matches!(mode, 4..=6) && !(av.top && av.left && av.top_left))
    {
        mode = 2;
    }
    let mut t = [0i32; 8];
    if av.top {
        for (i, v) in t.iter_mut().enumerate().take(4) {
            *v = d[at - s + i].get();
        }
        if av.top_right {
            for (i, v) in t.iter_mut().enumerate().skip(4) {
                *v = d[at - s + i].get();
            }
        } else {
            let last = t[3];
            t[4..8].fill(last);
        }
    }
    let mut l = [0i32; 4];
    if av.left {
        for (i, v) in l.iter_mut().enumerate() {
            *v = d[at + i * s - 1].get();
        }
    }
    let tl = if av.top_left && av.top && av.left {
        d[at - s - 1].get()
    } else {
        0
    };
    let mut p = [0i32; 16];
    match mode {
        0 => {
            for y in 0..4 {
                p[y * 4..y * 4 + 4].copy_from_slice(&t[..4]);
            }
        }
        1 => {
            for y in 0..4 {
                p[y * 4..y * 4 + 4].fill(l[y]);
            }
        }
        2 => {
            let top: i32 = t[..4].iter().sum();
            let left: i32 = l.iter().sum();
            let v = match (av.top, av.left) {
                (true, true) => (top + left + 4) >> 3,
                (true, false) => (top + 2) >> 2,
                (false, true) => (left + 2) >> 2,
                _ => 1 << (bit_depth - 1),
            };
            p.fill(v);
        }
        3 => {
            for y in 0..4 {
                for x in 0..4 {
                    p[y * 4 + x] = if x == 3 && y == 3 {
                        (t[6] + 3 * t[7] + 2) >> 2
                    } else {
                        (t[x + y] + 2 * t[x + y + 1] + t[x + y + 2] + 2) >> 2
                    };
                }
            }
        }
        4 | 5 | 6 => {
            // A line through the corner: e[0..=8] runs from the bottom of the left
            // column, through the corner, along the top row.
            let e = [l[3], l[2], l[1], l[0], tl, t[0], t[1], t[2], t[3]];
            let f3 = |i: usize| (e[i - 1] + 2 * e[i] + e[i + 1] + 2) >> 2;
            let f2 = |i: usize| (e[i] + e[i + 1] + 1) >> 1;
            for y in 0..4i32 {
                for x in 0..4i32 {
                    p[(y * 4 + x) as usize] = match mode {
                        4 => f3((4 + x - y) as usize),
                        5 => {
                            let z = 2 * x - y;
                            if z >= 0 {
                                if z & 1 == 0 {
                                    f2((4 + x - (y >> 1)) as usize)
                                } else {
                                    f3((4 + x - (y >> 1)) as usize)
                                }
                            } else if z == -1 {
                                f3(4)
                            } else {
                                f3((5 - y) as usize)
                            }
                        }
                        _ => {
                            let z = 2 * y - x;
                            if z >= 0 {
                                if z & 1 == 0 {
                                    f2((3 - y + (x >> 1)) as usize)
                                } else {
                                    f3((4 - y + (x >> 1)) as usize)
                                }
                            } else if z == -1 {
                                f3(4)
                            } else {
                                f3((4 + x - 1) as usize)
                            }
                        }
                    };
                }
            }
        }
        7 => {
            for y in 0..4 {
                for x in 0..4 {
                    let i = x + (y >> 1);
                    p[y * 4 + x] = if y & 1 == 0 {
                        (t[i] + t[i + 1] + 1) >> 1
                    } else {
                        (t[i] + 2 * t[i + 1] + t[i + 2] + 2) >> 2
                    };
                }
            }
        }
        _ => {
            for y in 0..4 {
                for x in 0..4 {
                    let z = x + 2 * y;
                    let i = y + (x >> 1);
                    p[y * 4 + x] = if z > 5 {
                        l[3]
                    } else if z == 5 {
                        (l[2] + 3 * l[3] + 2) >> 2
                    } else if z & 1 == 0 {
                        (l[i] + l[i + 1] + 1) >> 1
                    } else {
                        (l[i] + 2 * l[i + 1] + l[i + 2] + 2) >> 2
                    };
                }
            }
        }
    }
    for y in 0..4 {
        for x in 0..4 {
            d[at + y * s + x] = P::new(p[y * 4 + x]);
        }
    }
}

/// Intra 8x8 prediction with its reference sample filter (8.3.2.2).
pub(crate) fn pred8x8l<P: Pixel>(
    d: &mut [P],
    at: usize,
    s: usize,
    mode: u8,
    av: Avail,
    bit_depth: u32,
) {
    let mut mode = mode;
    if (matches!(mode, 0 | 3 | 7) && !av.top)
        || (matches!(mode, 1 | 8) && !av.left)
        || (matches!(mode, 4..=6) && !(av.top && av.left && av.top_left))
    {
        mode = 2;
    }
    // Raw neighbours.
    let mut rt = [0i32; 16];
    let mut rl = [0i32; 8];
    let corner = if av.top_left {
        d[at - s - 1].get()
    } else {
        0
    };
    if av.top {
        for (i, v) in rt.iter_mut().enumerate().take(8) {
            *v = d[at - s + i].get();
        }
        if av.top_right {
            for (i, v) in rt.iter_mut().enumerate().skip(8) {
                *v = d[at - s + i].get();
            }
        } else {
            let last = rt[7];
            rt[8..16].fill(last);
        }
    }
    if av.left {
        for (i, v) in rl.iter_mut().enumerate() {
            *v = d[at + i * s - 1].get();
        }
    }
    // Filtered neighbours.
    let mut t = [0i32; 16];
    let mut l = [0i32; 8];
    if av.top {
        t[0] = if av.top_left {
            (corner + 2 * rt[0] + rt[1] + 2) >> 2
        } else {
            (3 * rt[0] + rt[1] + 2) >> 2
        };
        for x in 1..15 {
            t[x] = (rt[x - 1] + 2 * rt[x] + rt[x + 1] + 2) >> 2;
        }
        t[15] = (rt[14] + 3 * rt[15] + 2) >> 2;
    }
    if av.left {
        l[0] = if av.top_left {
            (corner + 2 * rl[0] + rl[1] + 2) >> 2
        } else {
            (3 * rl[0] + rl[1] + 2) >> 2
        };
        for y in 1..7 {
            l[y] = (rl[y - 1] + 2 * rl[y] + rl[y + 1] + 2) >> 2;
        }
        l[7] = (rl[6] + 3 * rl[7] + 2) >> 2;
    }
    let tl = if av.top_left {
        match (av.top, av.left) {
            (true, true) => (rt[0] + 2 * corner + rl[0] + 2) >> 2,
            (true, false) => (3 * corner + rt[0] + 2) >> 2,
            (false, true) => (3 * corner + rl[0] + 2) >> 2,
            _ => corner,
        }
    } else {
        0
    };
    let mut p = [0i32; 64];
    match mode {
        0 => {
            for y in 0..8 {
                p[y * 8..y * 8 + 8].copy_from_slice(&t[..8]);
            }
        }
        1 => {
            for y in 0..8 {
                p[y * 8..y * 8 + 8].fill(l[y]);
            }
        }
        2 => {
            let top: i32 = t[..8].iter().sum();
            let left: i32 = l.iter().sum();
            let v = match (av.top, av.left) {
                (true, true) => (top + left + 8) >> 4,
                (true, false) => (top + 4) >> 3,
                (false, true) => (left + 4) >> 3,
                _ => 1 << (bit_depth - 1),
            };
            p.fill(v);
        }
        3 => {
            for y in 0..8 {
                for x in 0..8 {
                    p[y * 8 + x] = if x == 7 && y == 7 {
                        (t[14] + 3 * t[15] + 2) >> 2
                    } else {
                        (t[x + y] + 2 * t[x + y + 1] + t[x + y + 2] + 2) >> 2
                    };
                }
            }
        }
        4 | 5 | 6 => {
            // e[8] is the corner, e[9..] the top row, e[7] downwards the left column.
            let mut e = [0i32; 17];
            for i in 0..8 {
                e[7 - i] = l[i];
                e[9 + i] = t[i];
            }
            e[8] = tl;
            let f3 = |i: i32| (e[(i - 1) as usize] + 2 * e[i as usize] + e[(i + 1) as usize] + 2) >> 2;
            let f2 = |i: i32| (e[i as usize] + e[(i + 1) as usize] + 1) >> 1;
            for y in 0..8i32 {
                for x in 0..8i32 {
                    p[(y * 8 + x) as usize] = match mode {
                        4 => f3(8 + x - y),
                        5 => {
                            let z = 2 * x - y;
                            if z >= 0 {
                                if z & 1 == 0 {
                                    f2(8 + x - (y >> 1))
                                } else {
                                    f3(8 + x - (y >> 1))
                                }
                            } else if z == -1 {
                                f3(8)
                            } else {
                                // p'[-1, y-2x-1], p'[-1, y-2x-2], p'[-1, y-2x-3]
                                f3(8 - (y - 2 * x - 1))
                            }
                        }
                        _ => {
                            let z = 2 * y - x;
                            if z >= 0 {
                                if z & 1 == 0 {
                                    f2(7 - y + (x >> 1))
                                } else {
                                    f3(8 - y + (x >> 1))
                                }
                            } else if z == -1 {
                                f3(8)
                            } else {
                                // p'[x-2y-1, -1], p'[x-2y-2, -1], p'[x-2y-3, -1]
                                f3(8 + (x - 2 * y - 1))
                            }
                        }
                    };
                }
            }
        }
        7 => {
            for y in 0..8 {
                for x in 0..8 {
                    let i = x + (y >> 1);
                    p[y * 8 + x] = if y & 1 == 0 {
                        (t[i] + t[i + 1] + 1) >> 1
                    } else {
                        (t[i] + 2 * t[i + 1] + t[i + 2] + 2) >> 2
                    };
                }
            }
        }
        _ => {
            for y in 0..8 {
                for x in 0..8 {
                    let z = x + 2 * y;
                    let i = y + (x >> 1);
                    p[y * 8 + x] = if z > 13 {
                        l[7]
                    } else if z == 13 {
                        (l[6] + 3 * l[7] + 2) >> 2
                    } else if z & 1 == 0 {
                        (l[i] + l[i + 1] + 1) >> 1
                    } else {
                        (l[i] + 2 * l[i + 1] + l[i + 2] + 2) >> 2
                    };
                }
            }
        }
    }
    for y in 0..8 {
        for x in 0..8 {
            d[at + y * s + x] = P::new(p[y * 8 + x]);
        }
    }
}

/// Intra 16x16 prediction.
pub(crate) fn pred16x16<P: Pixel>(
    d: &mut [P],
    at: usize,
    s: usize,
    mode: u8,
    left: bool,
    top: bool,
    top_left: bool,
    bit_depth: u32,
) {
    let max = (1 << bit_depth) - 1;
    let mode = match mode {
        0 if !top => 2,
        1 if !left => 2,
        3 if !(top && left && top_left) => 2,
        m => m,
    };
    match mode {
        0 => {
            for y in 0..16 {
                d.copy_within(at - s..at - s + 16, at + y * s);
            }
        }
        1 => {
            for y in 0..16 {
                let v = d[at + y * s - 1];
                d[at + y * s..at + y * s + 16].fill(v);
            }
        }
        2 => {
            let mut sum = 0;
            if top {
                sum += d[at - s..at - s + 16].iter().map(|v| v.get()).sum::<i32>();
            }
            if left {
                sum += (0..16).map(|y| d[at + y * s - 1].get()).sum::<i32>();
            }
            let v = match (top, left) {
                (true, true) => (sum + 16) >> 5,
                (false, false) => 1 << (bit_depth - 1),
                _ => (sum + 8) >> 4,
            };
            for y in 0..16 {
                d[at + y * s..at + y * s + 16].fill(P::new(v));
            }
        }
        _ => {
            let top_at = |x: i32| d[(at - s).wrapping_add_signed(x as isize)].get();
            let left_at = |y: i32| d[(at + (y + 1) as usize * s) - s - 1].get();
            let mut h = 0;
            let mut v = 0;
            for i in 0..8 {
                h += (i + 1) * (top_at(8 + i) - top_at(6 - i));
                v += (i + 1) * (left_at(8 + i) - left_at(6 - i));
            }
            let a = 16 * (left_at(15) + top_at(15));
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            for y in 0..16i32 {
                for x in 0..16i32 {
                    d[at + y as usize * s + x as usize] =
                        clip((a + b * (x - 7) + c * (y - 7) + 16) >> 5, max);
                }
            }
        }
    }
}

/// Chroma prediction of one plane, 8 wide and `height` (8 or 16) high.
/// `left` gives the left neighbours' availability per block row.
pub(crate) fn pred_chroma<P: Pixel>(
    d: &mut [P],
    at: usize,
    s: usize,
    mode: u8,
    left: [bool; 4],
    top: bool,
    top_left: bool,
    height: usize,
    bit_depth: u32,
) {
    let max = (1 << bit_depth) - 1;
    let rows = height / 4;
    let all_left = left[..rows].iter().all(|&l| l);
    let mode = match mode {
        1 if !all_left => 0,
        2 if !top => 0,
        3 if !(top && all_left && top_left) => 0,
        m => m,
    };
    match mode {
        0 => {
            for by in 0..rows {
                for bx in 0..2 {
                    let sum_top = |d: &[P]| -> i32 {
                        (0..4).map(|x| d[at - s + bx * 4 + x].get()).sum()
                    };
                    let sum_left = |d: &[P]| -> i32 {
                        (0..4).map(|y| d[at + (by * 4 + y) * s - 1].get()).sum()
                    };
                    let (t, l) = (top, left[by]);
                    let v = if (bx == 0 && by == 0) || (bx > 0 && by > 0) {
                        match (t, l) {
                            (true, true) => (sum_top(d) + sum_left(d) + 4) >> 3,
                            (true, false) => (sum_top(d) + 2) >> 2,
                            (false, true) => (sum_left(d) + 2) >> 2,
                            _ => 1 << (bit_depth - 1),
                        }
                    } else if bx > 0 {
                        if t {
                            (sum_top(d) + 2) >> 2
                        } else if l {
                            (sum_left(d) + 2) >> 2
                        } else {
                            1 << (bit_depth - 1)
                        }
                    } else if l {
                        (sum_left(d) + 2) >> 2
                    } else if t {
                        (sum_top(d) + 2) >> 2
                    } else {
                        1 << (bit_depth - 1)
                    };
                    for y in 0..4 {
                        let row = at + (by * 4 + y) * s + bx * 4;
                        d[row..row + 4].fill(P::new(v));
                    }
                }
            }
        }
        1 => {
            for y in 0..height {
                let v = d[at + y * s - 1];
                d[at + y * s..at + y * s + 8].fill(v);
            }
        }
        2 => {
            for y in 0..height {
                d.copy_within(at - s..at - s + 8, at + y * s);
            }
        }
        _ => {
            let yc = if height == 16 { 4 } else { 0i32 };
            let top_at = |x: i32| d[(at - s).wrapping_add_signed(x as isize)].get();
            let left_at = |y: i32| d[(at + (y + 1) as usize * s) - s - 1].get();
            let mut h = 0;
            let mut v = 0;
            for i in 0..4 {
                h += (i + 1) * (top_at(4 + i) - top_at(2 - i));
            }
            for i in 0..4 + yc {
                v += (i + 1) * (left_at(4 + yc + i) - left_at(2 + yc - i));
            }
            let a = 16 * (left_at(height as i32 - 1) + top_at(7));
            let b = (34 * h + 32) >> 6;
            let c = ((if height == 16 { 5 } else { 34 }) * v + 32) >> 6;
            for y in 0..height as i32 {
                for x in 0..8i32 {
                    d[at + y as usize * s + x as usize] =
                        clip((a + b * (x - 3) + c * (y - 3 - yc) + 16) >> 5, max);
                }
            }
        }
    }
}
