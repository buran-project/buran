//! Pattern matching for route `match` blocks: exact, `*` wildcard,
//! `!` negation, `~` regex; arrays are OR-sets (a value matches if it hits
//! any positive pattern and no negative one; a set of only negative
//! patterns matches unless one hits).

use regex::bytes::Regex;

pub struct PatternSet {
    positive: Vec<Pattern>,
    negative: Vec<Pattern>,
}

enum Pattern {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    /// `pre*suf` — single infix wildcard.
    Circumfix(Vec<u8>, Vec<u8>),
    Any,
    Regex(Regex),
}

impl PatternSet {
    pub fn compile<'a>(
        patterns: impl Iterator<Item = &'a str>,
        case_insensitive: bool,
    ) -> anyhow::Result<Self> {
        let mut positive = Vec::new();
        let mut negative = Vec::new();

        for raw in patterns {
            let (negated, body) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            let compiled = Pattern::compile(body, case_insensitive)?;
            if negated {
                negative.push(compiled);
            } else {
                positive.push(compiled);
            }
        }

        Ok(Self { positive, negative })
    }

    pub fn matches(&self, value: &[u8], case_insensitive: bool) -> bool {
        let value = if case_insensitive {
            std::borrow::Cow::Owned(value.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(value)
        };

        if self.negative.iter().any(|p| p.matches(&value)) {
            return false;
        }
        if self.positive.is_empty() {
            return true;
        }
        self.positive.iter().any(|p| p.matches(&value))
    }
}

/// `source` matcher: exact IPs and CIDR blocks, with `!` negation.
pub struct CidrSet {
    positive: Vec<Cidr>,
    negative: Vec<Cidr>,
}

struct Cidr {
    addr: std::net::IpAddr,
    prefix: u8,
}

impl CidrSet {
    pub fn compile<'a>(entries: impl Iterator<Item = &'a str>) -> anyhow::Result<Self> {
        let mut positive = Vec::new();
        let mut negative = Vec::new();

        for raw in entries {
            let (negated, body) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            let (addr_s, prefix_s) = match body.split_once('/') {
                Some((a, p)) => (a, Some(p)),
                None => (body, None),
            };
            let addr: std::net::IpAddr =
                addr_s.parse().map_err(|_| anyhow::anyhow!("bad address \"{raw}\" in match.source"))?;
            let max = if addr.is_ipv4() { 32 } else { 128 };
            let prefix: u8 = match prefix_s {
                Some(p) => {
                    let p: u8 = p.parse().map_err(|_| anyhow::anyhow!("bad prefix in \"{raw}\""))?;
                    anyhow::ensure!(p <= max, "prefix out of range in \"{raw}\"");
                    p
                }
                None => max,
            };
            let cidr = Cidr { addr, prefix };
            if negated {
                negative.push(cidr);
            } else {
                positive.push(cidr);
            }
        }

        Ok(Self { positive, negative })
    }

    pub fn matches(&self, ip: std::net::IpAddr) -> bool {
        if self.negative.iter().any(|c| c.contains(ip)) {
            return false;
        }
        if self.positive.is_empty() {
            return true;
        }
        self.positive.iter().any(|c| c.contains(ip))
    }
}

impl Cidr {
    fn contains(&self, ip: std::net::IpAddr) -> bool {
        fn prefix_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
            let full = (prefix / 8) as usize;
            if a[..full] != b[..full] {
                return false;
            }
            let bits = prefix % 8;
            if bits == 0 {
                return true;
            }
            let mask = !(0xffu8 >> bits);
            (a[full] & mask) == (b[full] & mask)
        }

        match (self.addr, ip) {
            (std::net::IpAddr::V4(net), std::net::IpAddr::V4(ip)) => {
                prefix_eq(&ip.octets(), &net.octets(), self.prefix)
            }
            (std::net::IpAddr::V6(net), std::net::IpAddr::V6(ip)) => {
                prefix_eq(&ip.octets(), &net.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

/// Compile a route regex with a bounded compiled program and lazy-DFA cache.
/// The `regex` crate is already linear-time on input (no catastrophic
/// backtracking); this caps compilation memory so a pathological operator
/// pattern matched against attacker-controlled input cannot blow up space.
fn build_bounded_regex(pattern: &str) -> Result<Regex, regex::Error> {
    regex::bytes::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
}

impl Pattern {
    fn compile(body: &str, case_insensitive: bool) -> anyhow::Result<Self> {
        if let Some(re) = body.strip_prefix('~') {
            let re = if case_insensitive { format!("(?i){re}") } else { re.to_string() };
            return Ok(Self::Regex(build_bounded_regex(&re)?));
        }

        let body = if case_insensitive { body.to_ascii_lowercase() } else { body.to_string() };
        let bytes = body.into_bytes();

        Ok(match bytes.iter().filter(|&&b| b == b'*').count() {
            0 => Self::Exact(bytes),
            1 => {
                let pos = bytes.iter().position(|&b| b == b'*').unwrap();
                match (pos, bytes.len()) {
                    (0, 1) => Self::Any,
                    (0, _) => Self::Suffix(bytes[1..].to_vec()),
                    (p, l) if p == l - 1 => Self::Prefix(bytes[..p].to_vec()),
                    (p, _) => Self::Circumfix(bytes[..p].to_vec(), bytes[p + 1..].to_vec()),
                }
            }
            // Multiple wildcards: degrade to a regex.
            _ => {
                let escaped = bytes
                    .split(|&b| b == b'*')
                    .map(|part| regex::escape(&String::from_utf8_lossy(part)))
                    .collect::<Vec<_>>()
                    .join(".*");
                Self::Regex(build_bounded_regex(&format!("^{escaped}$"))?)
            }
        })
    }

    fn matches(&self, value: &[u8]) -> bool {
        match self {
            Self::Exact(p) => value == &p[..],
            Self::Prefix(p) => value.starts_with(p),
            Self::Suffix(s) => value.ends_with(s),
            Self::Circumfix(p, s) => {
                value.len() >= p.len() + s.len() && value.starts_with(p) && value.ends_with(s)
            }
            Self::Any => true,
            Self::Regex(re) => re.is_match(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(patterns: &[&str], ci: bool) -> PatternSet {
        PatternSet::compile(patterns.iter().copied(), ci).unwrap()
    }

    #[test]
    fn exact_match() {
        let s = set(&["GET"], false);
        assert!(s.matches(b"GET", false));
        assert!(!s.matches(b"POST", false));
    }

    #[test]
    fn wildcard_shapes() {
        assert!(set(&["*.php"], false).matches(b"index.php", false)); // suffix
        assert!(!set(&["*.php"], false).matches(b"index.html", false));
        assert!(set(&["/api/*"], false).matches(b"/api/users", false)); // prefix
        assert!(set(&["a*z"], false).matches(b"abcz", false)); // circumfix
        assert!(!set(&["a*z"], false).matches(b"abc", false));
        assert!(set(&["*"], false).matches(b"anything", false)); // any
    }

    #[test]
    fn circumfix_needs_room_for_both_ends() {
        // `pre*suf` must not double-count overlapping bytes.
        assert!(!set(&["ab*bc"], false).matches(b"abc", false));
        assert!(set(&["ab*bc"], false).matches(b"abXbc", false));
    }

    #[test]
    fn multi_wildcard_degrades_to_regex() {
        let s = set(&["/a/*/b/*"], false);
        assert!(s.matches(b"/a/x/b/y", false));
        assert!(!s.matches(b"/a/x/c/y", false));
    }

    #[test]
    fn regex_pattern() {
        let s = set(&["~^/user/[0-9]+$"], false);
        assert!(s.matches(b"/user/42", false));
        assert!(!s.matches(b"/user/abc", false));
    }

    #[test]
    fn negation_excludes() {
        let s = set(&["!*.php"], false);
        assert!(s.matches(b"index.html", false)); // only-negative set matches by default
        assert!(!s.matches(b"index.php", false));
    }

    #[test]
    fn positive_and_negative_combined() {
        let s = set(&["/api/*", "!/api/private*"], false);
        assert!(s.matches(b"/api/users", false));
        assert!(!s.matches(b"/api/private/keys", false));
        assert!(!s.matches(b"/other", false)); // no positive hit
    }

    #[test]
    fn empty_positive_set_matches_all() {
        let s = set(&[], false);
        assert!(s.matches(b"whatever", false));
    }

    #[test]
    fn case_insensitive_matching() {
        // Pattern is lowercased at compile time; the caller lowercases the
        // value by passing the same case_insensitive flag.
        let s = set(&["Example.TEST"], true);
        assert!(s.matches(b"EXAMPLE.test", true));
        assert!(s.matches(b"example.test", true));
        // Case-sensitive set keeps the original casing and won't cross-match.
        let cs = set(&["Example"], false);
        assert!(!cs.matches(b"example", false));
    }

    fn cidr(entries: &[&str]) -> CidrSet {
        CidrSet::compile(entries.iter().copied()).unwrap()
    }

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_exact_and_block_v4() {
        assert!(cidr(&["10.0.0.1"]).matches(ip("10.0.0.1")));
        assert!(!cidr(&["10.0.0.1"]).matches(ip("10.0.0.2")));
        let block = cidr(&["10.0.0.0/24"]);
        assert!(block.matches(ip("10.0.0.200")));
        assert!(!block.matches(ip("10.0.1.1")));
    }

    #[test]
    fn cidr_non_byte_aligned_prefix() {
        let block = cidr(&["192.168.0.0/20"]);
        assert!(block.matches(ip("192.168.15.1")));
        assert!(!block.matches(ip("192.168.16.1")));
    }

    #[test]
    fn cidr_v6_block() {
        let block = cidr(&["2001:db8::/32"]);
        assert!(block.matches(ip("2001:db8::1")));
        assert!(!block.matches(ip("2001:db9::1")));
    }

    #[test]
    fn cidr_family_mismatch_never_matches() {
        assert!(!cidr(&["10.0.0.0/8"]).matches(ip("::1")));
    }

    #[test]
    fn cidr_negation_and_empty() {
        assert!(cidr(&[]).matches(ip("1.2.3.4"))); // empty = allow all
        let s = cidr(&["10.0.0.0/8", "!10.0.0.1"]);
        assert!(s.matches(ip("10.1.1.1")));
        assert!(!s.matches(ip("10.0.0.1")));
    }

    #[test]
    fn cidr_rejects_bad_input() {
        assert!(CidrSet::compile(["notanip"].into_iter()).is_err());
        assert!(CidrSet::compile(["10.0.0.0/40"].into_iter()).is_err());
    }
}
