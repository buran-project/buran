//! URI path utilities shared by the HTTP layer and the router.

/// Percent-decode and collapse `.`/`..` segments; rejects traversal above
/// root by construction (segments are resolved on a stack).
pub fn normalize_path(path: &[u8]) -> Vec<u8> {
    let decoded = percent_decode(path);
    let mut stack: Vec<&[u8]> = Vec::new();
    for segment in decoded.split(|&b| b == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    let mut out = Vec::with_capacity(decoded.len());
    for s in &stack {
        out.push(b'/');
        out.extend_from_slice(s);
    }
    if out.is_empty() || decoded.ends_with(b"/") {
        out.push(b'/');
    }
    out
}

pub fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len()
            && let (Some(hi), Some(lo)) = (hex(input[i + 1]), hex(input[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// IMF-fixdate (RFC 9110): `Sun, 06 Nov 1994 08:49:37 GMT`.
pub fn http_date(unix_secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // epoch was Thursday
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let days = unix_secs / 86_400;
    let rem = unix_secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days as i64);

    format!(
        "{}, {day:02} {} {year} {hh:02}:{mm:02}:{ss:02} GMT",
        DAYS[(days % 7) as usize],
        MONTHS[(month - 1) as usize],
    )
}

/// Days-since-epoch -> (year, month, day); Howard Hinnant's algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(p: &str) -> String {
        String::from_utf8(normalize_path(p.as_bytes())).unwrap()
    }

    #[test]
    fn normalize_keeps_plain_paths() {
        assert_eq!(norm("/a/b/c"), "/a/b/c");
        assert_eq!(norm("/"), "/");
    }

    #[test]
    fn normalize_collapses_dot_segments() {
        assert_eq!(norm("/a/./b"), "/a/b");
        assert_eq!(norm("/a//b"), "/a/b");
        // Trailing slash follows the original input, not the popped segment.
        assert_eq!(norm("/a/b/.."), "/a");
        assert_eq!(norm("/a/b/../"), "/a/");
        assert_eq!(norm("/a/b/../c"), "/a/c");
    }

    #[test]
    fn normalize_cannot_escape_root() {
        // `..` popping past the root is absorbed, never climbs above `/`.
        assert_eq!(norm("/../../etc/passwd"), "/etc/passwd");
        assert_eq!(norm("/a/../../b"), "/b");
    }

    #[test]
    fn normalize_preserves_trailing_slash() {
        assert_eq!(norm("/a/b/"), "/a/b/");
        assert_eq!(norm("/a/"), "/a/");
    }

    #[test]
    fn normalize_decodes_before_resolving() {
        // %2e%2e is `..` after decode and must still be resolved on the stack.
        assert_eq!(norm("/a/%2e%2e/b"), "/b");
        assert_eq!(norm("/foo%2Fbar"), "/foo/bar");
    }

    #[test]
    fn normalize_surfaces_decoded_control_bytes() {
        // Load-bearing for the http1 layer's control-byte reject (response-
        // splitting defence): percent-encoded CR/LF/NUL in the raw target — which
        // httparse accepts as valid target bytes — decode to real control bytes
        // here, so the caller must reject them before they reach a header/$_SERVER.
        let split = normalize_path(b"/foo%0d%0aSet-Cookie:%20x");
        assert!(split.contains(&b'\r') && split.contains(&b'\n'));
        assert!(normalize_path(b"/a%00b").contains(&0));
        // A clean path carries no controls, so the reject never fires on it.
        assert!(!normalize_path(b"/a/b/c").iter().any(|&b| b < 0x20 || b == 0x7f));
    }

    #[test]
    fn percent_decode_valid_and_invalid() {
        assert_eq!(percent_decode(b"a%20b"), b"a b");
        assert_eq!(percent_decode(b"%41%42"), b"AB");
        // Invalid hex is left verbatim.
        assert_eq!(percent_decode(b"%zz"), b"%zz");
        // Truncated escape at end of input.
        assert_eq!(percent_decode(b"abc%2"), b"abc%2");
        assert_eq!(percent_decode(b"%"), b"%");
    }

    #[test]
    fn http_date_matches_rfc_example() {
        // RFC 9110 sample: 784111777 == Sun, 06 Nov 1994 08:49:37 GMT.
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        // Epoch itself.
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn civil_from_days_known_points() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01 is 11017 days after the epoch.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // Leap day.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
