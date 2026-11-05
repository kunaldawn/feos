//! DNS: A queries, and reading their answers (RFC 1035).

use alloc::vec::Vec;

use feos_hal::net::Ipv4Addr;

/// The port name servers listen on.
pub const PORT: u16 = 53;
/// Longest name on the wire.
const MAX_NAME: usize = 255;
/// How many compression pointers one name may go through.
const MAX_POINTERS: usize = 16;
/// The most records to look through in an answer.
const MAX_RECORDS: usize = 64;

/// What an answer said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// The first address in it.
    Address(Ipv4Addr),
    /// The name doesn't exist, or has no address.
    NoSuchName,
    /// The server couldn't say, for the reason in this code.
    ServerError(u8),
}

/// Writes a recursive query for a name's A records. `None` for a name that can't
/// be put on the wire.
pub fn build_query(id: u16, name: &str) -> Option<Vec<u8>> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > MAX_NAME - 2 {
        return None;
    }
    let mut out = Vec::with_capacity(18 + name.len());
    out.extend_from_slice(&id.to_be_bytes());
    // Recursion desired, one question.
    out.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        if !label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
            return None;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    // Type A, class IN.
    out.extend_from_slice(&[0, 1, 0, 1]);
    Some(out)
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Compares a query's name with one in a message, compression included, ignoring
/// case. Returns the offset just past the name where it's written, and whether it
/// matched. A loop or an overrun is `None`.
fn read_name(message: &[u8], start: usize, expected: Option<&str>) -> Option<(usize, bool)> {
    let mut at = start;
    let mut after = None;
    let mut pointers = 0;
    let mut matches = true;
    let mut expected = expected.map(|name| name.strip_suffix('.').unwrap_or(name).split('.'));
    let mut total = 0usize;
    loop {
        let length = *message.get(at)?;
        match length & 0xc0 {
            0xc0 => {
                let low = *message.get(at + 1)?;
                after.get_or_insert(at + 2);
                pointers += 1;
                if pointers > MAX_POINTERS {
                    return None;
                }
                let target = usize::from(length & 0x3f) << 8 | usize::from(low);
                // Pointers go backwards, which is what stops most loops, and the
                // count stops the rest.
                if target >= at {
                    return None;
                }
                at = target;
            }
            0 => {
                if length == 0 {
                    if let Some(expected) = expected.as_mut() {
                        matches &= expected.next().is_none();
                    }
                    return Some((after.unwrap_or(at + 1), matches));
                }
                let length = usize::from(length);
                total += length + 1;
                if total > MAX_NAME {
                    return None;
                }
                let label = message.get(at + 1..at + 1 + length)?;
                if let Some(expected) = expected.as_mut() {
                    matches &= expected
                        .next()
                        .is_some_and(|wanted| wanted.as_bytes().eq_ignore_ascii_case(label));
                }
                at += 1 + length;
            }
            _ => return None,
        }
    }
}

/// Reads the answer to the query with this `id` for this `name`. `None` means it's
/// not an answer to that question, which may well be someone being clever.
pub fn parse_response(message: &[u8], id: u16, name: &str) -> Option<Answer> {
    if be16(message, 0)? != id {
        return None;
    }
    let flags = be16(message, 2)?;
    // The response bit.
    if flags & 0x8000 == 0 {
        return None;
    }
    let questions = be16(message, 4)?;
    let answers = be16(message, 6)?;
    if questions != 1 {
        return None;
    }
    let (after, same) = read_name(message, 12, Some(name))?;
    if !same {
        return None;
    }
    let mut at = after.checked_add(4)?;
    if at > message.len() {
        return None;
    }
    match flags & 0xf {
        0 => {}
        3 => return Some(Answer::NoSuchName),
        code => return Some(Answer::ServerError(code as u8)),
    }
    for _ in 0..usize::from(answers).min(MAX_RECORDS) {
        let (after, _) = read_name(message, at, None)?;
        let kind = be16(message, after)?;
        let class = be16(message, after + 2)?;
        let length = usize::from(be16(message, after + 8)?);
        let data = message.get(after + 10..after + 10 + length)?;
        if kind == 1 && class == 1 && length == 4 {
            return Some(Answer::Address(Ipv4Addr([data[0], data[1], data[2], data[3]])));
        }
        // A CNAME's target comes with its own record further on.
        at = after + 10 + length;
    }
    Some(Answer::NoSuchName)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(id: u16, name: &str, records: &[u8], count: u16, code: u8) -> Vec<u8> {
        let mut message = build_query(id, name).unwrap();
        message[2] = 0x81;
        message[3] = 0x80 | code;
        message[6..8].copy_from_slice(&count.to_be_bytes());
        message.extend_from_slice(records);
        message
    }

    /// A record whose name points back at the question.
    const A_RECORD: [u8; 16] = [0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 0, 2, 99];

    #[test]
    fn a_query_is_laid_out_as_the_rfc_says() {
        let query = build_query(0x1234, "a.bc").unwrap();
        assert_eq!(
            query,
            [0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, b'a', 2, b'b', b'c', 0, 0, 1, 0, 1]
        );
    }

    #[test]
    fn names_that_cannot_be_written_are_refused() {
        let long_label = "a".repeat(64);
        let long_name = "ab.".repeat(100);
        for name in ["", ".", "a..b", long_label.as_str(), long_name.as_str(), "a b.c", "a\0.c"] {
            assert!(build_query(1, name).is_none(), "{name:?}");
        }
        assert!(build_query(1, "feos.test.").is_some());
    }

    #[test]
    fn reads_an_address() {
        let message = answer(7, "feos.test", &A_RECORD, 1, 0);
        assert_eq!(
            parse_response(&message, 7, "feos.test"),
            Some(Answer::Address(Ipv4Addr([10, 0, 2, 99])))
        );
    }

    #[test]
    fn the_case_of_the_answer_does_not_matter() {
        let message = answer(7, "FEOS.test", &A_RECORD, 1, 0);
        assert!(matches!(parse_response(&message, 7, "feos.TEST"), Some(Answer::Address(_))));
    }

    #[test]
    fn follows_a_cname_to_its_address() {
        let mut records = vec![0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 4, 1, b'x', 0xc0, 12];
        records.extend_from_slice(&A_RECORD);
        let message = answer(7, "feos.test", &records, 2, 0);
        assert!(matches!(parse_response(&message, 7, "feos.test"), Some(Answer::Address(_))));
    }

    #[test]
    fn another_question_or_id_is_not_an_answer() {
        let message = answer(7, "feos.test", &A_RECORD, 1, 0);
        assert_eq!(parse_response(&message, 8, "feos.test"), None);
        assert_eq!(parse_response(&message, 7, "other.test"), None);
        assert_eq!(parse_response(&message, 7, "feos.test.more"), None);
        assert_eq!(parse_response(&message, 7, "feos"), None);
        let mut query = message;
        query[2] = 0x01;
        assert_eq!(parse_response(&query, 7, "feos.test"), None);
    }

    #[test]
    fn reports_failure_codes() {
        let message = answer(7, "feos.test", &[], 0, 3);
        assert_eq!(parse_response(&message, 7, "feos.test"), Some(Answer::NoSuchName));
        let message = answer(7, "feos.test", &[], 0, 2);
        assert_eq!(parse_response(&message, 7, "feos.test"), Some(Answer::ServerError(2)));
        let message = answer(7, "feos.test", &[], 0, 0);
        assert_eq!(parse_response(&message, 7, "feos.test"), Some(Answer::NoSuchName));
    }

    #[test]
    fn pointer_loops_and_overruns_are_refused() {
        // A name that points at itself.
        assert_eq!(read_name(&[0xc0, 0], 0, None), None);
        // Two that point at each other can't happen: pointers only go backwards.
        assert_eq!(read_name(&[0xc0, 2, 0xc0, 0], 2, None).map(|(at, _)| at), None);
        // A label running off the end, and a reserved kind of length.
        assert_eq!(read_name(&[5, b'a'], 0, None), None);
        assert_eq!(read_name(&[0x80, 0], 0, None), None);
        assert_eq!(read_name(&[], 0, None), None);
        // A very long chain of backward pointers.
        let mut chain = vec![0u8];
        for index in 0..100usize {
            chain.extend_from_slice(&[0xc0, (index * 2).saturating_sub(1).max(0) as u8]);
        }
        let start = chain.len() - 2;
        assert_eq!(read_name(&chain, start, None), None);
    }

    #[test]
    fn a_record_length_that_lies_is_refused() {
        let mut record = A_RECORD;
        record[11] = 200;
        let message = answer(7, "feos.test", &record, 1, 0);
        assert_eq!(parse_response(&message, 7, "feos.test"), None);
    }

    #[test]
    fn no_input_panics() {
        let good = answer(7, "feos.test", &A_RECORD, 1, 0);
        let mut state = 0x5555_1234_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..200_000usize {
            let mut message = good.clone();
            let length = round % (message.len() + 1);
            if round % 2 == 0 {
                for byte in message.iter_mut() {
                    *byte = next() as u8;
                }
                message[..2].copy_from_slice(&7u16.to_be_bytes());
            } else {
                for _ in 0..1 + round % 5 {
                    let at = next() as usize % message.len();
                    message[at] = next() as u8;
                }
            }
            let _ = parse_response(&message[..length], 7, "feos.test");
            let _ = read_name(&message[..length], 12, None);
        }
    }
}
