#!/usr/bin/env python3
"""Generates libs/audio-codec/src/sbr_tables.rs.

    ./scripts/gen-sbr-tables.py <libxaac directory> > libs/audio-codec/src/sbr_tables.rs

The tables are the normative ones of ISO/IEC 14496-3 for SBR and parametric stereo, as they
appear in libxaac, the Android Open Source Project's AAC decoder, under Apache-2.0. Its source
ships in the `libxaac-sys` crate (version 0.1.0 was used) as `libxaac/decoder` and
`libxaac/common`. No FFmpeg or other LGPL table goes in here. Huffman codes are recovered by
running libxaac's table formats, and checked to be complete prefix codes. The mixing matrices
are computed here from the standard's formulas and libxaac's level tables.
"""

import math
import os
import re
import sys
from fractions import Fraction


def body(source, marker, start=0):
    """The numbers in the brace list after a comment, as text."""
    at = source.index(marker, start)
    first = source.index("{", at)
    depth = 0
    for position in range(first, len(source)):
        if source[position] == "{":
            depth += 1
        elif source[position] == "}":
            depth -= 1
            if depth == 0:
                text = source[first + 1 : position]
                # Comments in a list hold numbers too.
                return re.sub(r"//[^\n]*", "", text)
    sys.exit(f"no end to {marker}")


def integers(source, marker):
    text = body(source, marker)
    # Written as (WORD16)-0x3fff, or as 40 - 27.
    text = re.sub(r"\(\s*WORD\d+\s*\)", "", text)
    text = re.sub(r"\(\s*UWORD\d+\s*\)", "", text)
    text = text.replace("{", " ").replace("}", " ")
    out = []
    for item in text.split(","):
        item = item.strip()
        if item:
            out.append(eval(item, {"__builtins__": {}}))
    return out


def floats(source, marker):
    text = body(source, marker)
    return [float(word.rstrip("fF")) for word in re.findall(r"-?\d+\.\d*(?:[eE][-+]?\d+)?[fF]?", text)]


def q(value, bits):
    return max(min(round(value * (1 << bits)), (1 << 31) - 1), -(1 << 31))


def array(name, values, kind="i32", per_line=8, doc=None):
    out = []
    if doc:
        out.append(f"/// {doc}")
    out.append(f"pub(crate) static {name}: [{kind}; {len(values)}] = [")
    for at in range(0, len(values), per_line):
        out.append("    " + " ".join(f"{value}," for value in values[at : at + per_line]))
    out.append("];")
    return "\n".join(out)


def code_text(name, codes, doc=None):
    out = []
    if doc:
        out.append(f"/// {doc}")
    out.append(f"pub(crate) static {name}: [Code; {len(codes)}] = [")
    for at in range(0, len(codes), 4):
        row = [f"c({code:#010x}, {length}, {symbol})," for code, length, symbol in codes[at : at + 4]]
        out.append("    " + " ".join(row))
    out.append("];")
    return "\n".join(out)


def leading_ones(word):
    count = 0
    while count < 32 and word & (0x80000000 >> count):
        count += 1
    return count


def run_sbr_table(word, table, index):
    """libxaac's lookup, on a 32-bit word: returns the symbol and its length."""
    mask = (0x80000000 - (1 << (31 - table[0]))) << 1 & 0xffffffff
    temp = word & mask
    length_end = table[0]
    run = leading_ones(temp)
    while True:
        offset = (index[run] >> 20) & 0xff
        length = table[offset + 1] & 0x1f
        code = index[run] & 0xfffff
        value = temp >> (32 - length)
        if value <= code:
            offset -= code - value
            return table[offset + 1] >> 5, length
        length_end += (index[run] >> 28) & 0xf
        run = length_end


def sbr_book(source, name):
    table = [v & 0xffff for v in integers(source, f"// WORD16 ixheaacd_{name}_inp_table")]
    index = [v & 0xffffffff for v in integers(source, f"// WORD32 ixheaacd_{name}_idx_table")]
    codes = []
    word = 0
    kraft = Fraction(0)
    while word < 1 << 32:
        symbol, length = run_sbr_table(word, table, index)
        codes.append((word, length, symbol))
        kraft += Fraction(1, 1 << length)
        word += 1 << (32 - length)
    if kraft != 1 or word != 1 << 32:
        sys.exit(f"{name} is not a complete prefix code: {kraft}")
    if sorted(c[2] for c in codes) != list(range(len(codes))):
        sys.exit(f"{name}'s symbols aren't a run of integers")
    return codes


def ps_book(source, marker):
    nodes = [v & 0xffff for v in integers(source, marker)]
    codes = []

    def signed8(byte):
        return byte - 256 if byte & 0x80 else byte

    def walk(node, prefix, length):
        word = nodes[node]
        for bit, child in ((0, signed8(word >> 8)), (1, signed8(word & 0xff))):
            code = (prefix << 1) | bit
            if child < 0:
                # A leaf's value goes up by 64 from its index, and can be negative.
                codes.append((code << (32 - length - 1), length + 1, child + 64 + 128))
            else:
                walk(child, code, length + 1)

    walk(0, 0, 0)
    codes.sort()
    kraft = sum(Fraction(1, 1 << length) for _, length, _ in codes)
    if kraft != 1:
        sys.exit(f"{marker} is not a complete prefix code: {kraft}")
    return codes


HEADER = """//! Tables for SBR and parametric stereo. Generated by `scripts/gen-sbr-tables.py`.
//!
//! The normative tables of ISO/IEC 14496-3, as they appear in libxaac (Android Open Source
//! Project, Apache-2.0). Huffman codes are left aligned in 32 bits and sorted, as the AAC
//! decoder's are. A parametric stereo code's symbol is 128 above the value it stands for.

#![allow(clippy::unreadable_literal)]

use crate::aac_tables::Code;

const fn c(code: u32, length: u8, symbol: u16) -> Code {
    Code { code, length, symbol }
}"""

SBR_BOOKS = [
    ("T_ENV_1_5", "t_huffman_env_1_5db"),
    ("F_ENV_1_5", "f_huffman_env_1_5db"),
    ("T_BAL_1_5", "t_huffman_env_bal_1_5db"),
    ("F_BAL_1_5", "f_huffman_env_bal_1_5db"),
    ("T_ENV_3_0", "t_huffman_env_3_0db"),
    ("F_ENV_3_0", "f_huffman_env_3_0db"),
    ("T_BAL_3_0", "t_huffman_env_bal_3_0db"),
    ("F_BAL_3_0", "f_huffman_env_bal_3_0db"),
    ("T_NOISE_3_0", "t_huffman_noise_3_0db"),
    ("T_NOISE_BAL_3_0", "t_huffman_noise_bal_3_0db"),
]

PS_BOOKS = [
    ("IID_DT", "huff_iid_dt[28]"),
    ("IID_DF", "huff_iid_df[28]"),
    ("ICC_DT", "huff_icc_dt[14]"),
    ("ICC_DF", "huff_icc_df[14]"),
    ("IID_DT_FINE", "huff_iid_dt_fine[60]"),
    ("IID_DF_FINE", "huff_iid_df_fine[60]"),
]

QUANTIZED_RHOS = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0]


def mix_a(scales, alphas, steps):
    """Mixing by the first of the standard's two methods, in Q28."""
    out = []
    for i in range(-steps, steps + 1):
        for alpha in alphas:
            right, left = scales[steps + i], scales[steps - i]
            beta = alpha * (right - left) / math.sqrt(2)
            for value in (
                left * math.cos(beta + alpha),
                right * math.cos(beta - alpha),
                left * math.sin(beta + alpha),
                right * math.sin(beta - alpha),
            ):
                out.append(q(value, 28))
    return out


def mix_b(iids, steps):
    """And by the second, from the coded levels and correlations."""
    out = []
    for i in range(-steps, steps + 1):
        level = (1 if i > 0 else -1) * iids[abs(i) - 1] if i else 0
        c_ = 10 ** (level / 20)
        for rho in QUANTIZED_RHOS:
            rho = max(rho, 0.05)
            mu = 1 + (4 * rho * rho - 4) / ((c_ + 1 / c_) ** 2)
            alpha = 0.5 * math.atan2(2 * c_ * rho, c_ * c_ - 1)
            root = math.sqrt(max(mu, 0))
            gamma = math.atan(math.sqrt(max((1 - root) / (1 + root), 0)))
            s = math.sqrt(2)
            for value in (
                s * math.cos(alpha) * math.cos(gamma),
                s * math.sin(alpha) * math.cos(gamma),
                -s * math.sin(alpha) * math.sin(gamma),
                s * math.cos(alpha) * math.sin(gamma),
            ):
                out.append(q(value, 28))
    return out


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = sys.argv[1]
    source = open(os.path.join(root, "decoder/ixheaacd_sbr_rom.c"), encoding="latin-1").read()
    common = open(os.path.join(root, "common/ixheaac_esbr_rom.c"), encoding="latin-1").read()
    parts = [HEADER]

    window = integers(source, "// WORD32 esbr_qmf_c[1280]")
    if len(window) != 1280 or window[:640] != window[640:] and window[1] != window[641]:
        sys.exit("the QMF window isn't what it was")
    parts.append(array("QMF_WINDOW", window[:640], doc="The QMF prototype, with the standard's signs, in Q31."))

    noise = floats(common, "ixheaac_random_phase[512][2]")
    if len(noise) != 1024:
        sys.exit(f"{len(noise)} noise values")
    parts.append(array("NOISE", [q(v, 31) for v in noise], per_line=6, doc="The noise table, real and imaginary, in Q31."))

    for name, table in SBR_BOOKS:
        parts.append(code_text(name, sbr_book(source, table)))
    for name, marker in PS_BOOKS:
        parts.append(code_text(name, ps_book(source, "// const WORD16 " + marker)))

    parts.append(array("START_MIN", integers(source, "// const WORD8 start_min[12]"), "u8", 12,
                       "By the rate's index among 96000 down to 8000."))
    parts.append(array("START_ROW", integers(source, "// const WORD8 offset_idx[12]"), "u8", 12))
    offsets = integers(source, "// const WORD8 ixheaacd_drc_offset[7][16]")
    parts.append(array("START_OFFSET", offsets, "i8", 16, "Seven rows of sixteen."))
    parts.append(array("STOP_MIN", integers(source, "// const WORD8 stop_min[12]"), "u8", 12))
    parts.append(array("STOP_OFFSET", integers(source, "// const WORD8 stop_off[12][14]"), "i8", 14, "Twelve rows of fourteen."))

    # Parametric stereo's filters.
    for name, marker in (
        ("P8_20", "// const FLOAT32 p8_13_20[13]"),
        ("P2_20", "// const FLOAT32 p2_13_20[13]"),
        ("P12_34", "// const FLOAT32 p12_13_34[13]"),
        ("P8_34", "// const FLOAT32 p8_13_34[13]"),
        ("P4_34", "// const FLOAT32 p4_13_34[13]"),
    ):
        values = floats(source, marker)
        if len(values) != 13:
            sys.exit(marker)
        parts.append(array(name, [q(v, 31) for v in values], per_line=7))
    for name, marker, size in (
        ("MOD_2", "// const FLOAT32 cos_mod_2channel[2][13]", 26),
        ("MOD_4", "// const FLOAT32 cos_sin_mod_4channel[4][13 * 2]", 104),
        ("MOD_8", "// const FLOAT32 cos_sin_mod_8channel[8][13 * 2]", 208),
        ("MOD_12", "// const FLOAT32 cos_sin_mod_12channel[12][13 * 2]", 312),
    ):
        values = floats(source, marker)
        if len(values) != size:
            sys.exit(f"{marker}: {len(values)}")
        parts.append(array(name, [q(v, 31) for v in values], per_line=6))

    # What each band's all-pass filter rotates by.
    for name, marker, size in (
        ("FRAC_QMF_IM", "// const FLOAT32 qmf_fract_delay_phase_factor_im", 64),
        ("FRAC_QMF_RE", "// const FLOAT32 qmf_fract_delay_phase_factor_re", 64),
        ("FRAC_SUB_IM_20", "// const FLOAT32 frac_delay_phase_fac_qmf_sub_im_20", 12),
        ("FRAC_SUB_RE_20", "// const FLOAT32 frac_delay_phase_fac_qmf_sub_re_20", 12),
        ("FRAC_SUB_IM_34", "// const FLOAT32 frac_delay_phase_fac_qmf_sub_im_34", 32),
        ("FRAC_SUB_RE_34", "// const FLOAT32 frac_delay_phase_fac_qmf_sub_re_34", 32),
        ("SER_QMF_IM", "// qmf_ser_fract_delay_phase_factor_im", 192),
        ("SER_QMF_RE", "// qmf_ser_fract_delay_phase_factor_re", 192),
        ("SER_SUB_IM_20", "// frac_delay_phase_fac_ser_qmf_sub_im_20", 36),
        ("SER_SUB_RE_20", "// frac_delay_phase_fac_ser_qmf_sub_re_20", 36),
        ("SER_SUB_IM_34", "// frac_delay_phase_fac_ser_qmf_sub_im_34", 96),
        ("SER_SUB_RE_34", "// frac_delay_phase_fac_ser_qmf_sub_re_34", 96),
    ):
        values = floats(source, marker)
        if len(values) < size:
            sys.exit(f"{marker}: {len(values)}")
        parts.append(array(name, [q(v, 31) for v in values[:size]], per_line=6))
    decay = floats(source, "// const FLOAT32 all_pass_link_decay_ser")
    parts.append(array("LINK_DECAY", [q(v, 31) for v in decay], per_line=3))

    parts.append(array("BORDERS_20", integers(source, "// const WORD32 group_borders_20_tbl"), "u8", 16))
    parts.append(array("BORDERS_34", integers(source, "// group_borders_34_tbl"), "u8", 16))

    text = body(source, "// const WORD32 bin_group_map_20").replace("NEGATE_IPD_MASK", "0")
    parts.append(array("BINS_20", [eval(v.strip()) for v in text.split(",") if v.strip()], "u8", 16))
    text = body(source, "// const WORD32 bin_group_map_34").replace("NEGATE_IPD_MASK", "0")
    parts.append(array("BINS_34", [eval(v.strip()) for v in text.split(",") if v.strip()], "u8", 16))

    scales = floats(source, "// const FLOAT32 scale_factors_flt")
    fine = floats(source, "// scale_factors_fine_flt")
    alphas = floats(source, "// const FLOAT32 alphas")
    parts.append(array("MIX_A", mix_a(scales, alphas, 7), per_line=8, doc="Per level, -7 to 7, and correlation, 0 to 7: h11, h12, h21, h22, in Q28."))
    parts.append(array("MIX_A_FINE", mix_a(fine, alphas, 15), per_line=8, doc="The same, for levels from -15 to 15."))
    iids = integers(source, "// const WORD32 quantized_iids[")
    iids_fine = integers(source, "// const WORD32 quantized_iids_fine")
    parts.append(array("MIX_B", mix_b(iids, 7), per_line=8))
    parts.append(array("MIX_B_FINE", mix_b(iids_fine, 15), per_line=8))

    # The analysis transform's rotations, which the standard writes as one exponential.
    pre = []
    for n in range(64):
        pre.append(f"({q(math.cos(math.pi * n / 64), 31)}, {q(math.sin(math.pi * n / 64), 31)})")
    parts.append(array("ANALYSIS_PRE", pre, "(i32, i32)", 4, "e^(i pi n / 64), in Q31."))
    post = []
    for k in range(32):
        angle = -math.pi * (k + 0.5) / 128
        post.append(f"({q(math.cos(angle), 31)}, {q(math.sin(angle), 31)})")
    parts.append(array("ANALYSIS_ROTATE", post, "(i32, i32)", 4, "e^(-i pi (k + 1/2) / 128), in Q31."))
    print("\n\n".join(parts))


main()
