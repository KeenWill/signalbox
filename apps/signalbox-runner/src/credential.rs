//! Dispatch-scoped runner credential resolution and output scrubbing.

use std::{
    error::Error,
    fmt::{self, Write as _},
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt as _,
};

use rustix::{
    fs::{CWD, Mode, OFlags, openat},
    process::geteuid,
};
use signalbox_runner_wire::ProfileName;
use signalbox_tools_exec::{
    InvalidSandboxEnvironmentVariable, MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES,
    SandboxEnvironmentVariable,
};

use crate::RunnerConfiguration;

const REDACTED: &str = "[redacted]";

/// A resolved value that exists only for one provisioning or tool dispatch.
///
/// The carrier deliberately implements neither `Clone`, `Display`, nor
/// serialization. Its diagnostic representation retains only non-secret
/// configuration facts.
#[derive(Eq, PartialEq)]
pub struct ResolvedRunnerCredential {
    profile: ProfileName,
    injection_env: String,
    value: String,
    escaped_value: String,
    solidus_escaped_value: String,
    unicode_escaped_value: String,
    solidus_unicode_escaped_value: String,
}

impl ResolvedRunnerCredential {
    /// Borrows the exact non-secret profile selected for this dispatch.
    pub const fn profile(&self) -> &ProfileName {
        &self.profile
    }

    /// Returns the exact checked environment name for the sandbox request.
    pub fn injection_env(&self) -> &str {
        &self.injection_env
    }

    /// Projects the value into the single restricted-process environment channel.
    pub fn sandbox_environment(
        &self,
    ) -> Result<SandboxEnvironmentVariable, RunnerCredentialResolutionError> {
        SandboxEnvironmentVariable::try_new(self.injection_env.clone(), self.value.clone()).map_err(
            |error| {
                RunnerCredentialResolutionError::new(
                    self.profile.clone(),
                    RunnerCredentialResolutionFailure::Unavailable,
                    Some(error),
                )
            },
        )
    }

    /// Scrubs the exact value and its JSON-string-escaped forms from captured text.
    pub fn redact_text(&self, text: String) -> String {
        let replacement = if REDACTED.contains(&self.value)
            || REDACTED.contains(&self.escaped_value)
            || REDACTED.contains(&self.solidus_escaped_value)
            || REDACTED.contains(&self.unicode_escaped_value)
            || REDACTED.contains(&self.solidus_unicode_escaped_value)
        {
            ""
        } else {
            REDACTED
        };
        let enumerated = text
            .replace(&self.solidus_unicode_escaped_value, replacement)
            .replace(&self.unicode_escaped_value, replacement)
            .replace(&self.solidus_escaped_value, replacement)
            .replace(&self.escaped_value, replacement)
            .replace(&self.value, replacement);
        let redacted = redact_json_decoded_spellings(&enumerated, &self.value, replacement);
        if self.contains_secret_spelling(&redacted)
            || contains_json_decoded_spelling(&redacted, &self.value)
        {
            String::new()
        } else {
            redacted
        }
    }

    fn contains_secret_spelling(&self, text: &str) -> bool {
        text.contains(&self.value)
            || text.contains(&self.escaped_value)
            || text.contains(&self.solidus_escaped_value)
            || text.contains(&self.unicode_escaped_value)
            || text.contains(&self.solidus_unicode_escaped_value)
    }
}

fn redact_json_decoded_spellings(text: &str, value: &str, replacement: &str) -> String {
    let (decoded, source_spans) = decode_json_escapes(text);
    let ranges = decoded
        .match_indices(value)
        .map(|(start, matched)| {
            let end = start + matched.len();
            (source_spans[start].0, source_spans[end - 1].1)
        })
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return text.to_owned();
    }

    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in ranges {
        if start < cursor {
            continue;
        }
        redacted.push_str(&text[cursor..start]);
        redacted.push_str(replacement);
        cursor = end;
    }
    redacted.push_str(&text[cursor..]);
    redacted
}

fn contains_json_decoded_spelling(text: &str, value: &str) -> bool {
    decode_json_escapes(text).0.contains(value)
}

fn decode_json_escapes(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut decoded = String::with_capacity(text.len());
    let mut source_spans = Vec::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let (character, source_end) = decoded_json_escape(text, offset).unwrap_or_else(|| {
            let character = text[offset..]
                .chars()
                .next()
                .expect("the offset is inside the string");
            (character, offset + character.len_utf8())
        });
        decoded.push(character);
        source_spans.extend(std::iter::repeat_n(
            (offset, source_end),
            character.len_utf8(),
        ));
        offset = source_end;
    }
    (decoded, source_spans)
}

fn decoded_json_escape(text: &str, offset: usize) -> Option<(char, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(offset) != Some(&b'\\') {
        return None;
    }
    let escaped = *bytes.get(offset + 1)?;
    let simple = match escaped {
        b'\"' => Some('\"'),
        b'\\' => Some('\\'),
        b'/' => Some('/'),
        b'b' => Some('\u{0008}'),
        b'f' => Some('\u{000c}'),
        b'n' => Some('\n'),
        b'r' => Some('\r'),
        b't' => Some('\t'),
        b'u' => None,
        _ => return None,
    };
    if let Some(character) = simple {
        return Some((character, offset + 2));
    }

    let first = parse_json_hex_quad(bytes.get(offset + 2..offset + 6)?)?;
    if (0xd800..=0xdbff).contains(&first) {
        if bytes.get(offset + 6..offset + 8) != Some(&b"\\u"[..]) {
            return None;
        }
        let second = parse_json_hex_quad(bytes.get(offset + 8..offset + 12)?)?;
        if !(0xdc00..=0xdfff).contains(&second) {
            return None;
        }
        let scalar = 0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
        return char::from_u32(scalar).map(|character| (character, offset + 12));
    }
    if (0xdc00..=0xdfff).contains(&first) {
        return None;
    }
    char::from_u32(u32::from(first)).map(|character| (character, offset + 6))
}

fn parse_json_hex_quad(bytes: &[u8]) -> Option<u16> {
    if bytes.len() != 4 {
        return None;
    }
    bytes.iter().try_fold(0_u16, |value, byte| {
        char::from(*byte)
            .to_digit(16)
            .map(|digit| (value << 4) | digit as u16)
    })
}

impl fmt::Debug for ResolvedRunnerCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedRunnerCredential")
            .field("profile", &self.profile)
            .field("injection_env", &self.injection_env)
            .field("value", &"<redacted>")
            .field("escaped_value", &"<redacted>")
            .field("solidus_escaped_value", &"<redacted>")
            .field("unicode_escaped_value", &"<redacted>")
            .field("solidus_unicode_escaped_value", &"<redacted>")
            .finish()
    }
}

/// Stable reason why one selected runner credential could not be prepared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerCredentialResolutionFailure {
    /// The selected non-secret profile is absent from checked runner configuration.
    UnknownProfile,
    /// The current credential artifact or its value is unavailable.
    Unavailable,
}

/// Sanitized failure to resolve one selected profile for one physical dispatch.
#[derive(Debug)]
pub struct RunnerCredentialResolutionError {
    profile: ProfileName,
    failure: RunnerCredentialResolutionFailure,
    source: Option<InvalidSandboxEnvironmentVariable>,
}

impl RunnerCredentialResolutionError {
    fn new(
        profile: ProfileName,
        failure: RunnerCredentialResolutionFailure,
        source: Option<InvalidSandboxEnvironmentVariable>,
    ) -> Self {
        Self {
            profile,
            failure,
            source,
        }
    }

    /// Borrows the exact non-secret profile whose resolution failed.
    pub const fn profile(&self) -> &ProfileName {
        &self.profile
    }

    /// Returns the stable sanitized failure class.
    pub const fn failure(&self) -> RunnerCredentialResolutionFailure {
        self.failure
    }
}

impl fmt::Display for RunnerCredentialResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "runner credential profile `{}` could not be resolved: {:?}",
            self.profile.as_str(),
            self.failure
        )
    }
}

impl Error for RunnerCredentialResolutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

/// Resolves the exact selected profile immediately before one physical dispatch.
pub fn resolve_runner_credential(
    configuration: &RunnerConfiguration,
    profile: &ProfileName,
) -> Result<ResolvedRunnerCredential, RunnerCredentialResolutionError> {
    let configured = configuration.credential(profile).ok_or_else(|| {
        RunnerCredentialResolutionError::new(
            profile.clone(),
            RunnerCredentialResolutionFailure::UnknownProfile,
            None,
        )
    })?;
    let descriptor = openat(
        CWD,
        configured.file(),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| unavailable(profile))?;
    let mut file = File::from(descriptor);
    let metadata = file.metadata().map_err(|_| unavailable(profile))?;
    let facts = CredentialFileFacts {
        regular: metadata.is_file(),
        user_id: metadata.uid(),
        mode: metadata.mode() & 0o7777,
        bytes: metadata.len(),
    };
    if !credential_file_facts_are_valid(facts, geteuid().as_raw()) {
        return Err(unavailable(profile));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable(profile))?;
    if bytes.len() > MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES {
        return Err(unavailable(profile));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    let value = String::from_utf8(bytes).map_err(|_| unavailable(profile))?;
    if value.is_empty() || value.contains('\0') {
        return Err(unavailable(profile));
    }
    let encoded = serde_json::to_string(&value).map_err(|_| unavailable(profile))?;
    let encoded_end = encoded
        .len()
        .checked_sub(1)
        .ok_or_else(|| unavailable(profile))?;
    let escaped_value = encoded
        .get(1..encoded_end)
        .ok_or_else(|| unavailable(profile))?
        .to_owned();
    let solidus_escaped_value = escaped_value.replace('/', "\\/");
    let unicode_escaped_value = escape_json_non_ascii(&escaped_value);
    let solidus_unicode_escaped_value = unicode_escaped_value.replace('/', "\\/");
    Ok(ResolvedRunnerCredential {
        profile: profile.clone(),
        injection_env: configured.injection_env().to_owned(),
        value,
        escaped_value,
        solidus_escaped_value,
        unicode_escaped_value,
        solidus_unicode_escaped_value,
    })
}

fn escape_json_non_ascii(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii() {
            escaped.push(character);
        } else {
            let mut units = [0; 2];
            for unit in character.encode_utf16(&mut units) {
                write!(escaped, "\\u{unit:04x}").expect("writing to a string cannot fail");
            }
        }
    }
    escaped
}

fn unavailable(profile: &ProfileName) -> RunnerCredentialResolutionError {
    RunnerCredentialResolutionError::new(
        profile.clone(),
        RunnerCredentialResolutionFailure::Unavailable,
        None,
    )
}

#[derive(Clone, Copy)]
struct CredentialFileFacts {
    regular: bool,
    user_id: u32,
    mode: u32,
    bytes: u64,
}

fn credential_file_facts_are_valid(facts: CredentialFileFacts, effective_user: u32) -> bool {
    facts.regular
        && facts.user_id == effective_user
        && facts.mode == 0o600
        && facts.bytes <= MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES as u64
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt as _, symlink},
        path::Path,
    };

    use tempfile::TempDir;

    use super::*;

    const PROFILE: &str = "github-runner";
    const ENVIRONMENT: &str = "GH_TOKEN";
    const FIRST_VALUE: &str = "synthetic-first-credential";
    const SECOND_VALUE: &str = "synthetic-second-credential";

    fn profile() -> ProfileName {
        ProfileName::try_new(PROFILE.to_owned()).expect("the fixture profile is valid")
    }

    fn configuration(credential_file: &Path) -> RunnerConfiguration {
        let document = format!(
            r#"
version = 1
daemon_socket_path = "/run/user/1000/signalbox-runner.sock"
runner_root = "/var/lib/signalbox-runner"
exec_supervisor_executable = "/usr/local/bin/signalbox-exec-supervisor"
bubblewrap_path = "/usr/bin/bwrap"
read_only_paths = ["/usr"]
allowed_network_hosts = []
git_author_name = "Signalbox Runner"
git_author_email = "runner@example.invalid"
repositories = {{}}

[credentials.{PROFILE}]
file = "{}"
injection_env = "{ENVIRONMENT}"
"#,
            credential_file.display()
        );
        RunnerConfiguration::parse(&document).expect("the synthetic configuration is valid")
    }

    fn write_credential(path: &Path, value: &[u8]) {
        fs::write(path, value).expect("the synthetic credential is written");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("the synthetic credential has exact user-only mode");
    }

    fn unavailable_error(path: &Path) -> RunnerCredentialResolutionError {
        resolve_runner_credential(&configuration(path), &profile())
            .expect_err("the invalid synthetic credential must be unavailable")
    }

    /// INV-035: the dispatch-scoped value is injected exactly and all diagnostics redact it.
    #[test]
    fn inv_035_resolved_value_projects_exactly_and_redacts_raw_and_json_forms() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let value = "synthetic-\"credential\nline";
        write_credential(&path, format!("{value}\r\n").as_bytes());
        let selected = profile();

        let resolved = resolve_runner_credential(&configuration(&path), &selected)
            .expect("the exact synthetic credential resolves");
        let environment = resolved
            .sandbox_environment()
            .expect("the checked value projects to the restricted environment");
        let expected_environment =
            SandboxEnvironmentVariable::try_new(ENVIRONMENT.to_owned(), value.to_owned())
                .expect("the expected synthetic environment is valid");
        let captured = format!(r#"raw={value}; json=synthetic-\"credential\nline"#);

        assert_eq!(resolved.profile(), &selected);
        assert_eq!(resolved.injection_env(), ENVIRONMENT);
        assert_eq!(environment, expected_environment);
        assert_eq!(
            resolved.redact_text(captured),
            "raw=[redacted]; json=[redacted]"
        );
        assert_eq!(
            format!("{resolved:?}"),
            "ResolvedRunnerCredential { profile: ProfileName(\"github-runner\"), injection_env: \"GH_TOKEN\", value: \"<redacted>\", escaped_value: \"<redacted>\", solidus_escaped_value: \"<redacted>\", unicode_escaped_value: \"<redacted>\", solidus_unicode_escaped_value: \"<redacted>\" }"
        );
    }

    /// INV-035: ASCII-only JSON Unicode escapes cannot expose the credential.
    #[test]
    fn inv_035_redaction_scrubs_the_unicode_escaped_json_form() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let value = "synthetic-é-😀";
        write_credential(&path, value.as_bytes());
        let resolved = resolve_runner_credential(&configuration(&path), &profile())
            .expect("the synthetic credential resolves");

        assert_eq!(
            resolved.redact_text(r"json=synthetic-\u00e9-\ud83d\ude00".to_owned()),
            "json=[redacted]"
        );
    }

    /// INV-035: arbitrary JSON Unicode escapes cannot expose an ASCII value.
    #[test]
    fn inv_035_redaction_scrubs_an_ascii_unicode_escape() {
        let directory = TempDir::new().expect("a synthetic value directory exists");
        let path = directory.path().join("value");
        let value = "alpha-beta";
        write_credential(&path, value.as_bytes());
        let resolved = resolve_runner_credential(&configuration(&path), &profile())
            .expect("the synthetic value resolves");

        assert_eq!(
            resolved.redact_text(r"json=alpha-\u0062eta".to_owned()),
            "json=[redacted]"
        );
    }

    /// INV-035: valid optional JSON solidus escapes cannot expose the credential.
    #[test]
    fn inv_035_redaction_scrubs_the_solidus_escaped_json_form() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let value = "synthetic/token";
        write_credential(&path, value.as_bytes());
        let resolved = resolve_runner_credential(&configuration(&path), &profile())
            .expect("the synthetic credential resolves");

        assert_eq!(
            resolved.redact_text(r"json=synthetic\/token".to_owned()),
            "json=[redacted]"
        );
    }

    /// INV-035: a short credential present in the usual marker is removed completely.
    #[test]
    fn inv_035_redaction_marker_cannot_reproduce_a_short_credential() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let value = "a";
        write_credential(&path, value.as_bytes());
        let resolved = resolve_runner_credential(&configuration(&path), &profile())
            .expect("the short synthetic credential resolves");

        assert_eq!(resolved.redact_text(value.to_owned()), "");
    }

    /// INV-035: a credential equal to the usual marker is removed completely.
    #[test]
    fn inv_035_redaction_marker_cannot_reproduce_an_equal_credential() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let value = REDACTED;
        write_credential(&path, value.as_bytes());
        let resolved = resolve_runner_credential(&configuration(&path), &profile())
            .expect("the marker-equal synthetic credential resolves");

        assert_eq!(resolved.redact_text(value.to_owned()), "");
    }

    /// INV-035: overlapping matches cannot leave a credential after redaction.
    #[test]
    fn inv_035_redaction_rechecks_the_final_output() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let value = "red";
        write_credential(&path, value.as_bytes());
        let resolved = resolve_runner_credential(&configuration(&path), &profile())
            .expect("the synthetic credential resolves");

        assert_eq!(resolved.redact_text("rrrredededed".to_owned()), "");
    }

    /// INV-035: every physical resolution reopens the configured path so rotation is visible.
    #[test]
    fn inv_035_atomic_replacement_rotates_the_dispatch_scoped_value() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let replacement = directory.path().join("replacement");
        write_credential(&path, FIRST_VALUE.as_bytes());
        let configuration = configuration(&path);
        let selected = profile();
        let first = resolve_runner_credential(&configuration, &selected)
            .expect("the first synthetic credential resolves");
        write_credential(&replacement, SECOND_VALUE.as_bytes());
        fs::rename(&replacement, &path).expect("the synthetic credential rotates atomically");

        let second = resolve_runner_credential(&configuration, &selected)
            .expect("the rotated synthetic credential resolves");

        assert_eq!(first.redact_text(FIRST_VALUE.to_owned()), "[redacted]");
        assert_eq!(second.redact_text(SECOND_VALUE.to_owned()), "[redacted]");
        assert_eq!(second.redact_text(FIRST_VALUE.to_owned()), FIRST_VALUE);
    }

    #[test]
    fn unknown_profile_fails_before_opening_any_credential_path() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        write_credential(&path, FIRST_VALUE.as_bytes());
        let unknown = ProfileName::try_new("other-profile".to_owned())
            .expect("the unknown fixture profile is valid");

        let error = resolve_runner_credential(&configuration(&path), &unknown)
            .expect_err("an unconfigured profile fails closed");

        assert_eq!(error.profile(), &unknown);
        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::UnknownProfile
        );
        assert_eq!(
            error.to_string(),
            "runner credential profile `other-profile` could not be resolved: UnknownProfile"
        );
    }

    #[test]
    fn symlink_replacement_is_unavailable() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let target = directory.path().join("target");
        let alias = directory.path().join("credential");
        write_credential(&target, FIRST_VALUE.as_bytes());
        symlink(&target, &alias).expect("the synthetic symlink exists");

        let error = unavailable_error(&alias);

        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::Unavailable
        );
    }

    #[test]
    fn wrong_mode_is_unavailable() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        fs::write(&path, FIRST_VALUE).expect("the synthetic credential is written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("the synthetic credential has a deliberately wrong mode");

        let error = unavailable_error(&path);

        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::Unavailable
        );
    }

    #[test]
    fn nonregular_file_is_unavailable() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o600))
            .expect("the synthetic directory has the nominal credential mode");

        let error = unavailable_error(directory.path());

        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::Unavailable
        );
    }

    #[test]
    fn exact_byte_bound_is_accepted() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        let value = "s".repeat(MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES);
        write_credential(&path, value.as_bytes());

        let resolved = resolve_runner_credential(&configuration(&path), &profile())
            .expect("the exact credential bound is valid");
        let environment = resolved
            .sandbox_environment()
            .expect("the exact bound projects to the sandbox environment");
        let expected = SandboxEnvironmentVariable::try_new(ENVIRONMENT.to_owned(), value)
            .expect("the exact environment bound is valid");

        assert_eq!(environment, expected);
    }

    #[test]
    fn oversized_value_is_unavailable() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        write_credential(
            &path,
            "s".repeat(MAX_SANDBOX_ENVIRONMENT_VALUE_BYTES + 1)
                .as_bytes(),
        );

        let error = unavailable_error(&path);

        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::Unavailable
        );
    }

    #[test]
    fn empty_value_after_line_ending_trim_is_unavailable() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        write_credential(&path, b"\r\n\n");

        let error = unavailable_error(&path);

        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::Unavailable
        );
    }

    #[test]
    fn nul_value_is_unavailable() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        write_credential(&path, b"synthetic\0credential");

        let error = unavailable_error(&path);

        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::Unavailable
        );
    }

    #[test]
    fn non_utf8_value_is_unavailable() {
        let directory = TempDir::new().expect("a synthetic credential directory exists");
        let path = directory.path().join("credential");
        write_credential(&path, &[b's', b'y', b'n', 0xff]);

        let error = unavailable_error(&path);

        assert_eq!(
            error.failure(),
            RunnerCredentialResolutionFailure::Unavailable
        );
    }

    #[test]
    fn metadata_facts_reject_a_wrong_user_id() {
        let facts = CredentialFileFacts {
            regular: true,
            user_id: 41,
            mode: 0o600,
            bytes: 32,
        };

        assert!(!credential_file_facts_are_valid(facts, 42));
    }
}
