//! Release-only CLI installation pin admission shared by the build and offline tests.

pub(crate) fn is_exact_pin(value: &str) -> bool {
    semver::Version::parse(value)
        .is_ok_and(|version| version.pre.is_empty() && version.build.is_empty())
}

/// Extract the unchanged binary version from an exact fork release tag.
pub(crate) fn upstream_version(tag: &str) -> Option<&str> {
    let (version, revision) = tag.strip_prefix("rust-v")?.split_once("-fork.")?;
    (is_exact_pin(version)
        && !revision.is_empty()
        && revision.bytes().all(|byte| byte.is_ascii_digit())
        && revision.parse::<u64>().is_ok_and(|number| number > 0))
    .then_some(version)
}
