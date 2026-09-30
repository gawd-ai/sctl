//! The four-part version (`0.6.7.431`) compared numerically, part by part.
//! A missing trailing part counts as 0, so `0.6.7` equals `0.6.7.0` and is
//! below `0.6.7.1`.

use std::cmp::Ordering;

/// A parsed version: two to four numeric parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version(Vec<u64>);

impl Version {
    /// Parse `a.b[.c[.d]]`; anything else is `None`. A leading `sctl ` or
    /// `v` is tolerated so `sctl --version` output and a tag both parse.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix("sctl ").unwrap_or(text).trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let parts: Vec<u64> = text
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if !(2..=4).contains(&parts.len()) {
            return None;
        }
        Some(Self(parts))
    }

    fn part(&self, i: usize) -> u64 {
        self.0.get(i).copied().unwrap_or(0)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (0..4)
            .map(|i| self.part(i).cmp(&other.part(i)))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text: Vec<String> = self.0.iter().map(u64::to_string).collect();
        f.write_str(&text.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_two_to_four_parts_and_the_cli_prefix() {
        assert_eq!(v("0.6.7.431").0, vec![0, 6, 7, 431]);
        assert_eq!(v("sctl 0.6.7.431").0, vec![0, 6, 7, 431]);
        assert_eq!(v("v0.6.7").0, vec![0, 6, 7]);
        assert_eq!(v("1.0").0, vec![1, 0]);
        assert!(Version::parse("1").is_none());
        assert!(Version::parse("1.2.3.4.5").is_none());
        assert!(Version::parse("1.x").is_none());
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn compares_numerically_with_missing_parts_as_zero() {
        assert!(v("0.6.7.431") > v("0.6.6.900"));
        assert!(v("0.6.10") > v("0.6.9"));
        assert_eq!(v("0.6.7").cmp(&v("0.6.7.0")), Ordering::Equal);
        assert!(v("0.6.7.1") > v("0.6.7"));
        assert!(v("1.0.0.0") > v("0.99.99.99"));
    }

    #[test]
    fn displays_as_parsed() {
        assert_eq!(v("sctl 0.6.7.431").to_string(), "0.6.7.431");
    }
}
