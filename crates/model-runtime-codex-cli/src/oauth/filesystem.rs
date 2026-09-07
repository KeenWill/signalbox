use super::OauthCredentialMaterial;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, mkdirat, openat, statat, unlinkat};
use std::{
    ffi::CString,
    io::{self, Write},
    os::fd::OwnedFd,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const HOME_PREFIX: &str = "oauth-";
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// One descriptor-owned private root, scavenged before model work is accepted.
pub struct OauthCredentialRoot {
    directory: OwnedFd,
    path: PathBuf,
}

impl std::fmt::Debug for OauthCredentialRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OauthCredentialRoot")
    }
}

pub(crate) struct OauthCredentialHome {
    pub material: OauthCredentialMaterial,
    pub path: PathBuf,
    root: Arc<OauthCredentialRoot>,
    name: CString,
}

impl OauthCredentialRoot {
    /// Opens or creates an absolute private root without following symlinks, then scavenges owned homes.
    pub fn open(path: &Path) -> io::Result<Arc<Self>> {
        if !path.is_absolute() {
            return Err(invalid());
        }
        let mut directory = openat(rustix::fs::CWD, "/", DIRECTORY_FLAGS, Mode::empty())?;
        let components = path.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    if index + 1 == components.len() {
                        match mkdirat(&directory, *name, Mode::from_raw_mode(0o700)) {
                            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    directory = openat(&directory, *name, DIRECTORY_FLAGS, Mode::empty())?;
                }
                _ => return Err(invalid()),
            }
        }
        owned(&directory, 0o700)?;
        let root = Arc::new(Self {
            directory,
            path: path.to_owned(),
        });
        let entries = names(&root.directory)?;
        for name in &entries {
            if !name.as_bytes().starts_with(HOME_PREFIX.as_bytes()) {
                return Err(invalid());
            }
            let home = openat(&root.directory, name, DIRECTORY_FLAGS, Mode::empty())?;
            validate_tree(&home)?;
        }
        for name in entries {
            root.remove(&name)?;
        }
        Ok(root)
    }

    pub(super) fn install(
        self: &Arc<Self>,
        material: OauthCredentialMaterial,
    ) -> io::Result<OauthCredentialHome> {
        let access =
            std::str::from_utf8(material.access_token.expose_bytes()).map_err(|_| invalid())?;
        let identity =
            std::str::from_utf8(material.identity_token.expose_bytes()).map_err(|_| invalid())?;
        if access.is_empty() || identity.is_empty() {
            return Err(invalid());
        }
        let contents = serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgptAuthTokens",
            "tokens": { "access_token": access, "id_token": identity, "refresh_token": "", "account_id": material.account_id },
        })).map_err(|_| invalid())?;
        let name = loop {
            let name = CString::new(format!(
                "{HOME_PREFIX}{}-{}",
                std::process::id(),
                HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ))
            .map_err(|_| invalid())?;
            match mkdirat(&self.directory, &name, Mode::from_raw_mode(0o700)) {
                Ok(()) => break name,
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(error.into()),
            }
        };
        // The prepared object owns both exact-value redaction seeds before any token is written.
        let home = OauthCredentialHome {
            path: self.path.join(name.to_str().map_err(|_| invalid())?),
            material,
            root: self.clone(),
            name,
        };
        let directory = openat(&self.directory, &home.name, DIRECTORY_FLAGS, Mode::empty())?;
        owned(&directory, 0o700)?;
        let file = openat(
            &directory,
            "auth.json",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        owned(&file, 0o600)?;
        std::fs::File::from(file).write_all(&contents)?;
        Ok(home)
    }

    fn remove(&self, name: &CString) -> io::Result<()> {
        let directory = openat(&self.directory, name, DIRECTORY_FLAGS, Mode::empty())?;
        validate_tree(&directory)?;
        remove_contents(&directory)?;
        unlinkat(&self.directory, name, AtFlags::REMOVEDIR)?;
        Ok(())
    }
}

impl Drop for OauthCredentialHome {
    fn drop(&mut self) {
        let _ = self.root.remove(&self.name);
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "OAuth credential home ownership or shape is invalid",
    )
}

fn owned(fd: &OwnedFd, permissions: rustix::fs::RawMode) -> io::Result<()> {
    let stat = fstat(fd)?;
    if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o7777 != permissions {
        return Err(invalid());
    }
    Ok(())
}

fn names(directory: &OwnedFd) -> io::Result<Vec<CString>> {
    Dir::read_from(directory)?
        .filter_map(|entry| match entry {
            Ok(entry)
                if entry.file_name().to_bytes() == b"."
                    || entry.file_name().to_bytes() == b".." =>
            {
                None
            }
            Ok(entry) => Some(Ok(entry.file_name().to_owned())),
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}

fn validate_tree(directory: &OwnedFd) -> io::Result<()> {
    owned(directory, 0o700)?;
    for name in names(directory)? {
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                validate_tree(&openat(directory, &name, DIRECTORY_FLAGS, Mode::empty())?)?
            }
            FileType::RegularFile => {
                let file = openat(
                    directory,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                    Mode::empty(),
                )?;
                owned(&file, 0o600)?;
                if FileType::from_raw_mode(fstat(&file)?.st_mode) != FileType::RegularFile {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

fn remove_contents(directory: &OwnedFd) -> io::Result<()> {
    for name in names(directory)? {
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let child = openat(directory, &name, DIRECTORY_FLAGS, Mode::empty())?;
                remove_contents(&child)?;
                unlinkat(directory, &name, AtFlags::REMOVEDIR)?;
            }
            FileType::RegularFile => unlinkat(directory, &name, AtFlags::empty())?,
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_model_runtime::CredentialValue;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn material() -> OauthCredentialMaterial {
        OauthCredentialMaterial {
            access_token: CredentialValue::new(b"synthetic-access".to_vec()),
            identity_token: CredentialValue::new(b"synthetic-identity".to_vec()),
            account_id: Some("synthetic-account".into()),
        }
    }

    #[test]
    fn oauth_home_withholds_refresh_token_and_removes_material_on_drop() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = OauthCredentialRoot::open(&temporary.path().join("root"))?;
        let home = root.install(material())?;
        let path = home.path.clone();
        assert_eq!(path.metadata()?.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            path.join("auth.json").metadata()?.permissions().mode() & 0o777,
            0o600
        );
        let auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("auth.json"))?)?;
        assert_eq!(auth["auth_mode"], "chatgptAuthTokens");
        assert_eq!(auth["tokens"]["refresh_token"], "");
        assert_eq!(auth["tokens"]["access_token"], "synthetic-access");
        assert_eq!(auth["tokens"]["id_token"], "synthetic-identity");
        drop(home);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn oauth_startup_scavenges_owned_homes_and_rejects_symlinks_without_removing_anything()
    -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("root");
        let root = OauthCredentialRoot::open(&path)?;
        let home = root.install(material())?;
        let home_path = home.path.clone();
        std::mem::forget(home);
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&outside)?;
        symlink(&outside, path.join("oauth-symlink"))?;
        assert!(OauthCredentialRoot::open(&path).is_err());
        assert!(home_path.join("auth.json").exists());
        assert!(outside.exists());
        std::fs::remove_file(path.join("oauth-symlink"))?;
        OauthCredentialRoot::open(&path)?;
        assert!(!home_path.exists());
        Ok(())
    }

    #[test]
    fn oauth_startup_rejects_unowned_shapes_and_symlink_ancestors() -> io::Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("root");
        let root = OauthCredentialRoot::open(&path)?;
        let home = root.install(material())?;
        std::fs::set_permissions(
            home.path.join("auth.json"),
            std::fs::Permissions::from_mode(0o644),
        )?;
        assert!(OauthCredentialRoot::open(&path).is_err());
        std::fs::set_permissions(
            home.path.join("auth.json"),
            std::fs::Permissions::from_mode(0o600),
        )?;
        symlink(temporary.path(), temporary.path().join("alias"))?;
        assert!(OauthCredentialRoot::open(&temporary.path().join("alias/root")).is_err());
        Ok(())
    }
}
