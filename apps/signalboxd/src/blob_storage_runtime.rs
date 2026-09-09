//! Startup composition for the durable blob-store registry.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use signalbox_blob_store::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreError, BlobStoreFuture,
    BlobStoreName, ExpectedBlob, OpenedBlob,
};
use signalbox_blob_store_filesystem::{
    FilesystemBlobStaging, FilesystemBlobStore, FilesystemBlobStoreConstructionError,
    FilesystemNamespaceIdentity, NamespaceBindingState, OpenedFilesystemBlobRoot,
};
use signalbox_blob_store_s3::{S3BlobStore, S3NamespaceBindingState};
use signalbox_persistence::blob::{
    BlobCatalogRepository, BlobCatalogRepositoryError, BlobStoreBindingRecord,
};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{BlobStorageClass, BlobStorageConfiguration};

const S3_STARTUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5 * 60);
pub(crate) const MAX_CONCURRENT_BLOB_READS: usize = 16;

/// Configured stores, semantic write routes, and private upload staging.
pub struct BlobStoreRegistry {
    stores: BTreeMap<BlobStoreName, Arc<dyn BlobStore>>,
    namespace_ids: BTreeMap<BlobStoreName, Uuid>,
    unavailable: BTreeMap<BlobStoreName, &'static str>,
    routes: BTreeMap<BlobStorageClass, BlobStoreName>,
    staging: FilesystemBlobStaging,
    max_blob_bytes: u64,
    read_budget: Arc<Semaphore>,
}

impl fmt::Debug for BlobStoreRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlobStoreRegistry")
            .field("store_count", &self.stores.len())
            .field("unavailable_store_count", &self.unavailable.len())
            .field("route_count", &self.routes.len())
            .field("max_blob_bytes", &self.max_blob_bytes)
            .finish_non_exhaustive()
    }
}

impl BlobStoreRegistry {
    /// Initializes backend namespaces after recovery and before socket admission.
    pub async fn initialize(
        configuration: Option<&BlobStorageConfiguration>,
        pool: PgPool,
    ) -> Result<Option<Self>, BlobStoreRegistryError> {
        let repository = BlobCatalogRepository::new(pool);
        let recorded = repository.recorded_store_bindings().await?;
        let Some(configuration) = configuration else {
            if repository.is_empty().await? {
                return Ok(None);
            }
            return Err(BlobStoreRegistryError::ConfigurationRequired);
        };
        validate_s3_locators(configuration)?;

        let staging_root =
            OpenedFilesystemBlobRoot::open(configuration.staging_directory().to_path_buf())?;
        let mut opened_stores = Vec::new();
        let mut s3_stores = Vec::new();
        let mut bindings_to_register = BTreeSet::new();
        let mut unavailable = BTreeMap::new();
        let recorded_by_name = recorded
            .iter()
            .map(|binding| (binding.store().clone(), binding.namespace_id()))
            .collect::<BTreeMap<_, _>>();
        for (name, configured) in configuration.stores() {
            let recorded_namespace = recorded_by_name.get(name).copied();
            let recorded_binding = recorded_namespace.is_some();
            if recorded_namespace.is_some_and(|namespace| namespace != configured.namespace_id()) {
                unavailable.insert(name.clone(), "recorded_namespace_mismatch");
                continue;
            }
            let routed = is_routed(configuration, name);
            if let Some(root) = configured.filesystem_root() {
                let state = if recorded_binding {
                    NamespaceBindingState::Recorded
                } else {
                    NamespaceBindingState::New
                };
                match OpenedFilesystemBlobRoot::open(root.to_path_buf()) {
                    Ok(opened) => {
                        opened_stores.push((
                            name.clone(),
                            configured.namespace_id(),
                            state,
                            opened,
                        ));
                    }
                    Err(_) => {
                        unavailable.insert(name.clone(), "filesystem_namespace_unavailable");
                    }
                }
            } else {
                let (endpoint, region, bucket, credentials_file) = configured
                    .s3()
                    .ok_or(BlobStoreRegistryError::InvalidStoreConfiguration)?;
                let store = match S3BlobStore::try_new_bound(
                    endpoint.clone(),
                    region,
                    bucket,
                    credentials_file.to_path_buf(),
                    format!("{}\n", configured.namespace_id()),
                ) {
                    Ok(store) => store,
                    Err(_) => {
                        unavailable.insert(name.clone(), "s3_configuration_unavailable");
                        continue;
                    }
                };
                s3_stores.push((
                    name.clone(),
                    configured.namespace_id(),
                    recorded_binding,
                    routed,
                    store,
                ));
            }
        }
        let staging_identity = OpenedNamespace::from(staging_root.identity());
        let identities = opened_stores
            .iter()
            .map(|(_, _, _, opened)| OpenedNamespace::from(opened.identity()))
            .collect::<Vec<_>>();
        validate_physical_namespaces(&staging_identity, &identities)?;

        let mut stores = BTreeMap::<BlobStoreName, Arc<dyn BlobStore>>::new();
        let mut namespace_ids = BTreeMap::new();
        for (name, namespace_id, state, opened) in opened_stores {
            match FilesystemBlobStore::from_opened_bound(opened, namespace_id, state) {
                Ok((store, _)) => {
                    bindings_to_register.insert(name.clone());
                    stores.insert(name.clone(), Arc::new(store));
                }
                Err(_) => {
                    unavailable.insert(name.clone(), "filesystem_namespace_unavailable");
                }
            }
            namespace_ids.insert(name.clone(), namespace_id);
        }

        let s3_deadline = tokio::time::Instant::now() + S3_STARTUP_DEADLINE;
        for (name, namespace_id, recorded_binding, routed, store) in s3_stores {
            if routed {
                let state = if recorded_binding {
                    S3NamespaceBindingState::Recorded
                } else {
                    S3NamespaceBindingState::New
                };
                match tokio::time::timeout_at(s3_deadline, Box::pin(store.prepare_namespace(state)))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        unavailable.insert(name.clone(), "s3_namespace_unavailable");
                        namespace_ids.insert(name, namespace_id);
                        continue;
                    }
                    Err(_) => {
                        unavailable.insert(name.clone(), "s3_startup_deadline");
                        namespace_ids.insert(name, namespace_id);
                        continue;
                    }
                }
            }
            if routed || recorded_binding {
                bindings_to_register.insert(name.clone());
            }
            namespace_ids.insert(name.clone(), namespace_id);
            stores.insert(name, Arc::new(store));
        }

        for binding in &recorded {
            if configuration.namespace_id(binding.store()).is_none() {
                unavailable.insert(binding.store().clone(), "recorded_store_missing");
                namespace_ids.insert(binding.store().clone(), binding.namespace_id());
            }
        }
        for (name, cause) in &unavailable {
            stores.insert(name.clone(), Arc::new(UnavailableBlobStore { cause }));
            if !namespace_ids.contains_key(name)
                && let Some(namespace_id) = recorded_by_name
                    .get(name)
                    .copied()
                    .or_else(|| configuration.namespace_id(name))
            {
                namespace_ids.insert(name.clone(), namespace_id);
            }
            tracing::warn!(store = %name, cause_code = cause, "blob store is unavailable");
        }

        for (name, configured) in configuration.stores() {
            if !bindings_to_register.contains(name) {
                continue;
            }
            repository
                .register_store_binding(BlobStoreBindingRecord::new(
                    name.clone(),
                    configured.namespace_id(),
                ))
                .await?;
        }

        // No fallible asynchronous work follows staging preparation. If the
        // caller cancels initialization after losing the singleton guard,
        // there is therefore no armed staging value hidden in this future.
        let staging = FilesystemBlobStaging::from_opened(staging_root)?;

        let routes = [
            BlobStorageClass::UserAttachment,
            BlobStorageClass::ToolArtifact,
            BlobStorageClass::ImportedSource,
            BlobStorageClass::GeneratedArtifact,
        ]
        .into_iter()
        .map(|class| (class, configuration.route(class).0.clone()))
        .collect();
        Ok(Some(Self {
            stores,
            namespace_ids,
            unavailable,
            routes,
            staging,
            max_blob_bytes: configuration.max_blob_bytes(),
            read_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_READS)),
        }))
    }

    /// Resolves the adapter selected for one new semantic use.
    pub fn routed_store(&self, class: BlobStorageClass) -> (&BlobStoreName, Arc<dyn BlobStore>) {
        let name = &self.routes[&class];
        (name, self.stores[name].clone())
    }

    /// Returns the durable namespace identity bound to one configured store.
    pub fn namespace_id(&self, name: &BlobStoreName) -> Uuid {
        self.namespace_ids[name]
    }

    /// Resolves one already-recorded durable store identity.
    pub fn recorded_store(&self, name: &BlobStoreName) -> Option<Arc<dyn BlobStore>> {
        self.stores.get(name).cloned()
    }

    /// Lists stores that startup retained as typed unavailable adapters.
    pub fn unavailable_stores(&self) -> impl Iterator<Item = (&BlobStoreName, &'static str)> {
        self.unavailable.iter().map(|(name, cause)| (name, *cause))
    }

    /// Replaces one configured adapter in a composed conformance fixture.
    #[cfg(feature = "test-support")]
    pub fn replace_store_for_conformance(
        &mut self,
        name: &BlobStoreName,
        replacement: Arc<dyn BlobStore>,
    ) -> bool {
        self.stores.insert(name.clone(), replacement).is_some()
    }

    /// Returns the deployment ceiling for one stored object.
    pub const fn max_blob_bytes(&self) -> u64 {
        self.max_blob_bytes
    }

    /// Shares the deployment-wide bound for store-backed read traversals.
    pub fn read_budget(&self) -> Arc<Semaphore> {
        Arc::clone(&self.read_budget)
    }

    /// Returns the private upload staging namespace.
    pub const fn staging(&self) -> &FilesystemBlobStaging {
        &self.staging
    }

    /// Removes proven upload spools and makes a successful sweep final.
    pub fn sweep_staging(&self) -> io::Result<()> {
        let result = self.staging.sweep();
        if result.is_ok() {
            self.staging.disarm_sweep_on_drop();
        }
        result
    }

    /// Prevents staging cleanup after the database singleton guard is lost.
    pub fn disarm_staging_sweep(&self) {
        self.staging.disarm_sweep_on_drop();
    }
}

#[derive(Debug)]
struct UnavailableBlobStore {
    cause: &'static str,
}

impl BlobStore for UnavailableBlobStore {
    fn put<'a>(
        &'a self,
        _expected: ExpectedBlob,
        _source: BlobReader,
    ) -> BlobStoreFuture<'a, BlobPutOutcome> {
        Box::pin(async move { Err(BlobStoreError::unavailable(self.cause)) })
    }

    fn open<'a>(&'a self, _key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(async move { Err(BlobStoreError::unavailable(self.cause)) })
    }

    fn open_verified<'a>(
        &'a self,
        _expected: ExpectedBlob,
        _key: &'a BlobObjectKey,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(async move { Err(BlobStoreError::unavailable(self.cause)) })
    }

    fn open_range<'a>(
        &'a self,
        _expected: ExpectedBlob,
        _key: &'a BlobObjectKey,
        _offset: u64,
        _byte_length: std::num::NonZeroU64,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(async move { Err(BlobStoreError::unavailable(self.cause)) })
    }
}
/// Reports whether any semantic write route currently selects this store.
///
/// The parsed route map is the authority, so a store named only by a storage
/// class added later still authenticates its namespace at startup.
fn is_routed(configuration: &BlobStorageConfiguration, name: &BlobStoreName) -> bool {
    configuration.routed_stores().any(|routed| routed == name)
}

fn validate_s3_locators(
    configuration: &BlobStorageConfiguration,
) -> Result<(), BlobStoreRegistryError> {
    let mut locators = BTreeSet::new();
    for (_, store) in configuration.stores() {
        let Some((endpoint, _, bucket, _)) = store.s3() else {
            continue;
        };
        if !locators.insert((endpoint.as_str(), bucket)) {
            return Err(BlobStoreRegistryError::PhysicalNamespaceAlias);
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct OpenedNamespace {
    canonical_path: PathBuf,
    device_inode: (u64, u64),
    physical_path: PathBuf,
}

impl From<&FilesystemNamespaceIdentity> for OpenedNamespace {
    fn from(identity: &FilesystemNamespaceIdentity) -> Self {
        Self {
            canonical_path: identity.canonical_path().to_path_buf(),
            device_inode: identity.device_inode(),
            physical_path: identity.physical_path().to_path_buf(),
        }
    }
}

fn validate_physical_namespaces(
    staging: &OpenedNamespace,
    stores: &[OpenedNamespace],
) -> Result<(), BlobStoreRegistryError> {
    for (index, left) in stores.iter().enumerate() {
        if paths_overlap(&staging.canonical_path, &left.canonical_path)
            || staging.device_inode == left.device_inode
            || physical_paths_overlap(staging, left)
        {
            return Err(BlobStoreRegistryError::StagingStoreOverlap);
        }
        for right in &stores[index + 1..] {
            if left.canonical_path == right.canonical_path
                || left.device_inode == right.device_inode
            {
                return Err(BlobStoreRegistryError::PhysicalNamespaceAlias);
            }
            if paths_overlap(&left.canonical_path, &right.canonical_path) {
                return Err(BlobStoreRegistryError::NestedStoreRoots);
            }
            if physical_paths_overlap(left, right) {
                return Err(BlobStoreRegistryError::NestedStoreRoots);
            }
        }
    }
    Ok(())
}

fn physical_paths_overlap(left: &OpenedNamespace, right: &OpenedNamespace) -> bool {
    left.device_inode.0 == right.device_inode.0
        && paths_overlap(&left.physical_path, &right.physical_path)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

/// Startup could not construct the durable blob-store registry.
#[derive(Debug)]
pub enum BlobStoreRegistryError {
    /// Durable blob facts exist while the optional configuration is absent.
    ConfigurationRequired,
    /// A parsed store entry could not be narrowed to one supported kind.
    InvalidStoreConfiguration,
    /// Two configured names resolve to one physical filesystem namespace.
    PhysicalNamespaceAlias,
    /// One filesystem store root contains another.
    NestedStoreRoots,
    /// Upload staging equals, contains, or is contained by a store root.
    StagingStoreOverlap,
    /// The durable catalog could not be loaded or reconciled.
    Catalog(BlobCatalogRepositoryError),
    /// A filesystem namespace could not be authenticated or prepared.
    Filesystem(FilesystemBlobStoreConstructionError),
}

impl fmt::Display for BlobStoreRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ConfigurationRequired => {
                "blob storage configuration is required by the durable catalog"
            }
            Self::InvalidStoreConfiguration => {
                "blob storage configuration has no supported adapter kind"
            }
            Self::PhysicalNamespaceAlias => "blob store names resolve to one physical namespace",
            Self::NestedStoreRoots => "blob filesystem store roots overlap",
            Self::StagingStoreOverlap => "blob staging and store roots overlap",
            Self::Catalog(_) => "blob catalog startup reconciliation failed",
            Self::Filesystem(_) => "blob filesystem startup reconciliation failed",
        })
    }
}

impl Error for BlobStoreRegistryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(source) => Some(source),
            Self::Filesystem(source) => Some(source),
            Self::ConfigurationRequired
            | Self::InvalidStoreConfiguration
            | Self::PhysicalNamespaceAlias
            | Self::NestedStoreRoots
            | Self::StagingStoreOverlap => None,
        }
    }
}

impl From<BlobCatalogRepositoryError> for BlobStoreRegistryError {
    fn from(source: BlobCatalogRepositoryError) -> Self {
        Self::Catalog(source)
    }
}

impl From<FilesystemBlobStoreConstructionError> for BlobStoreRegistryError {
    fn from(source: FilesystemBlobStoreConstructionError) -> Self {
        Self::Filesystem(source)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlobStoreRegistryError, OpenedNamespace, UnavailableBlobStore, paths_overlap,
        validate_physical_namespaces, validate_s3_locators,
    };
    use crate::BlobStorageConfiguration;
    use signalbox_blob_store::{BlobObjectKey, BlobStore, BlobStoreFailureKind};
    use std::{error::Error, io, path::Path, str::FromStr};
    use toml_edit::DocumentMut;

    const ALIASED_S3_CONFIGURATION: &str = r#"
[blob_storage]
version = 1
staging_directory = "/staging"
max_blob_bytes = 2
[[blob_storage.stores]]
name = "primary"
namespace_id = "5a100001-0000-4000-8000-000000000001"
kind = "s3"
endpoint = "https://objects.example.test"
region = "fixture-region"
bucket = "fixture-bucket"
credentials_file = "/run/credentials/s3-primary"
[[blob_storage.stores]]
name = "secondary"
namespace_id = "5a100001-0000-4000-8000-000000000002"
kind = "s3"
endpoint = "https://objects.example.test:443/"
region = "another-region"
bucket = "fixture-bucket"
credentials_file = "/run/credentials/s3-secondary"
[blob_storage.routes]
user_attachment = "primary"
tool_artifact = "primary"
imported_source = "primary"
generated_artifact = "primary"
"#;

    fn aliased_s3_configuration() -> Result<BlobStorageConfiguration, Box<dyn Error>> {
        let document = DocumentMut::from_str(ALIASED_S3_CONFIGURATION)?;
        BlobStorageConfiguration::parse(document.get("blob_storage"), 1)?
            .ok_or_else(|| io::Error::other("the fixture enables blob storage").into())
    }

    fn namespace(path: &str, device: u64, inode: u64) -> OpenedNamespace {
        OpenedNamespace {
            canonical_path: path.into(),
            device_inode: (device, inode),
            physical_path: path.into(),
        }
    }

    fn mounted_namespace(
        path: &str,
        device: u64,
        inode: u64,
        physical_path: &str,
    ) -> OpenedNamespace {
        OpenedNamespace {
            canonical_path: path.into(),
            device_inode: (device, inode),
            physical_path: physical_path.into(),
        }
    }

    #[test]
    fn canonical_s3_locator_rejects_default_port_aliases() -> Result<(), Box<dyn Error>> {
        let configuration = aliased_s3_configuration()?;

        assert!(matches!(
            validate_s3_locators(&configuration),
            Err(BlobStoreRegistryError::PhysicalNamespaceAlias)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_store_returns_the_typed_store_failure() {
        let store = UnavailableBlobStore {
            cause: "filesystem_namespace_unavailable",
        };
        let key =
            BlobObjectKey::try_from_recorded("sha256/aa/aa/value").expect("fixture key is safe");

        let error = store
            .open(&key)
            .await
            .expect_err("the placeholder never reaches its backing store");

        assert_eq!(error.kind(), BlobStoreFailureKind::Unavailable);
    }

    #[test]
    fn path_overlap_is_symmetric() {
        assert!(paths_overlap(Path::new("/blob"), Path::new("/blob/child")));
        assert!(paths_overlap(Path::new("/blob/child"), Path::new("/blob")));
    }

    #[test]
    fn path_overlap_observes_component_boundaries() {
        assert!(!paths_overlap(
            Path::new("/blob"),
            Path::new("/blob-sibling")
        ));
    }

    #[test]
    fn physical_namespace_alias_fails_startup() {
        let staging = namespace("/staging", 1, 1);
        let primary = namespace("/stores/primary", 2, 2);
        let alias = namespace("/stores/alias", 2, 2);

        let error = validate_physical_namespaces(&staging, &[primary, alias])
            .expect_err("one inode cannot represent two stores");

        assert!(matches!(
            error,
            BlobStoreRegistryError::PhysicalNamespaceAlias
        ));
    }

    #[test]
    fn bind_mounted_descendant_fails_before_namespace_preparation() {
        let staging = mounted_namespace("/staging", 1, 8, "/store/.publish-v1");
        let store = mounted_namespace("/store", 1, 4, "/store");

        let error = validate_physical_namespaces(&staging, &[store])
            .expect_err("a bind-mounted descendant overlaps the store namespace");

        assert!(matches!(error, BlobStoreRegistryError::StagingStoreOverlap));
    }

    #[test]
    fn nested_store_roots_fail_startup() {
        let staging = namespace("/staging", 1, 1);
        let primary = namespace("/stores", 2, 2);
        let nested = namespace("/stores/nested", 2, 3);

        let error = validate_physical_namespaces(&staging, &[primary, nested])
            .expect_err("one store cannot contain another");

        assert!(matches!(error, BlobStoreRegistryError::NestedStoreRoots));
    }

    #[test]
    fn staging_overlap_fails_startup() {
        let staging = namespace("/stores/primary/staging", 1, 1);
        let primary = namespace("/stores/primary", 2, 2);

        let error = validate_physical_namespaces(&staging, &[primary])
            .expect_err("staging cannot live beneath a store root");

        assert!(matches!(error, BlobStoreRegistryError::StagingStoreOverlap));
    }
}
