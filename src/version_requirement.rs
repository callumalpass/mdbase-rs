//! Portable contract version requirements (mdbase spec Chapter 05A).
//!
//! The grammar is a subset of npm `semver` ranges: an exact version, `^` and
//! `~` ranges, and space-separated comparator sets. Pre-release versions
//! satisfy a requirement whenever they fall inside its bounds, and build
//! metadata is ignored.

use std::cmp::Ordering;

use semver::{BuildMetadata, Prerelease, Version};

#[derive(Debug, Clone, Copy)]
enum Operator {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// Whether `version` satisfies `requirement`.
#[cfg(test)]
pub(crate) fn satisfies(version: &Version, requirement: &str) -> Result<bool, String> {
    let version = without_build(version);
    Ok(bounds(requirement)?
        .iter()
        .all(|(operator, bound)| compare(&version, *operator, bound)))
}

/// The highest candidate version that satisfies `requirement`.
pub(crate) fn resolve<'a>(
    requirement: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Result<Option<&'a str>, String> {
    let bounds = bounds(requirement)?;
    Ok(candidates
        .into_iter()
        .filter_map(|candidate| Some((candidate, Version::parse(candidate).ok()?)))
        .filter(|(_, version)| {
            let version = without_build(version);
            bounds
                .iter()
                .all(|(operator, bound)| compare(&version, *operator, bound))
        })
        .max_by(|(_, left), (_, right)| without_build(left).cmp(&without_build(right)))
        .map(|(candidate, _)| candidate))
}

fn bounds(requirement: &str) -> Result<Vec<(Operator, Version)>, String> {
    let invalid = || format!("'{requirement}' is not a portable version requirement");
    if let Some(base) = requirement.strip_prefix('^') {
        let lower = parse(base).ok_or_else(invalid)?;
        let upper = if lower.major > 0 {
            floor(lower.major + 1, 0, 0)
        } else if lower.minor > 0 {
            floor(0, lower.minor + 1, 0)
        } else {
            floor(0, 0, lower.patch + 1)
        };
        return Ok(vec![(Operator::Gte, lower), (Operator::Lt, upper)]);
    }
    if let Some(base) = requirement.strip_prefix('~') {
        let lower = parse(base).ok_or_else(invalid)?;
        let upper = floor(lower.major, lower.minor + 1, 0);
        return Ok(vec![(Operator::Gte, lower), (Operator::Lt, upper)]);
    }
    requirement
        .split(' ')
        .map(|comparator| {
            let (operator, version) = [
                (">=", Operator::Gte),
                ("<=", Operator::Lte),
                (">", Operator::Gt),
                ("<", Operator::Lt),
                ("=", Operator::Eq),
            ]
            .iter()
            .find_map(|(prefix, operator)| {
                comparator
                    .strip_prefix(prefix)
                    .map(|rest| (*operator, rest))
            })
            .unwrap_or((Operator::Eq, comparator));
            Ok((operator, parse(version).ok_or_else(invalid)?))
        })
        .collect()
}

fn parse(value: &str) -> Option<Version> {
    Version::parse(value)
        .ok()
        .map(|version| without_build(&version))
}

/// The lowest version with this core, `major.minor.patch-0`.
fn floor(major: u64, minor: u64, patch: u64) -> Version {
    let mut version = Version::new(major, minor, patch);
    version.pre = Prerelease::new("0").expect("`0` is a valid pre-release");
    version
}

fn without_build(version: &Version) -> Version {
    let mut version = version.clone();
    version.build = BuildMetadata::EMPTY;
    version
}

fn compare(version: &Version, operator: Operator, bound: &Version) -> bool {
    let ordering = version.cmp(bound);
    match operator {
        Operator::Eq => ordering == Ordering::Equal,
        Operator::Gt => ordering == Ordering::Greater,
        Operator::Gte => ordering != Ordering::Less,
        Operator::Lt => ordering == Ordering::Less,
        Operator::Lte => ordering != Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_requirements_follow_the_spec_bounds() {
        let cases = [
            ("1.0.0", "1.0.0", true),
            ("1.0.1", "=1.0.0", false),
            ("1.9.0", "^1.0.0", true),
            ("2.0.0", "^1.0.0", false),
            ("2.0.0-rc.1", "^1.0.0", false),
            ("1.5.0-rc.1", "^1.0.0", true),
            ("0.4.9", "^0.4.2", true),
            ("0.5.0", "^0.4.2", false),
            ("0.0.4", "^0.0.4", true),
            ("0.0.5", "^0.0.4", false),
            ("1.4.9", "~1.4.2", true),
            ("1.5.0", "~1.4.2", false),
            ("1.0.0-rc.2", "^1.0.0-rc.1", true),
            ("1.0.0-rc.1", "^1.0.0-rc.2", false),
            ("1.3.0", ">=1.2.0 <2.0.0", true),
            ("2.0.0", ">=1.2.0 <2.0.0", false),
            ("1.0.0+build", "1.0.0", true),
        ];
        for (version, requirement, expected) in cases {
            let version = Version::parse(version).unwrap();
            assert_eq!(
                satisfies(&version, requirement).unwrap(),
                expected,
                "{version} {requirement}"
            );
        }
    }

    #[test]
    fn resolution_picks_the_highest_satisfying_version() {
        let versions = ["1.0.0", "1.4.0", "2.0.0", "1.4.1-rc.1"];
        assert_eq!(resolve("^1.0.0", versions).unwrap(), Some("1.4.1-rc.1"));
        assert_eq!(resolve("~1.0.0", versions).unwrap(), Some("1.0.0"));
        assert_eq!(resolve("^3.0.0", versions).unwrap(), None);
        assert!(resolve("1.x", versions).is_err());
    }
}
