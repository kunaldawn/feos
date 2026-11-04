//! `diff`: compares two files with a longest-common-subsequence search.
//!
//! Output is the POSIX default format, or the unified format with `-u`. The
//! search is Myers' greedy algorithm, so memory grows with the number of
//! differences and not with the file size; identical prefixes and suffixes are
//! trimmed first and a very large difference is refused instead of exhausting
//! the heap.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::opts::{bad_option, Opt, Opts};
use super::{error, parse_usize, path_error, read_path, split_lines, usage, Ctx};

/// Most differences the search will track, which bounds its memory.
const MAX_EDITS: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edit {
    Keep,
    Delete,
    Insert,
}

pub(super) fn diff(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut unified: Option<usize> = None;
    let mut brief = false;
    let mut opts = Opts::new(args, "uqU:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('u') => unified = Some(3),
            Opt::Flag('q') => brief = true,
            Opt::Value('U', value) => unified = Some(parse_usize(value).unwrap_or(3)),
            other => return bad_option(ctx, "diff", other),
        }
    }
    let files = core::mem::take(&mut opts.operands);
    if files.len() != 2 {
        return usage(ctx.stderr, "diff", "diff [-uq] [-U N] FILE1 FILE2");
    }

    let left_data = match read_path(ctx, &files[0]) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "diff", &files[0], err);
            return 2;
        }
    };
    let right_data = match read_path(ctx, &files[1]) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "diff", &files[1], err);
            return 2;
        }
    };
    if left_data == right_data {
        return 0;
    }
    if brief {
        let message = format!("Files {} and {} differ\n", files[0], files[1]);
        ctx.stdout.write(message.as_bytes());
        return 1;
    }

    let left = split_lines(&left_data);
    let right = split_lines(&right_data);
    let Some(edits) = shortest_edit(&left, &right) else {
        error(ctx.stderr, "diff", "the files differ too much to compare");
        return 2;
    };

    let mut output = Vec::new();
    match unified {
        Some(context) => write_unified(&mut output, &files, &left, &right, &edits, context),
        None => write_normal(&mut output, &left, &right, &edits),
    }
    ctx.stdout.write(&output);
    1
}

/// Returns the edit script from `left` to `right`, or `None` past [`MAX_EDITS`].
fn shortest_edit(left: &[&[u8]], right: &[&[u8]]) -> Option<Vec<Edit>> {
    let mut prefix = 0usize;
    while prefix < left.len() && prefix < right.len() && left[prefix] == right[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < left.len() - prefix
        && suffix < right.len() - prefix
        && left[left.len() - 1 - suffix] == right[right.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let a = &left[prefix..left.len() - suffix];
    let b = &right[prefix..right.len() - suffix];

    let total = a.len() + b.len();
    let limit = total.min(MAX_EDITS);
    let offset = limit as isize + 1;
    let mut furthest = alloc::vec![0usize; 2 * limit + 3];
    let mut history: Vec<Vec<usize>> = Vec::new();
    let mut found = None;

    'search: for edits in 0..=limit as isize {
        // Only the diagonals this round can reach need saving.
        let low = (offset - edits - 1) as usize;
        let high = (offset + edits + 1) as usize;
        history.push(furthest[low..=high].to_vec());
        let mut diagonal = -edits;
        while diagonal <= edits {
            let index = (offset + diagonal) as usize;
            let mut x = if diagonal == -edits
                || (diagonal != edits && furthest[index - 1] < furthest[index + 1])
            {
                furthest[index + 1]
            } else {
                furthest[index - 1] + 1
            };
            let mut y = (x as isize - diagonal) as usize;
            while x < a.len() && y < b.len() && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            furthest[index] = x;
            if x >= a.len() && y >= b.len() {
                found = Some(edits);
                break 'search;
            }
            diagonal += 2;
        }
    }
    let found = found?;

    // Walk the saved rounds backwards to recover the path.
    let mut reversed = Vec::new();
    let mut x = a.len() as isize;
    let mut y = b.len() as isize;
    for edits in (0..=found).rev() {
        let round = &history[edits as usize];
        let base = offset - edits - 1;
        let at = |diagonal: isize| round[(offset + diagonal - base) as usize] as isize;
        let diagonal = x - y;
        let previous = if diagonal == -edits || (diagonal != edits && at(diagonal - 1) < at(diagonal + 1))
        {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let previous_x = at(previous);
        let previous_y = previous_x - previous;
        while x > previous_x && y > previous_y {
            reversed.push(Edit::Keep);
            x -= 1;
            y -= 1;
        }
        if edits > 0 {
            reversed.push(if x == previous_x { Edit::Insert } else { Edit::Delete });
            x = previous_x;
            y = previous_y;
        }
    }

    let mut script = alloc::vec![Edit::Keep; prefix];
    script.extend(reversed.into_iter().rev());
    script.extend(core::iter::repeat_n(Edit::Keep, suffix));
    Some(script)
}

fn push_line(output: &mut Vec<u8>, marker: &str, line: &[u8]) {
    output.extend_from_slice(marker.as_bytes());
    output.extend_from_slice(line);
    if !line.ends_with(b"\n") {
        output.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

fn write_normal(output: &mut Vec<u8>, left: &[&[u8]], right: &[&[u8]], edits: &[Edit]) {
    let range = |from: usize, to: usize| -> String {
        if to <= from + 1 {
            format!("{to}")
        } else {
            format!("{},{to}", from + 1)
        }
    };
    let (mut l, mut r) = (0usize, 0usize);
    let mut index = 0usize;
    while index < edits.len() {
        if edits[index] == Edit::Keep {
            l += 1;
            r += 1;
            index += 1;
            continue;
        }
        let (start_l, start_r) = (l, r);
        while index < edits.len() && edits[index] != Edit::Keep {
            if edits[index] == Edit::Delete {
                l += 1;
            } else {
                r += 1;
            }
            index += 1;
        }
        let header = match (l > start_l, r > start_r) {
            (true, true) => format!("{}c{}\n", range(start_l, l), range(start_r, r)),
            (true, false) => format!("{}d{}\n", range(start_l, l), start_r),
            _ => format!("{}a{}\n", start_l, range(start_r, r)),
        };
        output.extend_from_slice(header.as_bytes());
        for line in &left[start_l..l] {
            push_line(output, "< ", line);
        }
        if l > start_l && r > start_r {
            output.extend_from_slice(b"---\n");
        }
        for line in &right[start_r..r] {
            push_line(output, "> ", line);
        }
    }
}

fn write_unified(
    output: &mut Vec<u8>,
    files: &[String],
    left: &[&[u8]],
    right: &[&[u8]],
    edits: &[Edit],
    context: usize,
) {
    output.extend_from_slice(format!("--- {}\n+++ {}\n", files[0], files[1]).as_bytes());
    let mut index = 0usize;
    let (mut l, mut r) = (0usize, 0usize);
    while index < edits.len() {
        if edits[index] == Edit::Keep {
            l += 1;
            r += 1;
            index += 1;
            continue;
        }
        // A hunk runs until `2 * context + 1` unchanged lines in a row.
        let lead = context.min(index).min(l);
        let hunk_start = index - lead;
        let mut hunk_end = index;
        let mut scan = index;
        let mut unchanged = 0usize;
        while scan < edits.len() {
            if edits[scan] == Edit::Keep {
                unchanged += 1;
                if unchanged > context * 2 {
                    break;
                }
            } else {
                unchanged = 0;
                hunk_end = scan + 1;
            }
            scan += 1;
        }
        let hunk_end = (hunk_end + context).min(edits.len());

        let (first_l, first_r) = (l - lead, r - lead);
        let (mut count_l, mut count_r) = (0usize, 0usize);
        for edit in &edits[hunk_start..hunk_end] {
            match edit {
                Edit::Keep => {
                    count_l += 1;
                    count_r += 1;
                }
                Edit::Delete => count_l += 1,
                Edit::Insert => count_r += 1,
            }
        }
        let spec = |first: usize, count: usize| -> String {
            if count == 1 {
                format!("{}", first + 1)
            } else if count == 0 {
                format!("{first},0")
            } else {
                format!("{},{count}", first + 1)
            }
        };
        let header = format!("@@ -{} +{} @@\n", spec(first_l, count_l), spec(first_r, count_r));
        output.extend_from_slice(header.as_bytes());

        let (mut at_l, mut at_r) = (first_l, first_r);
        for edit in &edits[hunk_start..hunk_end] {
            match edit {
                Edit::Keep => {
                    push_line(output, " ", left[at_l]);
                    at_l += 1;
                    at_r += 1;
                }
                Edit::Delete => {
                    push_line(output, "-", left[at_l]);
                    at_l += 1;
                }
                Edit::Insert => {
                    push_line(output, "+", right[at_r]);
                    at_r += 1;
                }
            }
        }
        l = at_l;
        r = at_r;
        index = hunk_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(left: &[&[u8]], right: &[&[u8]]) -> Vec<Edit> {
        shortest_edit(left, right).unwrap()
    }

    #[test]
    fn finds_the_shortest_script() {
        use Edit::{Delete as D, Insert as I, Keep as K};
        assert_eq!(script(&[b"a", b"b"], &[b"a", b"b"]), [K, K]);
        assert_eq!(script(&[b"a", b"b", b"c"], &[b"a", b"c"]), [K, D, K]);
        assert_eq!(script(&[b"a", b"c"], &[b"a", b"b", b"c"]), [K, I, K]);
        assert_eq!(script(&[], &[b"x"]), [I]);
        assert_eq!(script(&[b"x"], &[]), [D]);
    }

    #[test]
    fn formats_normal_output() {
        let left: Vec<&[u8]> = alloc::vec![b"a\n", b"b\n", b"c\n"];
        let right: Vec<&[u8]> = alloc::vec![b"a\n", b"x\n", b"c\n", b"d\n"];
        let mut output = Vec::new();
        write_normal(&mut output, &left, &right, &script(&left, &right));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "2c2\n< b\n---\n> x\n3a4\n> d\n"
        );
    }

    #[test]
    fn formats_unified_output() {
        let left: Vec<&[u8]> = alloc::vec![b"a\n", b"b\n", b"c\n"];
        let right: Vec<&[u8]> = alloc::vec![b"a\n", b"x\n", b"c\n"];
        let mut output = Vec::new();
        let files = [String::from("l"), String::from("r")];
        write_unified(&mut output, &files, &left, &right, &script(&left, &right), 3);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "--- l\n+++ r\n@@ -1,3 +1,3 @@\n a\n-b\n+x\n c\n"
        );
    }

    #[test]
    fn refuses_inputs_that_differ_too_much() {
        let left: Vec<&[u8]> = alloc::vec![b"a"; MAX_EDITS + 1];
        let right: Vec<&[u8]> = alloc::vec![b"b"; MAX_EDITS + 1];
        assert!(shortest_edit(&left, &right).is_none());
    }
}
