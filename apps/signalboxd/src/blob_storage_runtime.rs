//! Startup composition for the durable blob-store registry.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use signalbox_blob_store::{BlobStore, BlobStoreName};
use signalbox_blob_store_filesystem::{
    FilesystemBlobStaging, FilesystemBlobStore, FilesystemBlobStoreConstructionError,
    FilesystemNamespaceIdentity, NamespaceBindingState, OpenedFilesystemBlobRoot,
};
use signalbox_persistence::blob::{
    BlobCatalogRepository, BlobCatalogRepositoryError, BlobStoreBindingRecord,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{BlobStorageClass, BlobStorageConfiguration};

/// Configured stores, semantic write routes, and private upload staging.
pub struct BlobStoreRegistry {
    stores: BTreeMap<BlobStoreName, Arc<dyn BlobStore>>,
    namespace_ids: BTreeMap<BlobStoreName, Uuid>,
    routes: BTreeMap<BlobStorageClass, BlobStoreName>,
    staging: FilesystemBlobStaging,
    max_blob_bytes: u64,
}

impl fmt::Debug for BlobStoreRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlobStoreRegistry")
            .field("store_count", &self.stores.len())
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
        Self::initialize_with_locality_policy(configuration, pool, true).await
    }

    /// Initializes a composed conformance fixture without relying on the test
    /// host's backing-device classification.
    pub async fn initialize_for_conformance(
        configuration: Option<&BlobStorageConfiguration>,
        pool: PgPool,
    ) -> Result<Option<Self>, BlobStoreRegistryError> {
        #[cfg(feature = "test-support")]
        {
            Self::initialize_with_locality_policy(configuration, pool, false).await
        }
        #[cfg(not(feature = "test-support"))]
        {
            Self::initialize_with_locality_policy(configuration, pool, true).await
        }
    }

    async fn initialize_with_locality_policy(
        configuration: Option<&BlobStorageConfiguration>,
        pool: PgPool,
        require_local_backing: bool,
    ) -> Result<Option<Self>, BlobStoreRegistryError> {
        let repository = BlobCatalogRepository::new(pool);
        let recorded = repository.recorded_store_bindings().await?;
        let Some(configuration) = configuration else {
            if repository.is_empty().await? {
                return Ok(None);
            }
            return Err(BlobStoreRegistryError::ConfigurationRequired);
        };
        validate_recorded_bindings(configuration, &recorded)?;
        if configuration
            .stores()
            .any(|(_, store)| store.s3().is_some())
        {
            return Err(BlobStoreRegistryError::UnsupportedStoreKind);
        }

        let staging_root = open_blob_root(
            configuration.staging_directory().to_path_buf(),
            require_local_backing,
        )?;
        let mut opened_stores = Vec::new();
        for (name, configured) in configuration.stores() {
            let root = configured
                .filesystem_root()
                .ok_or(BlobStoreRegistryError::UnsupportedStoreKind)?;
            let state = if recorded.iter().any(|binding| binding.store() == name) {
                NamespaceBindingState::Recorded
            } else {
                NamespaceBindingState::New
            };
            let opened = open_blob_root(root.to_path_buf(), require_local_backing)?;
            opened_stores.push((name.clone(), configured.namespace_id(), state, opened));
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
            let (store, _) = FilesystemBlobStore::from_opened_bound(opened, namespace_id, state)?;
            namespace_ids.insert(name.clone(), namespace_id);
            stores.insert(name, Arc::new(store));
        }

        for (name, configured) in configuration.stores() {
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
            routes,
            staging,
            max_blob_bytes: configuration.max_blob_bytes(),
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

    /// Returns the deployment ceiling for one stored object.
    pub const fn max_blob_bytes(&self) -> u64 {
        self.max_blob_bytes
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

fn open_blob_root(
    root: PathBuf,
    require_local_backing: bool,
) -> Result<OpenedFilesystemBlobRoot, FilesystemBlobStoreConstructionError> {
    if require_local_backing {
        return OpenedFilesystemBlobRoot::open(root);
    }
    #[cfg(feature = "test-support")]
    {
        OpenedFilesystemBlobRoot::open_without_locality_check_for_test(root)
    }
    #[cfg(not(feature = "test-support"))]
    OpenedFilesystemBlobRoot::open(root)
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

fn validate_recorded_bindings(
    configuration: &BlobStorageConfiguration,
    recorded: &[BlobStoreBindingRecord],
) -> Result<(), BlobStoreRegistryError> {
    for binding in recorded {
        match configuration.namespace_id(binding.store()) {
            Some(namespace_id) if namespace_id == binding.namespace_id() => {}
            Some(_) => return Err(BlobStoreRegistryError::RecordedNamespaceMismatch),
            None => return Err(BlobStoreRegistryError::RecordedStoreMissing),
        }
    }
    Ok(())
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

/// Startup could not prove that configuration resolves durable blob placement.
#[derive(Debug)]
pub enum BlobStoreRegistryError {
    /// Durable blob facts exist while the optional configuration is absent.
    ConfigurationRequired,
    /// A recorded store name is absent from current configuration.
    RecordedStoreMissing,
    /// A recorded store name is paired with another namespace UUID.
    RecordedNamespaceMismatch,
    /// This child does not yet compose the declared adapter kind.
    UnsupportedStoreKind,
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
            Self::RecordedStoreMissing => "blob storage configuration omits a recorded store",
            Self::RecordedNamespaceMismatch => {
                "blob storage configuration disagrees with a recorded namespace"
            }
            Self::UnsupportedStoreKind => {
                "blob storage configuration names an adapter not composed by this child"
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
            | Self::RecordedStoreMissing
            | Self::RecordedNamespaceMismatch
            | Self::UnsupportedStoreKind
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
        BlobStoreRegistryError, OpenedNamespace, paths_overlap, validate_physical_namespaces,
        validate_recorded_bindings,
    };
    use crate::BlobStorageConfiguration;
    use signalbox_blob_store::BlobStoreName;
    use signalbox_persistence::blob::BlobStoreBindingRecord;
    use std::{error::Error, io, path::Path, str::FromStr};
    use toml_edit::DocumentMut;
    use uuid::Uuid;

    const CONFIGURATION: &str = r#"
[blob_storage]
version = 1
staging_directory = "/staging"
max_blob_bytes = 2
[[blob_storage.stores]]
name = "primary"
namespace_id = "5a100001-0000-4000-8000-000000000001"
kind = "filesystem"
root_directory = "/stores/primary"
[blob_storage.routes]
user_attachment = "primary"
tool_artifact = "primary"
imported_source = "primary"
generated_artifact = "primary"
"#;

    fn configuration() -> Result<BlobStorageConfiguration, Box<dyn Error>> {
        let document = DocumentMut::from_str(CONFIGURATION)?;
        BlobStorageConfiguration::parse(document.get("blob_storage"), 1)?
            .ok_or_else(|| io::Error::other("the fixture enables blob storage").into())
    }

    fn binding(namespace: &str) -> Result<BlobStoreBindingRecord, Box<dyn Error>> {
        Ok(BlobStoreBindingRecord::new(
            BlobStoreName::try_new("primary")?,
            Uuid::parse_str(namespace)?,
        ))
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
    fn recorded_binding_requires_the_configured_namespace() {
        let recorded = binding("5a100001-0000-4000-8000-000000000002")
            .expect("the recorded binding fixture is valid");

        let configured = configuration().expect("the configuration fixture is valid");
        let error = validate_recorded_bindings(&configured, &[recorded])
            .expect_err("a namespace disagreement fails startup");

        assert!(matches!(
            error,
            BlobStoreRegistryError::RecordedNamespaceMismatch
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
