//! The loop filter: RFC 6386 section 15.

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub edge: i32,
    pub interior: i32,
    pub hev: i32,
}

#[inline]
fn s8(x: i32) -> i32 {
    x.clamp(-128, 127)
}

#[inline]
fn u8c(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

#[inline]
fn simple_ok(p: &[u8], at: usize, step: usize, limit: i32) -> bool {
    let (p1, p0, q0, q1) = (
        p[at - 2 * step] as i32,
        p[at - step] as i32,
        p[at] as i32,
        p[at + step] as i32,
    );
    (p0 - q0).abs() * 2 + ((p1 - q1).abs() >> 1) <= limit
}

#[inline]
fn normal_ok(p: &[u8], at: usize, step: usize, edge: i32, interior: i32) -> bool {
    let at3 = at - 4 * step;
    let [p3, p2, p1, p0, q0, q1, q2, q3] = core::array::from_fn(|i| p[at3 + i * step] as i32);
    (p0 - q0).abs() * 2 + ((p1 - q1).abs() >> 1) <= 2 * edge + interior
        && (p3 - p2).abs() <= interior
        && (p2 - p1).abs() <= interior
        && (p1 - p0).abs() <= interior
        && (q3 - q2).abs() <= interior
        && (q2 - q1).abs() <= interior
        && (q1 - q0).abs() <= interior
}

#[inline]
fn high_variance(p: &[u8], at: usize, step: usize, threshold: i32) -> bool {
    (p[at - 2 * step] as i32 - p[at - step] as i32).abs() > threshold
        || (p[at + step] as i32 - p[at] as i32).abs() > threshold
}

/// Moves the two pixels by the edge, and if `outer` isn't used, the next two as well.
#[inline]
fn common(p: &mut [u8], at: usize, step: usize, outer: bool) {
    let (p1, p0, q0, q1) = (
        p[at - 2 * step] as i32,
        p[at - step] as i32,
        p[at] as i32,
        p[at + step] as i32,
    );
    let mut a = 3 * (q0 - p0);
    if outer {
        a += s8(p1 - q1);
    }
    let a = s8(a);
    let f1 = (a + 4).min(127) >> 3;
    let f2 = (a + 3).min(127) >> 3;
    p[at - step] = u8c(p0 + f2);
    p[at] = u8c(q0 - f1);
    if !outer {
        let a = (f1 + 1) >> 1;
        p[at - 2 * step] = u8c(p1 + a);
        p[at + step] = u8c(q1 - a);
    }
}

#[inline]
fn mb_edge(p: &mut [u8], at: usize, step: usize) {
    let (p2, p1, p0, q0, q1, q2) = (
        p[at - 3 * step] as i32,
        p[at - 2 * step] as i32,
        p[at - step] as i32,
        p[at] as i32,
        p[at + step] as i32,
        p[at + 2 * step] as i32,
    );
    let w = s8(s8(p1 - q1) + 3 * (q0 - p0));
    let a = (27 * w + 63) >> 7;
    p[at - step] = u8c(p0 + a);
    p[at] = u8c(q0 - a);
    let a = (18 * w + 63) >> 7;
    p[at - 2 * step] = u8c(p1 + a);
    p[at + step] = u8c(q1 - a);
    let a = (9 * w + 63) >> 7;
    p[at - 3 * step] = u8c(p2 + a);
    p[at + 2 * step] = u8c(q2 - a);
}

/// A macroblock's left edge: `size` rows from `at`.
pub(super) fn mb_v(p: &mut [u8], at: usize, stride: usize, size: usize, limits: Limits) {
    for i in 0..size {
        let at = at + i * stride;
        if normal_ok(p, at, 1, limits.edge + 2, limits.interior) {
            if high_variance(p, at, 1, limits.hev) {
                common(p, at, 1, true);
            } else {
                mb_edge(p, at, 1);
            }
        }
    }
}

/// A macroblock's top edge.
pub(super) fn mb_h(p: &mut [u8], at: usize, stride: usize, size: usize, limits: Limits) {
    for i in 0..size {
        let at = at + i;
        if normal_ok(p, at, stride, limits.edge + 2, limits.interior) {
            if high_variance(p, at, stride, limits.hev) {
                common(p, at, stride, true);
            } else {
                mb_edge(p, at, stride);
            }
        }
    }
}

/// An inner vertical edge.
pub(super) fn sub_v(p: &mut [u8], at: usize, stride: usize, size: usize, limits: Limits) {
    for i in 0..size {
        let at = at + i * stride;
        if normal_ok(p, at, 1, limits.edge, limits.interior) {
            let outer = high_variance(p, at, 1, limits.hev);
            common(p, at, 1, outer);
        }
    }
}

/// An inner horizontal edge.
pub(super) fn sub_h(p: &mut [u8], at: usize, stride: usize, size: usize, limits: Limits) {
    for i in 0..size {
        let at = at + i;
        if normal_ok(p, at, stride, limits.edge, limits.interior) {
            let outer = high_variance(p, at, stride, limits.hev);
            common(p, at, stride, outer);
        }
    }
}

/// The simple filter's vertical edges, luma only.
pub(super) fn simple_v(p: &mut [u8], at: usize, stride: usize, limit: i32) {
    for i in 0..16 {
        let at = at + i * stride;
        if simple_ok(p, at, 1, limit) {
            common(p, at, 1, true);
        }
    }
}

/// The simple filter's horizontal edges, luma only.
pub(super) fn simple_h(p: &mut [u8], at: usize, stride: usize, limit: i32) {
    for i in 0..16 {
        let at = at + i;
        if simple_ok(p, at, stride, limit) {
            common(p, at, stride, true);
        }
    }
}
