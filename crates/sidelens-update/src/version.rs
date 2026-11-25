//! 语义化版本的三段比较。
//!
//! # 为什么不用 `semver` crate
//!
//! 我们要的比较规则比 semver **窄**：清单与 `installed.json` 里只允许
//! `MAJOR.MINOR.PATCH`，不接受预发布标签与构建元数据。用完整的 semver
//! 反而会接受 `1.0.0-beta.1 < 1.0.0` 这种我们不想支持的输入 ——
//! 一旦清单里混进预发布版本，「稳定版用户该不该收到它」就成了政策问题，
//! 而这里只想回答一个确定的问题。
//!
//! 窄规则的代价是：将来真要发预发布，得回来改这里。收益是
//! **任何非法输入都会在解析期被拒绝**，不会悄悄变成「小于所有版本」。

use std::fmt;

/// `MAJOR.MINOR.PATCH`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// 解析 `MAJOR.MINOR.PATCH`。
    ///
    /// 拒绝：段数不对、空段、非数字、带前导 `+`/`-`、前导零（`01.2.3`）、
    /// 以及任何超出 `u64` 的数字。前导零被拒绝是刻意的 ——
    /// `01` 和 `1` 在同一份清单里出现会让字符串相等与版本相等不一致。
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(VersionError::Empty);
        }
        let mut parts = trimmed.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(VersionError::Shape(text.to_string()));
        };
        Ok(Self {
            major: number(major, text)?,
            minor: number(minor, text)?,
            patch: number(patch, text)?,
        })
    }

    /// ``major.minor.patch`` 形式的字符串，可直接写进清单。
    pub fn to_string_checked(self) -> String {
        self.to_string()
    }
}

fn number(part: &str, whole: &str) -> Result<u64, VersionError> {
    if part.is_empty() {
        return Err(VersionError::Shape(whole.to_string()));
    }
    if !part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(VersionError::Shape(whole.to_string()));
    }
    if part.len() > 1 && part.starts_with('0') {
        return Err(VersionError::LeadingZero(whole.to_string()));
    }
    part.parse::<u64>()
        .map_err(|_| VersionError::Shape(whole.to_string()))
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionError {
    Empty,
    Shape(String),
    LeadingZero(String),
}

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionError::Empty => write!(f, "版本号为空"),
            VersionError::Shape(v) => {
                write!(f, "版本号 \"{v}\" 不是 MAJOR.MINOR.PATCH 形式")
            }
            VersionError::LeadingZero(v) => {
                write!(f, "版本号 \"{v}\" 含前导零的段")
            }
        }
    }
}

impl std::error::Error for VersionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析三段式并保留数值() {
        assert_eq!(Version::parse("0.2.0"), Ok(Version::new(0, 2, 0)));
        assert_eq!(Version::parse(" 12.34.56 "), Ok(Version::new(12, 34, 56)));
    }

    #[test]
    fn 段数不对一律拒绝() {
        for text in ["1", "1.2", "1.2.3.4", "", "  "] {
            assert!(Version::parse(text).is_err(), "{text} 不该被接受");
        }
    }

    #[test]
    fn 非数字与前导零被拒绝() {
        assert!(Version::parse("1.2.x").is_err());
        assert!(Version::parse("1.-2.3").is_err());
        assert!(Version::parse("1.2.3-beta").is_err());
        assert!(Version::parse("01.2.3").is_err());
        assert_eq!(
            Version::parse("01.2.3"),
            Err(VersionError::LeadingZero("01.2.3".into()))
        );
    }

    #[test]
    fn 比较按数值而不是字符串() {
        // 字符串比较会把 "0.10.0" 排在 "0.9.0" 前面 —— 这是最经典的坑
        assert!(Version::parse("0.10.0").unwrap() > Version::parse("0.9.0").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
        assert_eq!(
            Version::parse("0.2.0").unwrap(),
            Version::parse("0.2.0").unwrap()
        );
    }

    #[test]
    fn 显示回来与原字符串一致() {
        for text in ["0.1.0", "1.0.0", "10.20.30"] {
            assert_eq!(Version::parse(text).unwrap().to_string(), text);
        }
    }
}
