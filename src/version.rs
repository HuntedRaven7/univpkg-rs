use std::cmp::Ordering;

pub fn compare(a: &str, b: &str) -> Ordering {
    let (ea, ra) = split_epoch(a);
    let (eb, rb) = split_epoch(b);
    match ea.cmp(&eb) {
        Ordering::Equal => {}
        o => return o,
    }
    let (ua, rev_a) = split_revision(ra);
    let (ub, rev_b) = split_revision(rb);
    match verrevcmp(ua.as_bytes(), ub.as_bytes()) {
        Ordering::Equal => verrevcmp(rev_a.as_bytes(), rev_b.as_bytes()),
        o => o,
    }
}

pub fn satisfies(candidate: &str, op: &str, requirement: &str) -> bool {
    let ord = compare(candidate, requirement);
    match op {
        "<<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        "=" => ord == Ordering::Equal,
        ">=" => ord != Ordering::Less,
        ">>" => ord == Ordering::Greater,
        _ => false,
    }
}

fn split_epoch(s: &str) -> (u64, &str) {
    match s.split_once(':') {
        Some((e, rest))
            if !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit()) =>
        {
            (e.parse().unwrap_or(0), rest)
        }
        _ => (0, s),
    }
}

fn split_revision(s: &str) -> (&str, &str) {
    match s.rsplit_once('-') {
        Some((upstream, rev)) => (upstream, rev),
        None => (s, "0"),
    }
}

fn order(c: u8) -> i32 {
    match c {
        0 => 0,
        c if c.is_ascii_digit() => 0,
        c if c.is_ascii_alphabetic() => c as i32,
        b'~' => -1,
        c => c as i32 + 256,
    }
}

fn verrevcmp(mut a: &[u8], mut b: &[u8]) -> Ordering {
    loop {
        let mut first_diff = 0;

        while (a.first().is_some_and(|&c| !c.is_ascii_digit()))
            || (b.first().is_some_and(|&c| !c.is_ascii_digit()))
        {
            let ac = order(a.first().copied().unwrap_or(0));
            let bc = order(b.first().copied().unwrap_or(0));
            match ac.cmp(&bc) {
                Ordering::Equal => {}
                o => return o,
            }
            a = a.get(1..).unwrap_or_default();
            b = b.get(1..).unwrap_or_default();
        }

        while a.first() == Some(&b'0') {
            a = &a[1..];
        }
        while b.first() == Some(&b'0') {
            b = &b[1..];
        }
        while a.first().is_some_and(|c| c.is_ascii_digit())
            && b.first().is_some_and(|c| c.is_ascii_digit())
        {
            if first_diff == 0 {
                first_diff = a[0] as i32 - b[0] as i32;
            }
            a = &a[1..];
            b = &b[1..];
        }

        if a.first().is_some_and(|c| c.is_ascii_digit()) {
            return Ordering::Greater;
        }
        if b.first().is_some_and(|c| c.is_ascii_digit()) {
            return Ordering::Less;
        }
        if first_diff != 0 {
            return first_diff.cmp(&0);
        }
        if a.is_empty() && b.is_empty() {
            return Ordering::Equal;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    fn lt(a: &str, b: &str) {
        assert_eq!(compare(a, b), Less, "{a} should sort before {b}");
    }

    #[test]
    fn known_orderings() {
        lt("1.0", "1.1");
        lt("1.0", "2.0");
        lt("0.9", "1.0");
        lt("0.1", "0.10");
        lt("1.0", "1.0a");
        lt("1.0a", "1.0b");
        lt("2.1a", "2.1b");
        lt("1.0-1", "1.0-2");
        lt("1.0-1", "1.0.1");
        lt("1.0~rc1", "1.0");
        lt("1.0~~a", "1.0~rc1");
        lt("1.0", "2.0");
        lt("1.0+dfsg1", "2.0");
        assert_eq!(compare("1.0", "1.0"), Equal);
        assert_eq!(compare("1.0-0", "1.0"), Equal);
        // epoch wins over everything
        lt("2.0", "1:1.0");
        lt("1:2.0", "2:0.1");
        assert_eq!(compare("1:1.0", "1:1.0-1"), Less);
        // revision comparisons
        lt("1.0-1", "1.0-2");
        lt("1.0-1.1", "1.0-1.2");
    }

    #[test]
    fn constraints() {
        assert!(satisfies("1.2", ">=", "1.0"));
        assert!(satisfies("1.0", ">=", "1.0"));
        assert!(!satisfies("0.9", ">=", "1.0"));
        assert!(satisfies("1.0", "<<", "1.1"));
        assert!(satisfies("1.0", "=", "1.0"));
        assert!(satisfies("1.0-1", ">=", "1.0"));
        assert!(satisfies("1.0", ">>", "1.0~rc1"));
        assert!(satisfies("1.0-1", "=", "1.0-1"));
        assert!(!satisfies("1.0-2", "=", "1.0-1"));
    }
}
