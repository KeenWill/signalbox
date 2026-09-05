//! Release-only CLI installation pin admission shared by the build and offline tests.

pub(crate) fn is_exact_pin(value: &str) -> bool {
    semver::Version::parse(value)
        .is_ok_and(|version| version.pre.is_empty() && version.build.is_empty())
}
