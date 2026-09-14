//! Descriptor-relative startup inventory and bounded canonical report pages.

use super::*;
use sha2::{Digest as _, Sha256};
use signalbox_runner_wire::{
    LeakFact, LeakFactKind, LeakPage, LeakPageCorrelation, LeakPageDigestInput,
    MAX_LEAK_PAGE_FACTS, leak_page_digest, leak_report_digest,
};

impl RunnerWorkspaceStore {
    pub(crate) async fn scan_startup_leaks(
        self,
        runner: CanonicalUuid,
    ) -> Result<Vec<LeakFact>, RunnerWorkspaceError> {
        let staging_guard = self.staging_cleanup.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _staging_guard = staging_guard;
            self.startup_leaks(runner)
        })
        .await
        .map_err(|error| RunnerWorkspaceError::Io(io::Error::other(error)))?
    }

    pub(crate) fn startup_leaks(
        &self,
        runner: CanonicalUuid,
    ) -> Result<Vec<LeakFact>, RunnerWorkspaceError> {
        validate_root_directory(&self.canonical_root, &self.root)
            .map_err(RunnerWorkspaceError::Io)?;
        let mut facts = Vec::new();
        let sessions = scan_directory(
            &self.root,
            SESSIONS_DIRECTORY,
            SESSIONS_DIRECTORY,
            &mut facts,
        )?;
        if let Some(sessions) = sessions {
            for name in entry_names(&sessions)? {
                let locator = format!("{SESSIONS_DIRECTORY}/{}", locator_component(&name));
                let Some(name_text) = name.to_str() else {
                    facts.push(entry_fact(&sessions, &name, locator)?);
                    continue;
                };
                let session = Uuid::parse_str(name_text)
                    .ok()
                    .filter(|id| id.to_string() == name_text);
                let Some(session) = session else {
                    facts.push(entry_fact(&sessions, &name, locator)?);
                    continue;
                };
                let Some(directory) = scan_directory(&sessions, name_text, &locator, &mut facts)?
                else {
                    continue;
                };
                for revision_name in entry_names(&directory)? {
                    let locator = format!("{locator}/{}", locator_component(&revision_name));
                    let revision = revision_name
                        .to_str()
                        .and_then(|value| {
                            value
                                .parse::<u64>()
                                .ok()
                                .filter(|revision| revision.to_string() == value)
                        })
                        .and_then(|value| PositiveU64::try_new(value).ok());
                    let Some(revision) = revision else {
                        facts.push(entry_fact(&directory, &revision_name, locator)?);
                        continue;
                    };
                    let Some(placement) = scan_directory(
                        &directory,
                        &revision.get().to_string(),
                        &locator,
                        &mut facts,
                    )?
                    else {
                        continue;
                    };
                    match read_manifest(&placement) {
                        Ok(mut manifest) => {
                            let leaf = if manifest.repository.is_some() {
                                REPOSITORY_WORKSPACE_DIRECTORY
                            } else {
                                PRIVATE_WORKSPACE_DIRECTORY
                            };
                            let expected_path = format!("{locator}/{leaf}");
                            let kind = if manifest.session.into_uuid() != session
                                || manifest.placement_revision != revision
                                || manifest.runner != runner
                                || manifest.relative_path != expected_path
                                || !statat(&placement, leaf, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(
                                    |status| {
                                        FileType::from_raw_mode(status.st_mode)
                                            == FileType::Directory
                                    },
                                ) {
                                LeakFactKind::ManifestConflict
                            } else if matches!(
                                manifest.lifecycle,
                                ManifestLifecycle::Ready
                                    | ManifestLifecycle::Active
                                    | ManifestLifecycle::Releasing
                            ) {
                                LeakFactKind::Unreconciled
                            } else {
                                LeakFactKind::RetiredPresent
                            };
                            if matches!(
                                manifest.lifecycle,
                                ManifestLifecycle::Active | ManifestLifecycle::Releasing
                            ) {
                                manifest.lifecycle = ManifestLifecycle::Ready;
                            }
                            facts.push(LeakFact {
                                kind,
                                locator: expected_path,
                                entry_digest: workspace_manifest_digest(&manifest)
                                    .map_err(|_| RunnerWorkspaceError::CorruptManifest)?,
                                session: Some(CanonicalUuid::from_uuid(session)),
                                placement_revision: Some(revision),
                            });
                        }
                        Err(_) => facts.push(entry_fact(&directory, &revision_name, locator)?),
                    }
                }
            }
        }
        if let Some(trash) = scan_directory(
            &self.root,
            release::TRASH_DIRECTORY,
            release::TRASH_DIRECTORY,
            &mut facts,
        )? {
            for name in entry_names(&trash)? {
                let locator = format!("{}/{}", release::TRASH_DIRECTORY, locator_component(&name));
                if let Some(fact) = trash_manifest_fact(&trash, &name, runner)? {
                    facts.push(fact);
                } else {
                    let mut fact = entry_fact(&trash, &name, locator)?;
                    fact.kind = LeakFactKind::RetiredPresent;
                    facts.push(fact);
                }
            }
        }
        facts.sort();
        facts.dedup();
        Ok(facts)
    }
}

fn trash_manifest_fact(
    trash: &File,
    name: &OsStr,
    runner: CanonicalUuid,
) -> Result<Option<LeakFact>, RunnerWorkspaceError> {
    let Some(name_text) = name.to_str() else {
        return Ok(None);
    };
    let Ok(placement) = open_directory(trash, name_text) else {
        return Ok(None);
    };
    let Ok(mut manifest) = read_manifest(&placement) else {
        return Ok(None);
    };
    let leaf = if manifest.repository.is_some() {
        REPOSITORY_WORKSPACE_DIRECTORY
    } else {
        PRIVATE_WORKSPACE_DIRECTORY
    };
    let expected_path = format!(
        "{SESSIONS_DIRECTORY}/{}/{}/{leaf}",
        manifest.session,
        manifest.placement_revision.get()
    );
    if manifest.manifest_id.to_string() != name_text
        || manifest.runner != runner
        || manifest.relative_path != expected_path
        || manifest.lifecycle != ManifestLifecycle::Releasing
    {
        return Ok(Some(LeakFact {
            kind: LeakFactKind::ManifestConflict,
            locator: format!("{}/{}", release::TRASH_DIRECTORY, locator_component(name)),
            entry_digest: workspace_manifest_digest(&manifest)
                .map_err(|_| RunnerWorkspaceError::CorruptManifest)?,
            session: Some(manifest.session),
            placement_revision: Some(manifest.placement_revision),
        }));
    }
    manifest.lifecycle = ManifestLifecycle::Ready;
    Ok(Some(LeakFact {
        kind: LeakFactKind::Unreconciled,
        locator: expected_path,
        entry_digest: workspace_manifest_digest(&manifest)
            .map_err(|_| RunnerWorkspaceError::CorruptManifest)?,
        session: Some(manifest.session),
        placement_revision: Some(manifest.placement_revision),
    }))
}

fn entry_names(directory: &File) -> Result<Vec<OsString>, RunnerWorkspaceError> {
    let mut entries = Dir::read_from(directory).map_err(rustix_io)?;
    let mut names = Vec::new();
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(rustix_io)?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name != "." && name != ".." {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn scan_directory(
    parent: &File,
    name: &str,
    locator: &str,
    facts: &mut Vec<LeakFact>,
) -> Result<Option<File>, RunnerWorkspaceError> {
    match open_directory(parent, name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => {
            facts.push(entry_fact(parent, OsStr::new(name), locator.to_owned())?);
            Ok(None)
        }
    }
}

fn locator_component(name: &OsStr) -> String {
    let mut text = String::new();
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            text.push(char::from(*byte));
        } else {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}

fn entry_fact(
    parent: &File,
    name: &OsStr,
    locator: String,
) -> Result<LeakFact, RunnerWorkspaceError> {
    let status = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(rustix_io)?;
    let mut digest = Sha256::new();
    digest.update(name.as_bytes());
    for value in [
        status.st_dev,
        status.st_ino,
        u64::from(status.st_mode),
        status.st_size as u64,
        status.st_ctime as u64,
        status.st_ctime_nsec as u64,
    ] {
        digest.update(value.to_be_bytes());
    }
    let digest = Digest::try_new(
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
    .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    Ok(LeakFact {
        kind: LeakFactKind::UnknownManifest,
        locator,
        entry_digest: digest,
        session: None,
        placement_revision: None,
    })
}

pub(crate) fn pages(
    registration_revision: PositiveU64,
    facts: &[LeakFact],
) -> Result<std::collections::VecDeque<LeakPage>, RunnerWorkspaceError> {
    let report_digest =
        leak_report_digest(facts).map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
    let mut pages = std::collections::VecDeque::new();
    let mut prior_page_digest = None;
    let count = facts.len().div_ceil(MAX_LEAK_PAGE_FACTS).max(1);
    for index in 0..count {
        let start = index * MAX_LEAK_PAGE_FACTS;
        let facts = facts[start..facts.len().min(start + MAX_LEAK_PAGE_FACTS)].to_vec();
        let page = PositiveU64::try_new(index as u64 + 1)
            .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
        let final_page = index + 1 == count;
        let page_digest = leak_page_digest(LeakPageDigestInput {
            registration_revision,
            report_digest: &report_digest,
            page,
            prior_page_digest: prior_page_digest.as_ref(),
            final_page,
            facts: &facts,
        })
        .map_err(|_| RunnerWorkspaceError::CorruptManifest)?;
        pages.push_back(LeakPage {
            correlation: LeakPageCorrelation {
                registration_revision,
                report_digest: report_digest.clone(),
                page,
            },
            prior_page_digest,
            final_page,
            facts,
            page_digest: page_digest.clone(),
        });
        prior_page_digest = Some(page_digest);
    }
    Ok(pages)
}
