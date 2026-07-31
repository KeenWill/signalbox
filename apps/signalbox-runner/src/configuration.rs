//! Strict version-one startup configuration.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use signalbox_runner_wire::{Advertisement, ProfileName, RepositoryKey, ValueError};
use url::Url;

const CONFIGURATION_VERSION: u64 = 1;

/// A checked source for the runner configuration document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerConfigurationPath(PathBuf);

impl RunnerConfigurationPath {
    /// Resolves exactly one `--config PATH` or environment-provided path.
    pub fn resolve(
        arguments: impl IntoIterator<Item = OsString>,
        environment: Option<OsString>,
    ) -> Result<Self, ArgumentError> {
        let mut arguments = arguments.into_iter();
        let first = arguments.next();
        match (first, environment) {
            (None, None) => Err(ArgumentError::MissingConfiguration),
            (None, Some(path)) => Self::from_environment(path),
            (Some(_), Some(_)) => Err(ArgumentError::ConflictingSources),
            (Some(option), None) if option == OsStr::new("--config") => {
                let path = arguments.next().ok_or(ArgumentError::MissingOptionValue)?;
                if arguments.next().is_some() {
                    return Err(ArgumentError::UnexpectedArgument);
                }
                Self::from_argument(path)
            }
            (Some(_), None) => Err(ArgumentError::UnexpectedArgument),
        }
    }

    fn from_environment(path: OsString) -> Result<Self, ArgumentError> {
        if path.is_empty() {
            Err(ArgumentError::EmptyEnvironmentPath)
        } else {
            Ok(Self(PathBuf::from(path)))
        }
    }

    fn from_argument(path: OsString) -> Result<Self, ArgumentError> {
        if path.is_empty() {
            Err(ArgumentError::EmptyArgumentPath)
        } else {
            Ok(Self(PathBuf::from(path)))
        }
    }

    /// Borrows the selected path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Sanitized command-line configuration-source failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgumentError {
    /// Neither supported source supplied a path.
    MissingConfiguration,
    /// Both supported sources supplied a path.
    ConflictingSources,
    /// `--config` lacked its required value.
    MissingOptionValue,
    /// The environment supplied an empty path.
    EmptyEnvironmentPath,
    /// `--config` supplied an empty path.
    EmptyArgumentPath,
    /// An unknown option, positional argument, or surplus argument was present.
    UnexpectedArgument,
}

impl fmt::Display for ArgumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingConfiguration => "exactly one runner configuration source is required",
            Self::ConflictingSources => {
                "runner configuration cannot come from both supported sources"
            }
            Self::MissingOptionValue => "--config requires one nonempty path",
            Self::EmptyEnvironmentPath => "SIGNALBOX_RUNNER_CONFIG_FILE must not be empty",
            Self::EmptyArgumentPath => "--config requires one nonempty path",
            Self::UnexpectedArgument => "runner command line contains an unsupported argument",
        })
    }
}

impl Error for ArgumentError {}

/// Complete immutable startup configuration for the registration-only runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerConfiguration {
    daemon_socket_path: PathBuf,
    runner_root: PathBuf,
    bubblewrap_path: PathBuf,
    read_only_paths: Vec<PathBuf>,
    allowed_network_hosts: Vec<AllowedNetworkHost>,
    git_author_name: String,
    git_author_email: String,
    credentials: BTreeMap<ProfileName, RunnerCredentialConfiguration>,
    repositories: BTreeMap<RepositoryKey, RunnerRepositoryConfiguration>,
    advertisement: Advertisement,
}

/// Non-secret structure for one runner-resident credential provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerCredentialConfiguration {
    file: PathBuf,
    injection_env: String,
}

impl RunnerCredentialConfiguration {
    /// Borrows the configured credential-file path; the file is not read at startup.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Returns the exact environment name used only inside an admitted dispatch.
    pub fn injection_env(&self) -> &str {
        &self.injection_env
    }
}

/// Non-secret checked clone and optional-profile structure for one repository key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerRepositoryConfiguration {
    clone_url: String,
    credential_profile: Option<ProfileName>,
}

impl RunnerRepositoryConfiguration {
    /// Returns the exact validated credential-free GitHub HTTPS clone URL.
    pub fn clone_url(&self) -> &str {
        &self.clone_url
    }

    /// Borrows the exact optional configured credential requirement.
    pub const fn credential_profile(&self) -> Option<&ProfileName> {
        self.credential_profile.as_ref()
    }
}

impl RunnerConfiguration {
    /// Reads and validates one strict version-one TOML document.
    pub fn read(path: &Path) -> Result<Self, RunnerConfigurationError> {
        let content = fs::read_to_string(path).map_err(RunnerConfigurationError::Read)?;
        Self::parse(&content)
    }

    /// Parses one complete strict version-one TOML document.
    pub fn parse(content: &str) -> Result<Self, RunnerConfigurationError> {
        let raw: RawConfiguration =
            toml::from_str(content).map_err(|_| RunnerConfigurationError::InvalidDocument)?;
        if raw.version != CONFIGURATION_VERSION {
            return Err(RunnerConfigurationError::UnsupportedVersion(raw.version));
        }
        if !valid_absolute_path(&raw.daemon_socket_path) {
            return Err(RunnerConfigurationError::InvalidDaemonSocketPath);
        }
        if !valid_absolute_path(&raw.runner_root) {
            return Err(RunnerConfigurationError::InvalidRunnerRoot);
        }
        if !valid_absolute_path(&raw.bubblewrap_path) {
            return Err(RunnerConfigurationError::InvalidBubblewrapPath);
        }
        validate_read_only_paths(&raw.read_only_paths)?;
        validate_network_hosts(&raw.allowed_network_hosts)?;
        validate_git_author(&raw.git_author_name, &raw.git_author_email)?;
        let credentials = validate_credentials(&raw.credentials)?;
        let repositories =
            validate_repositories(&raw.repositories, &credentials, &raw.allowed_network_hosts)?;

        // Every inventory is constructed independently from an actual compiled
        // provider. This registration-only milestone compiles no workspace,
        // sandbox, tool, or capability-class provider. Credential and repository
        // availability is the exact validated configuration projection.
        let advertisement = Advertisement {
            capability_classes: Vec::new(),
            tools: Vec::new(),
            workspace_capabilities: Vec::new(),
            sandbox_profiles: Vec::new(),
            credential_profiles: credentials.keys().cloned().collect(),
            repositories: repositories
                .iter()
                .map(|(key, repository)| signalbox_runner_wire::RepositoryEntry {
                    key: key.clone(),
                    credential_profile: repository.credential_profile.clone(),
                })
                .collect(),
        };
        advertisement
            .validate()
            .map_err(RunnerConfigurationError::InvalidAdvertisement)?;

        Ok(Self {
            daemon_socket_path: raw.daemon_socket_path,
            runner_root: raw.runner_root,
            bubblewrap_path: raw.bubblewrap_path,
            read_only_paths: raw.read_only_paths,
            allowed_network_hosts: raw.allowed_network_hosts,
            git_author_name: raw.git_author_name,
            git_author_email: raw.git_author_email,
            credentials,
            repositories,
            advertisement,
        })
    }

    /// Borrows the configured dedicated hub socket path.
    pub fn daemon_socket_path(&self) -> &Path {
        &self.daemon_socket_path
    }

    /// Borrows the configured owner-private durable root.
    pub fn runner_root(&self) -> &Path {
        &self.runner_root
    }

    /// Borrows the configured absolute bubblewrap executable path.
    pub fn bubblewrap_path(&self) -> &Path {
        &self.bubblewrap_path
    }

    /// Borrows the explicit read-only path inventory.
    pub fn read_only_paths(&self) -> &[PathBuf] {
        &self.read_only_paths
    }

    /// Borrows the explicit closed network-host inventory.
    pub fn allowed_network_hosts(&self) -> &[AllowedNetworkHost] {
        &self.allowed_network_hosts
    }

    /// Returns the checked Git author name.
    pub fn git_author_name(&self) -> &str {
        &self.git_author_name
    }

    /// Returns the checked Git author email.
    pub fn git_author_email(&self) -> &str {
        &self.git_author_email
    }

    /// Resolves one advertised credential name to its non-secret provider configuration.
    pub fn credential(&self, profile: &ProfileName) -> Option<&RunnerCredentialConfiguration> {
        self.credentials.get(profile)
    }

    /// Resolves one advertised repository key to its exact checked configuration.
    pub fn repository(&self, key: &RepositoryKey) -> Option<&RunnerRepositoryConfiguration> {
        self.repositories.get(key)
    }

    /// Borrows the complete explicit six-inventory advertisement.
    pub const fn advertisement(&self) -> &Advertisement {
        &self.advertisement
    }
}

fn valid_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name().is_some_and(|name| {
            !name.is_empty() && name != OsStr::new(".") && name != OsStr::new("..")
        })
}

fn validate_read_only_paths(paths: &[PathBuf]) -> Result<(), RunnerConfigurationError> {
    if paths.is_empty() || paths.iter().any(|path| !valid_absolute_path(path)) {
        return Err(RunnerConfigurationError::InvalidReadOnlyPaths);
    }
    let unique: BTreeSet<&PathBuf> = paths.iter().collect();
    if unique.len() != paths.len() {
        return Err(RunnerConfigurationError::InvalidReadOnlyPaths);
    }
    Ok(())
}

fn validate_network_hosts(hosts: &[AllowedNetworkHost]) -> Result<(), RunnerConfigurationError> {
    let unique: BTreeSet<AllowedNetworkHost> = hosts.iter().copied().collect();
    if unique.len() != hosts.len() {
        Err(RunnerConfigurationError::InvalidNetworkHosts)
    } else {
        Ok(())
    }
}

fn validate_git_author(name: &str, email: &str) -> Result<(), RunnerConfigurationError> {
    if name.is_empty()
        || email.is_empty()
        || name.contains('\0')
        || email.contains('\0')
        || name.trim() != name
        || email.trim() != email
    {
        Err(RunnerConfigurationError::InvalidGitAuthor)
    } else {
        Ok(())
    }
}

fn validate_credentials(
    credentials: &BTreeMap<String, RawCredential>,
) -> Result<BTreeMap<ProfileName, RunnerCredentialConfiguration>, RunnerConfigurationError> {
    let mut profiles = BTreeMap::new();
    for (name, credential) in credentials {
        let profile = ProfileName::try_new(name.clone())
            .map_err(|_| RunnerConfigurationError::InvalidCredentials)?;
        if !valid_absolute_path(&credential.file)
            || !valid_environment_name(&credential.injection_env)
        {
            return Err(RunnerConfigurationError::InvalidCredentials);
        }
        let configured = RunnerCredentialConfiguration {
            file: credential.file.clone(),
            injection_env: credential.injection_env.clone(),
        };
        if profiles.insert(profile, configured).is_some() {
            return Err(RunnerConfigurationError::InvalidCredentials);
        }
    }
    Ok(profiles)
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_repositories(
    repositories: &BTreeMap<String, RawRepository>,
    profiles: &BTreeMap<ProfileName, RunnerCredentialConfiguration>,
    allowed_network_hosts: &[AllowedNetworkHost],
) -> Result<BTreeMap<RepositoryKey, RunnerRepositoryConfiguration>, RunnerConfigurationError> {
    let mut configured = BTreeMap::new();
    for (name, repository) in repositories {
        let key = RepositoryKey::try_new(name.clone())
            .map_err(|_| RunnerConfigurationError::InvalidRepositories)?;
        validate_clone_url(&repository.clone_url)?;
        if repository
            .credential_profile
            .as_ref()
            .is_some_and(|profile| !profiles.contains_key(profile))
        {
            return Err(RunnerConfigurationError::InvalidRepositories);
        }
        let value = RunnerRepositoryConfiguration {
            clone_url: repository.clone_url.clone(),
            credential_profile: repository.credential_profile.clone(),
        };
        if configured.insert(key, value).is_some() {
            return Err(RunnerConfigurationError::InvalidRepositories);
        }
    }
    if !repositories.is_empty() && !allowed_network_hosts.contains(&AllowedNetworkHost::GithubCom) {
        return Err(RunnerConfigurationError::InvalidRepositories);
    }
    Ok(configured)
}

fn validate_clone_url(value: &str) -> Result<(), RunnerConfigurationError> {
    let parsed = Url::parse(value).map_err(|_| RunnerConfigurationError::InvalidRepositories)?;
    let components = parsed
        .path_segments()
        .ok_or(RunnerConfigurationError::InvalidRepositories)?
        .collect::<Vec<_>>();
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || value.contains('%')
        || components.len() != 2
        || components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return Err(RunnerConfigurationError::InvalidRepositories);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfiguration {
    version: u64,
    daemon_socket_path: PathBuf,
    runner_root: PathBuf,
    bubblewrap_path: PathBuf,
    read_only_paths: Vec<PathBuf>,
    allowed_network_hosts: Vec<AllowedNetworkHost>,
    git_author_name: String,
    git_author_email: String,
    repositories: BTreeMap<String, RawRepository>,
    credentials: BTreeMap<String, RawCredential>,
}

/// Closed configured network-host inventory.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
pub enum AllowedNetworkHost {
    #[serde(rename = "github.com")]
    GithubCom,
    #[serde(rename = "crates.io")]
    CratesIo,
    #[serde(rename = "api.anthropic.com")]
    ApiAnthropicCom,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepository {
    clone_url: String,
    credential_profile: Option<ProfileName>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredential {
    file: PathBuf,
    injection_env: String,
}

/// Sanitized strict runner-configuration failure.
#[derive(Debug)]
pub enum RunnerConfigurationError {
    /// The configuration file could not be read.
    Read(io::Error),
    /// TOML shape, type, duplicate, or field validation failed.
    InvalidDocument,
    /// The required root version was not version one.
    UnsupportedVersion(u64),
    /// The daemon socket was not an absolute path with a final component.
    InvalidDaemonSocketPath,
    /// The state root was not an absolute path with a final component.
    InvalidRunnerRoot,
    /// The bubblewrap executable path was not absolute.
    InvalidBubblewrapPath,
    /// The read-only path inventory was empty, nonabsolute, or duplicated.
    InvalidReadOnlyPaths,
    /// The closed network-host inventory contained a duplicate.
    InvalidNetworkHosts,
    /// Git author text was empty, padded, or contained NUL.
    InvalidGitAuthor,
    /// A credential profile name, file, or injection environment was invalid.
    InvalidCredentials,
    /// A repository key, clone URL, profile relation, or host policy was invalid.
    InvalidRepositories,
    /// The explicit advertisement was malformed.
    InvalidAdvertisement(ValueError),
}

impl fmt::Display for RunnerConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(_) => formatter.write_str("runner configuration could not be read"),
            Self::InvalidDocument => {
                formatter.write_str("runner configuration document is invalid")
            }
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "runner configuration version {version} is unsupported"
                )
            }
            Self::InvalidDaemonSocketPath => {
                formatter.write_str("runner daemon socket path is invalid")
            }
            Self::InvalidRunnerRoot => formatter.write_str("runner state root is invalid"),
            Self::InvalidBubblewrapPath => formatter.write_str("runner bubblewrap path is invalid"),
            Self::InvalidReadOnlyPaths => {
                formatter.write_str("runner read-only path inventory is invalid")
            }
            Self::InvalidNetworkHosts => {
                formatter.write_str("runner network-host inventory is invalid")
            }
            Self::InvalidGitAuthor => formatter.write_str("runner Git author is invalid"),
            Self::InvalidCredentials => {
                formatter.write_str("runner credential configuration is invalid")
            }
            Self::InvalidRepositories => {
                formatter.write_str("runner repository configuration is invalid")
            }
            Self::InvalidAdvertisement(_) => {
                formatter.write_str("runner advertisement configuration is invalid")
            }
        }
    }
}

impl Error for RunnerConfigurationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::InvalidAdvertisement(error) => Some(error),
            Self::InvalidDocument
            | Self::UnsupportedVersion(_)
            | Self::InvalidDaemonSocketPath
            | Self::InvalidRunnerRoot
            | Self::InvalidBubblewrapPath
            | Self::InvalidReadOnlyPaths
            | Self::InvalidNetworkHosts
            | Self::InvalidGitAuthor
            | Self::InvalidCredentials
            | Self::InvalidRepositories => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_CONFIGURATION: &str = r#"
version = 1
daemon_socket_path = "/run/user/1000/signalbox-runner.sock"
runner_root = "/var/lib/signalbox-runner"
bubblewrap_path = "/usr/bin/bwrap"
read_only_paths = ["/usr"]
allowed_network_hosts = []
git_author_name = "Signalbox Runner"
git_author_email = "runner@example.invalid"
repositories = {}
credentials = {}
"#;
    const CONFIGURED_PROFILE: &str = "github-runner";
    const CONFIGURED_REPOSITORY: &str = "signalbox";
    const CONFIGURED_CLONE_URL: &str = "https://github.com/KeenWill/signalbox.git";
    const CONFIGURED_CREDENTIAL_FILE: &str = "/run/secrets/github-token";
    const CONFIGURED_INJECTION_ENV: &str = "GH_TOKEN";

    #[test]
    fn configuration_preserves_all_six_explicit_empty_inventories() {
        let configuration = RunnerConfiguration::parse(EMPTY_CONFIGURATION)
            .expect("the explicit empty capability configuration is valid");

        assert_eq!(
            configuration.advertisement(),
            &Advertisement {
                capability_classes: Vec::new(),
                tools: Vec::new(),
                workspace_capabilities: Vec::new(),
                sandbox_profiles: Vec::new(),
                credential_profiles: Vec::new(),
                repositories: Vec::new(),
            }
        );
    }

    #[test]
    fn configuration_rejects_unknown_root_field() {
        let document = EMPTY_CONFIGURATION.replace("version = 1", "version = 1\nextra = 1");

        let error = RunnerConfiguration::parse(&document)
            .expect_err("an unknown root field must fail closed");

        assert_eq!(
            error.to_string(),
            "runner configuration document is invalid"
        );
    }

    #[test]
    fn configuration_round_trips_exact_credential_and_repository_advertisement_axes() {
        let document = EMPTY_CONFIGURATION
            .replace(
                "allowed_network_hosts = []",
                "allowed_network_hosts = [\"github.com\"]",
            )
            .replace(
                "repositories = {}\ncredentials = {}",
                r#"[repositories.signalbox]
clone_url = "https://github.com/KeenWill/signalbox.git"
credential_profile = "github-runner"

[credentials.github-runner]
file = "/run/secrets/github-token"
injection_env = "GH_TOKEN""#,
            );
        let profile = ProfileName::try_new(CONFIGURED_PROFILE.to_owned())
            .expect("the configured profile name is valid");
        let repository = RepositoryKey::try_new(CONFIGURED_REPOSITORY.to_owned())
            .expect("the configured repository key is valid");

        let configuration = RunnerConfiguration::parse(&document)
            .expect("the configured credential and repository are valid");

        assert_eq!(
            configuration.advertisement(),
            &Advertisement {
                capability_classes: Vec::new(),
                tools: Vec::new(),
                workspace_capabilities: Vec::new(),
                sandbox_profiles: Vec::new(),
                credential_profiles: vec![profile.clone()],
                repositories: vec![signalbox_runner_wire::RepositoryEntry {
                    key: repository.clone(),
                    credential_profile: Some(profile.clone()),
                }],
            }
        );
        assert_eq!(
            configuration
                .credential(&profile)
                .expect("the advertised credential resolves")
                .file(),
            Path::new(CONFIGURED_CREDENTIAL_FILE)
        );
        assert_eq!(
            configuration
                .credential(&profile)
                .expect("the advertised credential resolves")
                .injection_env(),
            CONFIGURED_INJECTION_ENV
        );
        assert_eq!(
            configuration
                .repository(&repository)
                .expect("the advertised repository resolves")
                .clone_url(),
            CONFIGURED_CLONE_URL
        );
        assert_eq!(
            configuration
                .repository(&repository)
                .expect("the advertised repository resolves")
                .credential_profile(),
            Some(&profile)
        );
    }

    #[test]
    fn argument_path_requires_exactly_one_source() {
        let error = RunnerConfigurationPath::resolve(
            [OsString::from("--config"), OsString::from("runner.toml")],
            Some(OsString::from("runner-env.toml")),
        )
        .expect_err("two configuration sources must fail closed");

        assert_eq!(error, ArgumentError::ConflictingSources);
    }
}
