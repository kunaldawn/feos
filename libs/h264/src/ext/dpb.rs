//! Reference picture management with field-level marking: POC (spec 8.2.1), list
//! construction and modification (8.2.4) and marking (8.2.5). A store is a frame's
//! worth of samples, whose fields can be marked apart.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::nal::NalUnitType;
use crate::slice::SliceHeader;
use crate::sps::Sps;

use super::pixel::Pixel;

/// Field bits of the marks: a frame is `TOP | BOTTOM`.
pub(crate) const TOP: u8 = 1;
pub(crate) const BOTTOM: u8 = 2;
pub(crate) const FRAME: u8 = 3;

/// Motion of one macroblock, kept with its picture for direct prediction and
/// deblocking. Blocks are in raster order, and macroblocks at their position in the
/// frame: rows alternate between top and bottom fields' macroblocks when coded in
/// fields, and between upper and lower macroblocks of MBAFF pairs.
#[derive(Clone, Copy)]
pub(crate) struct MbMotion {
    pub mv: [[[i16; 2]; 16]; 2],
    pub ref_idx: [[i8; 4]; 2],
    /// Which picture each reference index means: `uid * 4 + parity`, with parity 0 for
    /// a frame, or -1.
    pub ref_key: [[i32; 4]; 2],
    pub intra: bool,
    pub field: bool,
}

impl Default for MbMotion {
    fn default() -> Self {
        Self {
            mv: [[[0; 2]; 16]; 2],
            ref_idx: [[-1; 4]; 2],
            ref_key: [[-1; 4]; 2],
            intra: true,
            field: false,
        }
    }
}

/// The finished samples and motion of a frame.
pub(crate) struct PicData<P> {
    pub planes: [Arc<Vec<P>>; 3],
    pub motion: Vec<MbMotion>,
}

/// One frame in the buffer.
pub(crate) struct Store<P> {
    /// `None` while it's being decoded, and for a frame that frame_num's gaps imply.
    pub data: Option<Arc<PicData<P>>>,
    pub uid: i32,
    pub frame_num: u32,
    pub frame_num_wrap: i32,
    pub lt_idx: u32,
    pub short: u8,
    pub long: u8,
    pub poc: [i32; 2],
    /// Fields decoded so far.
    pub present: u8,
    /// Coded as two field pictures.
    pub coded_fields: bool,
    pub mbaff: bool,
    pub non_existing: bool,
    pub current: bool,
}

impl<P> Store<P> {
    /// PicOrderCnt of the frame, or of what's there of it.
    pub fn frame_poc(&self) -> i32 {
        match self.present {
            TOP => self.poc[0],
            BOTTOM => self.poc[1],
            _ => self.poc[0].min(self.poc[1]),
        }
    }

    /// PicOrderCnt for sorting when listing fields: only marked fields count.
    fn marked_poc(&self, marks: u8) -> i32 {
        match marks & self.present {
            TOP => self.poc[0],
            BOTTOM => self.poc[1],
            _ => self.poc[0].min(self.poc[1]),
        }
    }

    fn poc_of(&self, parity: u8) -> i32 {
        match parity {
            TOP => self.poc[0],
            BOTTOM => self.poc[1],
            _ => self.frame_poc(),
        }
    }
}

/// An entry of a reference list.
pub(crate) struct RefEntry<P> {
    /// `None` means the picture being decoded: the other field of its frame.
    pub data: Option<Arc<PicData<P>>>,
    /// `FRAME`, `TOP` or `BOTTOM`.
    pub parity: u8,
    pub uid: i32,
    pub poc: [i32; 2],
    pub long: bool,
    pub coded_fields: bool,
    pub mbaff: bool,
    /// Picks the POC of the whole picture or of this field.
    pub own_poc: i32,
}

impl<P> Clone for RefEntry<P> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            parity: self.parity,
            uid: self.uid,
            poc: self.poc,
            long: self.long,
            coded_fields: self.coded_fields,
            mbaff: self.mbaff,
            own_poc: self.own_poc,
        }
    }
}

/// Reference marking and POC state.
pub(crate) struct Dpb<P> {
    pub stores: Vec<Store<P>>,
    next_uid: i32,
    max_refs: usize,
    prev_ref_msb: i32,
    prev_ref_lsb: i32,
    prev_frame_num: u32,
    prev_frame_num_offset: i32,
    pub prev_ref_frame_num: u32,
}

/// POCs of a picture, and what's needed to finish it.
#[derive(Clone, Copy, Default)]
pub(crate) struct PocResult {
    pub top: i32,
    pub bottom: i32,
    pub msb: i32,
    pub offset: i32,
}

impl<P: Pixel> Dpb<P> {
    pub fn new() -> Self {
        Self {
            stores: Vec::new(),
            next_uid: 1,
            max_refs: 1,
            prev_ref_msb: 0,
            prev_ref_lsb: 0,
            prev_frame_num: 0,
            prev_frame_num_offset: 0,
            prev_ref_frame_num: 0,
        }
    }

    pub fn reset(&mut self) {
        self.stores.clear();
        self.prev_ref_msb = 0;
        self.prev_ref_lsb = 0;
        self.prev_frame_num = 0;
        self.prev_frame_num_offset = 0;
        self.prev_ref_frame_num = 0;
    }

    pub fn set_max_refs(&mut self, refs: u32) {
        self.max_refs = (refs as usize).clamp(1, 16);
    }

    pub fn new_uid(&mut self) -> i32 {
        let uid = self.next_uid;
        // Keys multiply by 4, and old ones are long gone when this wraps.
        self.next_uid = if uid >= 0x1fff_ffff { 1 } else { uid + 1 };
        uid
    }

    /// POC of a new picture (8.2.1). `structure` is `FRAME`, `TOP` or `BOTTOM`.
    pub fn compute_poc(
        &self,
        sps: &Sps,
        h: &SliceHeader,
        nal_type: NalUnitType,
        ref_idc: u8,
        structure: u8,
    ) -> PocResult {
        let idr = nal_type == NalUnitType::SliceIdr;
        let max_frame_num = 1i32 << (sps.log2_max_frame_num_minus4 + 4).min(30);
        let offset = if idr {
            0
        } else if self.prev_frame_num > h.frame_num {
            self.prev_frame_num_offset.wrapping_add(max_frame_num)
        } else {
            self.prev_frame_num_offset
        };
        let mut out = PocResult { offset, ..PocResult::default() };
        match sps.pic_order_cnt_type {
            0 => {
                let (prev_msb, prev_lsb) = if idr {
                    (0, 0)
                } else {
                    (self.prev_ref_msb, self.prev_ref_lsb)
                };
                let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4).min(30);
                let lsb = h.pic_order_cnt_lsb as i32;
                out.msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb.wrapping_add(max_lsb)
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb.wrapping_sub(max_lsb)
                } else {
                    prev_msb
                };
                out.top = out.msb.wrapping_add(lsb);
                out.bottom = if structure == FRAME {
                    out.top.wrapping_add(h.delta_pic_order_cnt_bottom)
                } else {
                    out.top
                };
            }
            1 => {
                let cycle_len = sps.num_ref_frames_in_pic_order_cnt_cycle as i32;
                let mut abs = if cycle_len != 0 {
                    out.offset.wrapping_add(h.frame_num as i32)
                } else {
                    0
                };
                if ref_idc == 0 && abs > 0 {
                    abs -= 1;
                }
                let per_cycle = sps
                    .offset_for_ref_frame
                    .iter()
                    .fold(0i32, |a, &b| a.wrapping_add(b));
                let mut expected = 0i32;
                if abs > 0 {
                    let cycles = (abs - 1) / cycle_len.max(1);
                    let in_cycle = ((abs - 1) % cycle_len.max(1)) as usize;
                    expected = cycles.wrapping_mul(per_cycle);
                    for &o in sps.offset_for_ref_frame.iter().take(in_cycle + 1) {
                        expected = expected.wrapping_add(o);
                    }
                }
                if ref_idc == 0 {
                    expected = expected.wrapping_add(sps.offset_for_non_ref_pic);
                }
                if structure == FRAME {
                    out.top = expected.wrapping_add(h.delta_pic_order_cnt[0]);
                    out.bottom = out
                        .top
                        .wrapping_add(sps.offset_for_top_to_bottom_field)
                        .wrapping_add(h.delta_pic_order_cnt[1]);
                } else if structure == TOP {
                    out.top = expected.wrapping_add(h.delta_pic_order_cnt[0]);
                    out.bottom = out.top;
                } else {
                    out.top = expected
                        .wrapping_add(sps.offset_for_top_to_bottom_field)
                        .wrapping_add(h.delta_pic_order_cnt[0]);
                    out.bottom = out.top;
                }
            }
            _ => {
                let temp = if idr {
                    0
                } else {
                    let doubled = out
                        .offset
                        .wrapping_add(h.frame_num as i32)
                        .wrapping_mul(2);
                    if ref_idc == 0 {
                        doubled.wrapping_sub(1)
                    } else {
                        doubled
                    }
                };
                out.top = temp;
                out.bottom = temp;
            }
        }
        out
    }

    /// Records POC state once a picture is finished. `mmco5` is its POC after that
    /// operation's reset for top-field purposes.
    pub fn finish_poc(
        &mut self,
        res: &PocResult,
        frame_num: u32,
        is_ref: bool,
        lsb: u32,
        mmco5: Option<i32>,
        structure: u8,
    ) {
        if let Some(temp_top) = mmco5 {
            self.prev_ref_msb = 0;
            self.prev_ref_lsb = if structure == BOTTOM { 0 } else { temp_top };
            self.prev_frame_num = 0;
            self.prev_frame_num_offset = 0;
            self.prev_ref_frame_num = 0;
            return;
        }
        if is_ref {
            self.prev_ref_msb = res.msb;
            self.prev_ref_lsb = lsb as i32;
            self.prev_ref_frame_num = frame_num;
        }
        self.prev_frame_num = frame_num;
        self.prev_frame_num_offset = res.offset;
    }

    fn update_wraps(&mut self, frame_num: u32, max_frame_num: u32) {
        for store in &mut self.stores {
            store.frame_num_wrap = if store.frame_num > frame_num {
                store.frame_num as i32 - max_frame_num as i32
            } else {
                store.frame_num as i32
            };
        }
    }

    /// Frames implied by a jump in frame_num (8.2.5.2). They hold reference slots but
    /// have no samples.
    pub fn fill_gaps(&mut self, sps: &Sps, frame_num: u32) {
        let max = 1u32 << (sps.log2_max_frame_num_minus4 + 4).min(30);
        let mut unused = (self.prev_ref_frame_num + 1) % max;
        let total = frame_num.wrapping_sub(unused) & (max - 1);
        // Only the last few could still be in the window, so a long jump can skip.
        if total > 32 {
            unused = frame_num.wrapping_sub(32) & (max - 1);
        }
        while unused != frame_num {
            self.update_wraps(unused, max);
            self.sliding_window();
            let uid = self.new_uid();
            let wrap = unused as i32;
            let mut poc = [0i32; 2];
            if sps.pic_order_cnt_type != 0 {
                let offset = if self.prev_frame_num > unused {
                    self.prev_frame_num_offset.wrapping_add(max as i32)
                } else {
                    self.prev_frame_num_offset
                };
                let doubled = offset.wrapping_add(unused as i32).wrapping_mul(2);
                poc = [doubled, doubled];
                self.prev_frame_num_offset = offset;
            }
            self.prev_frame_num = unused;
            self.prev_ref_frame_num = unused;
            self.stores.push(Store {
                data: None,
                uid,
                frame_num: unused,
                frame_num_wrap: wrap,
                lt_idx: 0,
                short: FRAME,
                long: 0,
                poc,
                present: FRAME,
                coded_fields: false,
                mbaff: false,
                non_existing: true,
                current: false,
            });
            unused = (unused + 1) % max;
        }
    }

    /// Builds the initial lists (8.2.4.2) as (store, parity).
    fn initial_lists(
        &self,
        is_b: bool,
        structure: u8,
        cur_poc: i32,
        poc_type0: bool,
    ) -> [Vec<(usize, u8)>; 2] {
        let mut out = [Vec::new(), Vec::new()];
        let by_key = |mut v: Vec<(usize, i64)>, descending: bool| -> Vec<usize> {
            v.sort_by_key(|&(_, k)| if descending { -k } else { k });
            v.into_iter().map(|(i, _)| i).collect()
        };
        if structure == FRAME {
            let usable = |s: &Store<P>| !s.current;
            let longs = by_key(
                self.stores
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| usable(s) && s.long == FRAME)
                    .map(|(i, s)| (i, s.lt_idx as i64))
                    .collect(),
                false,
            );
            if !is_b {
                let shorts = by_key(
                    self.stores
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| usable(s) && s.short == FRAME)
                        .map(|(i, s)| (i, s.frame_num_wrap as i64))
                        .collect(),
                    true,
                );
                out[0] = shorts.iter().chain(&longs).map(|&i| (i, FRAME)).collect();
            } else {
                let candidates = || {
                    self.stores.iter().enumerate().filter(|(_, s)| {
                        usable(s) && s.short == FRAME && !(s.non_existing && poc_type0)
                    })
                };
                let before = by_key(
                    candidates()
                        .filter(|(_, s)| s.frame_poc() < cur_poc)
                        .map(|(i, s)| (i, s.frame_poc() as i64))
                        .collect(),
                    true,
                );
                let after = by_key(
                    candidates()
                        .filter(|(_, s)| s.frame_poc() >= cur_poc)
                        .map(|(i, s)| (i, s.frame_poc() as i64))
                        .collect(),
                    false,
                );
                out[0] = before
                    .iter()
                    .chain(&after)
                    .chain(&longs)
                    .map(|&i| (i, FRAME))
                    .collect();
                out[1] = after
                    .iter()
                    .chain(&before)
                    .chain(&longs)
                    .map(|&i| (i, FRAME))
                    .collect();
            }
        } else {
            let longs = by_key(
                self.stores
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.long != 0)
                    .map(|(i, s)| (i, s.lt_idx as i64))
                    .collect(),
                false,
            );
            let long_fields = self.alternate(&longs, structure, false);
            if !is_b {
                let shorts = by_key(
                    self.stores
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.short != 0)
                        .map(|(i, s)| (i, s.frame_num_wrap as i64))
                        .collect(),
                    true,
                );
                out[0] = self.alternate(&shorts, structure, true);
                out[0].extend_from_slice(&long_fields);
            } else {
                let candidates = || {
                    self.stores
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.short != 0 && !(s.non_existing && poc_type0))
                };
                let before = by_key(
                    candidates()
                        .filter(|(_, s)| s.marked_poc(s.short) <= cur_poc)
                        .map(|(i, s)| (i, s.marked_poc(s.short) as i64))
                        .collect(),
                    true,
                );
                let after = by_key(
                    candidates()
                        .filter(|(_, s)| s.marked_poc(s.short) > cur_poc)
                        .map(|(i, s)| (i, s.marked_poc(s.short) as i64))
                        .collect(),
                    false,
                );
                let l0: Vec<usize> = before.iter().chain(&after).copied().collect();
                let l1: Vec<usize> = after.iter().chain(&before).copied().collect();
                out[0] = self.alternate(&l0, structure, true);
                out[1] = self.alternate(&l1, structure, true);
                out[0].extend_from_slice(&long_fields);
                out[1].extend_from_slice(&long_fields);
            }
        }
        if is_b && out[1].len() > 1 && out[0] == out[1] {
            out[1].swap(0, 1);
        }
        out
    }

    /// 8.2.4.2.5: fields from an ordered list of frames, alternating parity from the
    /// current field's.
    fn alternate(&self, frames: &[usize], structure: u8, short: bool) -> Vec<(usize, u8)> {
        let marked = |i: usize, parity: u8| {
            let s = &self.stores[i];
            (if short { s.short } else { s.long }) & parity != 0
        };
        let same = structure;
        let other = FRAME ^ structure;
        let (mut a, mut b) = (0, 0);
        let mut out = Vec::new();
        loop {
            while a < frames.len() && !marked(frames[a], same) {
                a += 1;
            }
            if a < frames.len() {
                out.push((frames[a], same));
                a += 1;
            } else {
                break;
            }
            while b < frames.len() && !marked(frames[b], other) {
                b += 1;
            }
            if b < frames.len() {
                out.push((frames[b], other));
                b += 1;
            } else {
                break;
            }
        }
        // One parity ran out: the rest of the other follow in order.
        while a < frames.len() {
            if marked(frames[a], same) {
                out.push((frames[a], same));
            }
            a += 1;
        }
        while b < frames.len() {
            if marked(frames[b], other) {
                out.push((frames[b], other));
            }
            b += 1;
        }
        out
    }

    /// Finds the store and parity with a short-term PicNum.
    fn find_short(&self, pic_num: i32, structure: u8) -> Option<(usize, u8)> {
        if structure == FRAME {
            self.stores
                .iter()
                .position(|s| !s.current && s.short == FRAME && s.frame_num_wrap == pic_num)
                .map(|i| (i, FRAME))
        } else {
            let parity = if pic_num & 1 != 0 {
                structure
            } else {
                FRAME ^ structure
            };
            let wrap = pic_num >> 1;
            self.stores
                .iter()
                .position(|s| s.short & parity != 0 && s.frame_num_wrap == wrap)
                .map(|i| (i, parity))
        }
    }

    fn find_long(&self, num: i32, structure: u8) -> Option<(usize, u8)> {
        if structure == FRAME {
            self.stores
                .iter()
                .position(|s| !s.current && s.long == FRAME && s.lt_idx as i32 == num)
                .map(|i| (i, FRAME))
        } else {
            let parity = if num & 1 != 0 {
                structure
            } else {
                FRAME ^ structure
            };
            let idx = num >> 1;
            self.stores
                .iter()
                .position(|s| s.long & parity != 0 && s.lt_idx as i32 == idx)
                .map(|i| (i, parity))
        }
    }

    /// The final lists for a slice (8.2.4). Entries that name nothing are filled
    /// from whatever else the list holds, so a damaged stream can still be decoded.
    #[allow(clippy::too_many_arguments)]
    pub fn build_lists(
        &mut self,
        h: &SliceHeader,
        sps: &Sps,
        is_b: bool,
        structure: u8,
        cur_poc: i32,
    ) -> [Vec<RefEntry<P>>; 2] {
        let max_frame_num = 1u32 << (sps.log2_max_frame_num_minus4 + 4).min(30);
        self.update_wraps(h.frame_num, max_frame_num);
        let init = self.initial_lists(is_b, structure, cur_poc, sps.pic_order_cnt_type == 0);
        let (cur_pic_num, max_pic_num) = if structure == FRAME {
            (h.frame_num as i32, max_frame_num as i32)
        } else {
            (h.frame_num as i32 * 2 + 1, max_frame_num as i32 * 2)
        };
        let mut out = [Vec::new(), Vec::new()];
        let [init0, init1] = init;
        for (list, mut init) in [init0, init1].into_iter().enumerate() {
            let (active, ops) = if list == 0 {
                (h.num_ref_idx_l0_active as usize, &h.ref_list_mod_l0)
            } else {
                (h.num_ref_idx_l1_active as usize, &h.ref_list_mod_l1)
            };
            init.truncate(active);
            let mut entries: Vec<Option<(usize, u8)>> = init.into_iter().map(Some).collect();
            entries.resize(active, None);
            let mut pred = cur_pic_num;
            let mut idx = 0;
            for &(idc, val) in ops.iter() {
                if idx >= active {
                    break;
                }
                let target = if idc < 2 {
                    let abs = val as i32 + 1;
                    if idc == 0 {
                        pred -= abs;
                        if pred < 0 {
                            pred += max_pic_num;
                        }
                    } else {
                        pred += abs;
                        if pred >= max_pic_num {
                            pred -= max_pic_num;
                        }
                    }
                    let pic_num = if pred > cur_pic_num {
                        pred - max_pic_num
                    } else {
                        pred
                    };
                    self.find_short(pic_num, structure)
                } else {
                    self.find_long(val as i32, structure)
                };
                if let Some(target) = target {
                    entries.insert(idx, Some(target));
                    let mut seen = idx + 1;
                    for i in idx + 1..entries.len() {
                        if entries[i] != Some(target) {
                            entries[seen] = entries[i];
                            seen += 1;
                        }
                    }
                    entries.truncate(active);
                    idx += 1;
                }
            }
            // A slot naming nothing borrows the first picture with samples.
            let fallback = entries.iter().flatten().copied().find(|&(i, _)| {
                self.stores[i].data.is_some() || self.stores[i].current
            });
            let fallback = fallback.or_else(|| {
                self.stores
                    .iter()
                    .rposition(|s| s.data.is_some() && s.present == FRAME)
                    .map(|i| {
                        (
                            i,
                            if structure == FRAME { FRAME } else { structure },
                        )
                    })
            });
            let mut built = Vec::with_capacity(active);
            for entry in &entries {
                let chosen = entry.filter(|&(i, _)| {
                    self.stores[i].data.is_some() || self.stores[i].current
                });
                let Some((i, parity)) = chosen.or(fallback) else {
                    continue;
                };
                let s = &self.stores[i];
                built.push(RefEntry {
                    data: s.data.clone(),
                    parity,
                    uid: entry.map_or(s.uid, |(e, _)| self.stores[e].uid),
                    poc: s.poc,
                    long: if parity == FRAME {
                        s.long == FRAME
                    } else {
                        s.long & parity != 0
                    },
                    coded_fields: s.coded_fields,
                    mbaff: s.mbaff,
                    own_poc: s.poc_of(parity),
                });
            }
            out[list] = built;
        }
        out
    }

    /// 8.2.5.3: make room for one more reference frame.
    fn sliding_window(&mut self) {
        loop {
            let used = self
                .stores
                .iter()
                .filter(|s| !s.current && s.short | s.long != 0)
                .count();
            if used < self.max_refs {
                return;
            }
            let oldest = self
                .stores
                .iter_mut()
                .filter(|s| !s.current && s.short != 0)
                .min_by_key(|s| s.frame_num_wrap);
            match oldest {
                Some(store) => store.short = 0,
                None => return,
            }
        }
    }

    /// Reference marking of a finished picture. `cur` is the current store's position.
    /// Returns true if MMCO 5 was in effect.
    pub fn mark(
        &mut self,
        cur: usize,
        sps: &Sps,
        frame_num: u32,
        structure: u8,
        idr: bool,
        long_term_flag: bool,
        mmco: &[(u32, u32)],
    ) -> bool {
        let max_frame_num = 1u32 << (sps.log2_max_frame_num_minus4 + 4).min(30);
        self.update_wraps(frame_num, max_frame_num);
        let mut had5 = false;
        if idr {
            if long_term_flag {
                self.stores[cur].long |= structure;
                self.stores[cur].lt_idx = 0;
            } else {
                self.stores[cur].short |= structure;
            }
            return false;
        }
        let cur_pic_num = if structure == FRAME {
            frame_num as i32
        } else {
            2 * frame_num as i32 + 1
        };
        let mut made_long = false;
        if mmco.is_empty() {
            // The second field of a reference frame needs no new slot.
            if self.stores[cur].short == 0 {
                self.sliding_window();
            }
        } else {
            for &(op, param) in mmco {
                match op {
                    1 => {
                        let pic_num = cur_pic_num - (param as i32 & 0xffff) - 1;
                        if let Some((i, parity)) = self.find_short(pic_num, structure) {
                            self.stores[i].short &= !parity;
                        }
                    }
                    2 => {
                        if let Some((i, parity)) = self.find_long(param as i32, structure) {
                            self.stores[i].long &= !parity;
                        }
                    }
                    3 => {
                        let pic_num = cur_pic_num - (param as i32 & 0xffff) - 1;
                        let idx = param >> 16;
                        if let Some((i, parity)) = self.find_short(pic_num, structure) {
                            for (j, s) in self.stores.iter_mut().enumerate() {
                                if j != i && s.long != 0 && s.lt_idx == idx {
                                    s.long = 0;
                                }
                            }
                            let s = &mut self.stores[i];
                            if s.long != 0 && s.lt_idx != idx {
                                s.long = 0;
                            }
                            s.short &= !parity;
                            s.long |= parity;
                            s.lt_idx = idx;
                        }
                    }
                    4 => {
                        for s in &mut self.stores {
                            if s.long != 0 && s.lt_idx >= param {
                                s.long = 0;
                            }
                        }
                    }
                    5 => {
                        for s in &mut self.stores {
                            s.short = 0;
                            s.long = 0;
                        }
                        had5 = true;
                    }
                    6 => {
                        for (j, s) in self.stores.iter_mut().enumerate() {
                            if j != cur && s.long != 0 && s.lt_idx == param {
                                s.long = 0;
                            }
                        }
                        let s = &mut self.stores[cur];
                        if s.long != 0 && s.lt_idx != param {
                            s.long = 0;
                        }
                        s.long |= structure;
                        s.lt_idx = param;
                        made_long = true;
                    }
                    _ => {}
                }
            }
        }
        if !made_long {
            self.stores[cur].short |= structure;
        }
        // Commands in a broken stream can leave too many: keep to the limit.
        loop {
            let used = self
                .stores
                .iter()
                .filter(|s| s.short | s.long != 0)
                .count();
            if used <= self.max_refs {
                break;
            }
            let oldest = self
                .stores
                .iter_mut()
                .filter(|s| !s.current && s.short != 0)
                .min_by_key(|s| s.frame_num_wrap);
            match oldest {
                Some(store) => store.short = 0,
                None => {
                    if let Some(store) = self
                        .stores
                        .iter_mut()
                        .filter(|s| !s.current && s.long != 0)
                        .min_by_key(|s| s.lt_idx)
                    {
                        store.long = 0;
                    } else {
                        break;
                    }
                }
            }
        }
        had5
    }

    /// Drops frames nothing refers to any more, keeping a few's motion storage for reuse.
    pub fn collect(&mut self, spare: &mut Vec<Vec<MbMotion>>) {
        let mut at = 0;
        while at < self.stores.len() {
            let store = &self.stores[at];
            if store.current || store.short | store.long != 0 {
                at += 1;
                continue;
            }
            let store = self.stores.remove(at);
            if spare.len() < 2
                && let Some(data) = store.data.and_then(|data| Arc::try_unwrap(data).ok()) {
                    spare.push(data.motion);
                }
        }
    }
}
