//! 版本号解析与比较。
//!
//! 只覆盖 dsh / node 真实出现的形状：`0.2.2-alpha.3`、`24.13.0`、`v0.2.1-rc.5`。
//! 不引入 semver crate：规则小到可以自己写，出问题时也更好定位。

use std::cmp::Ordering;
use std::fmt;

/// 预发布标识符。`alpha` 与 `alpha.2` 的 `2` 是两种东西，比较规则不同。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreId {
    Numeric(u64),
    Alpha(String),
}

impl PreId {
    fn parse(raw: &str) -> Self {
        match raw.parse::<u64>() {
            Ok(number) => Self::Numeric(number),
            Err(_) => Self::Alpha(raw.to_owned()),
        }
    }
}

impl fmt::Display for PreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Numeric(number) => write!(f, "{number}"),
            Self::Alpha(text) => f.write_str(text),
        }
    }
}

/// 一个已解析的版本号。构建号（`+sha`）保留但不参与比较 —— 也不参与相等判断，
/// 否则 `1.0.0+abc == 1.0.0` 会是假，而 `Ord` 说是相等，两边就打架了。
#[derive(Clone, Debug)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<PreId>,
    pub build: String,
}

impl Version {
    /// 解析 `v0.2.2-alpha.3` / `24.13.0` / `0.2.2-alpha.3+build.7`。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (rest, build) = match text.split_once('+') {
            Some((rest, build)) => (rest, build.to_owned()),
            None => (text, String::new()),
        };
        let (core, pre) = match rest.split_once('-') {
            Some((core, pre)) => (
                core,
                pre.split('.')
                    .filter(|part| !part.is_empty())
                    .map(PreId::parse)
                    .collect(),
            ),
            None => (rest, Vec::new()),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre,
            build,
        })
    }

    /// 从 `dsh 0.2.2-alpha.3` / `v24.13.0` 这种整行输出里挑出版本号。
    #[must_use]
    pub fn parse_from_output(output: &str) -> Option<Self> {
        output.split_whitespace().find_map(|token| {
            let candidate = token.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '+'
            });
            // `node --version` 打 `v24.13.0`，`dsh --version` 打 `0.2.2-alpha.3`，两种都要认。
            let digits = candidate.strip_prefix(['v', 'V']).unwrap_or(candidate);
            let looks_like_version =
                digits.chars().next().is_some_and(|c| c.is_ascii_digit()) && digits.contains('.');
            looks_like_version.then(|| Self::parse(candidate)).flatten()
        })
    }

    /// 是否落在 `[min, max)` 区间内。
    #[must_use]
    pub fn in_range(&self, min: &Self, max: &Self) -> bool {
        self >= min && self < max
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            let pre = self
                .pre
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(".");
            write!(f, "-{pre}")?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build)?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                // 有预发布号的排在正式版之前：0.2.2-alpha.3 < 0.2.2。
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => compare_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_pre(left: &[PreId], right: &[PreId]) -> Ordering {
    for (a, b) in left.iter().zip(right.iter()) {
        let order = match (a, b) {
            (PreId::Numeric(x), PreId::Numeric(y)) => x.cmp(y),
            (PreId::Alpha(x), PreId::Alpha(y)) => x.cmp(y),
            // 数字标识符永远小于字母标识符。
            (PreId::Numeric(_), PreId::Alpha(_)) => Ordering::Less,
            (PreId::Alpha(_), PreId::Numeric(_)) => Ordering::Greater,
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    // 前缀更短的排前面：alpha < alpha.1。
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("解析失败：{text}"))
    }

    #[test]
    fn parses_leading_v_and_prerelease() {
        assert_eq!(v("v0.2.2-alpha.3").to_string(), "0.2.2-alpha.3");
        assert_eq!(v("24.13.0").major, 24);
        assert!(v("0.2.2-alpha.3").pre == vec![PreId::Alpha("alpha".into()), PreId::Numeric(3)]);
    }

    #[test]
    fn orders_prereleases_against_releases() {
        assert!(v("0.2.2-alpha.3") > v("0.2.2-alpha.2"));
        assert!(v("0.2.2-alpha.3") < v("0.2.2"));
        assert!(v("0.2.2-alpha") < v("0.2.2-alpha.1"));
        assert!(v("0.2.1-rc.5") < v("0.2.2-alpha.2"));
        assert_eq!(v("0.2.2+abc"), v("0.2.2"));
    }

    #[test]
    fn range_is_half_open() {
        let min = v("0.2.2-alpha.2");
        let max = v("0.3.0");
        assert!(v("0.2.2-alpha.2").in_range(&min, &max));
        assert!(v("0.2.2-alpha.3").in_range(&min, &max));
        assert!(v("0.2.9").in_range(&min, &max));
        assert!(!v("0.2.1-rc.5").in_range(&min, &max));
        assert!(!v("0.3.0").in_range(&min, &max));
    }

    #[test]
    fn reads_version_out_of_command_output() {
        assert_eq!(
            Version::parse_from_output("dsh 0.2.2-alpha.3")
                .unwrap()
                .to_string(),
            "0.2.2-alpha.3"
        );
        assert_eq!(
            Version::parse_from_output("v24.13.0").unwrap().to_string(),
            "24.13.0"
        );
        assert_eq!(Version::parse_from_output("(empty)"), None);
    }
}
