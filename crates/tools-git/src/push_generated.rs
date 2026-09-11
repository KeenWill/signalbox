//! Checked-in generated-output declarations for merge verification.

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::Instant,
};

use git2::{ObjectType, Oid, Repository, Tree};
use serde::Deserialize;

use crate::{push_executor::GitPushFailure, push_objects::ObjectSource};

const MANIFEST: &str = "config/generated-files.json";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GeneratedManifest {
    generators: Vec<Generator>,
    #[serde(skip)]
    selected: BTreeSet<usize>,
    #[serde(skip)]
    paths: BTreeSet<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Generator {
    program: String,
    script: String,
    #[serde(default)]
    arguments: Vec<String>,
    outputs: Vec<String>,
}

/// A checked-in generator to execute without shell interpolation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitGeneratorCommand {
    program: String,
    arguments: Vec<String>,
}

impl GitGeneratorCommand {
    /// Borrows the declared interpreter or executable.
    pub fn program(&self) -> &str {
        &self.program
    }
    /// Borrows the script path followed by its declared arguments.
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

/// An isolated combined tree and its required checked-in generators.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitGenerationRequest {
    root: PathBuf,
    commands: Vec<GitGeneratorCommand>,
}

impl GitGenerationRequest {
    /// Borrows the disposable workspace containing the combined tree.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Borrows the generators selected by changed declared outputs.
    pub fn commands(&self) -> &[GitGeneratorCommand] {
        &self.commands
    }
}

#[derive(Debug)]
pub(super) struct GeneratedMerge {
    workspace: tempfile::TempDir,
    commands: Vec<GitGeneratorCommand>,
    expected: Vec<(PathBuf, Option<Oid>)>,
    object_format: git2::ObjectFormat,
}

fn failure<T>(_: T) -> GitPushFailure {
    GitPushFailure::Repository
}

fn relative_path(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

impl GeneratedManifest {
    pub(super) fn load(
        repository: &Repository,
        base: &Tree<'_>,
        source: &mut ObjectSource,
    ) -> Result<Self, GitPushFailure> {
        let entry = match base.get_path(Path::new(MANIFEST)) {
            Ok(entry) => entry,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(Self::default()),
            Err(error) => return Err(failure(error)),
        };
        if entry.filemode() != 0o100644 {
            return Err(GitPushFailure::Repository);
        }
        source
            .capture(&repository.odb().map_err(failure)?, entry.id())
            .map_err(failure)?;
        let blob = repository.find_blob(entry.id()).map_err(failure)?;
        let manifest: Self = serde_json::from_slice(blob.content()).map_err(failure)?;
        for generator in &manifest.generators {
            if generator.program.is_empty()
                || !relative_path(&generator.script)
                || generator.outputs.is_empty()
                || generator
                    .outputs
                    .iter()
                    .any(|output| !relative_path(output))
            {
                return Err(GitPushFailure::Repository);
            }
            let script = base
                .get_path(Path::new(&generator.script))
                .map_err(failure)?;
            if !matches!(script.filemode(), 0o100644 | 0o100755) {
                return Err(GitPushFailure::Repository);
            }
        }
        Ok(manifest)
    }

    pub(super) fn select(&mut self, path: &Path) -> bool {
        let mut selected = false;
        for (index, generator) in self.generators.iter().enumerate() {
            if generator.outputs.iter().any(|output| {
                if output.ends_with('/') {
                    path.starts_with(output)
                } else {
                    path == Path::new(output)
                }
            }) {
                self.selected.insert(index);
                selected = true;
            }
        }
        if selected {
            self.paths.insert(path.to_owned());
        }
        selected
    }

    pub(super) fn prepare(
        self,
        repository: &Repository,
        tree: &Tree<'_>,
        source: &mut ObjectSource,
        deadline: Instant,
    ) -> Result<Option<GeneratedMerge>, GitPushFailure> {
        if self.selected.is_empty() {
            return Ok(None);
        }
        for index in &self.selected {
            let script = tree
                .get_path(Path::new(&self.generators[*index].script))
                .map_err(failure)?;
            if !matches!(script.filemode(), 0o100644 | 0o100755) {
                return Err(GitPushFailure::Repository);
            }
        }
        let workspace = tempfile::tempdir().map_err(failure)?;
        let database = repository.odb().map_err(failure)?;
        let mut pending = vec![(tree.id(), PathBuf::new())];
        while let Some((id, parent)) = pending.pop() {
            if Instant::now() >= deadline {
                return Err(GitPushFailure::PreDispatchInfrastructure);
            }
            source.capture(&database, id).map_err(failure)?;
            let tree = repository.find_tree(id).map_err(failure)?;
            for entry in tree.iter() {
                use std::os::unix::ffi::OsStrExt;
                let name = std::ffi::OsStr::from_bytes(entry.name_bytes());
                if !Path::new(name)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                {
                    return Err(GitPushFailure::Repository);
                }
                let relative = parent.join(name);
                let destination = workspace.path().join(&relative);
                match entry.kind() {
                    Some(ObjectType::Tree) => {
                        fs::create_dir(&destination).map_err(failure)?;
                        pending.push((entry.id(), relative));
                    }
                    Some(ObjectType::Blob) => {
                        source.capture(&database, entry.id()).map_err(failure)?;
                        if entry.filemode() == 0o120000 {
                            let blob = repository.find_blob(entry.id()).map_err(failure)?;
                            std::os::unix::fs::symlink(
                                std::ffi::OsStr::from_bytes(blob.content()),
                                destination,
                            )
                            .map_err(failure)?;
                        } else {
                            use std::os::unix::fs::PermissionsExt;
                            let (mut reader, size, kind) =
                                database.reader(entry.id()).map_err(failure)?;
                            if kind != ObjectType::Blob {
                                return Err(GitPushFailure::Repository);
                            }
                            let mut output = fs::File::create(&destination).map_err(failure)?;
                            // Copy memory is independent of the generated repository's blob sizes.
                            let mut buffer = [0u8; 8192];
                            let mut written = 0usize;
                            loop {
                                if Instant::now() >= deadline {
                                    return Err(GitPushFailure::PreDispatchInfrastructure);
                                }
                                let count = reader.read(&mut buffer).map_err(failure)?;
                                if count == 0 {
                                    break;
                                }
                                output.write_all(&buffer[..count]).map_err(failure)?;
                                written = written
                                    .checked_add(count)
                                    .ok_or(GitPushFailure::Repository)?;
                            }
                            if written != size {
                                return Err(GitPushFailure::Repository);
                            }
                            fs::set_permissions(
                                destination,
                                fs::Permissions::from_mode(entry.filemode() as u32 & 0o777),
                            )
                            .map_err(failure)?;
                        }
                    }
                    Some(ObjectType::Commit) => {
                        fs::create_dir(destination).map_err(failure)?;
                    }
                    _ => return Err(GitPushFailure::Repository),
                }
            }
        }
        let mut expected = Vec::new();
        for path in self.paths {
            let id = match tree.get_path(&path) {
                Ok(entry) if entry.filemode() == 0o100644 => Some(entry.id()),
                Err(error) if error.code() == git2::ErrorCode::NotFound => None,
                _ => return Err(GitPushFailure::Repository),
            };
            if id.is_some() {
                fs::remove_file(workspace.path().join(&path)).map_err(failure)?;
            }
            expected.push((path, id));
        }
        let commands = self
            .selected
            .into_iter()
            .map(|index| {
                let generator = &self.generators[index];
                GitGeneratorCommand {
                    program: generator.program.clone(),
                    arguments: std::iter::once(generator.script.clone())
                        .chain(generator.arguments.clone())
                        .collect(),
                }
            })
            .collect();
        Ok(Some(GeneratedMerge {
            workspace,
            commands,
            expected,
            object_format: repository.object_format(),
        }))
    }
}

impl GeneratedMerge {
    pub(super) fn request(&self) -> GitGenerationRequest {
        GitGenerationRequest {
            root: self.workspace.path().to_owned(),
            commands: self.commands.clone(),
        }
    }

    pub(super) fn verify(&self) -> Result<(), GitPushFailure> {
        for (path, expected) in &self.expected {
            let output = self.workspace.path().join(path);
            let actual = match fs::symlink_metadata(&output) {
                Ok(metadata) if metadata.is_file() => {
                    if !output
                        .canonicalize()
                        .map_err(failure)?
                        .starts_with(self.workspace.path())
                    {
                        return Err(GitPushFailure::Repository);
                    }
                    Some(
                        Oid::hash_file_ext(ObjectType::Blob, &output, self.object_format)
                            .map_err(failure)?,
                    )
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                _ => return Err(GitPushFailure::Repository),
            };
            if actual != *expected {
                return Err(GitPushFailure::MergeDroppedBaseChanges(vec![
                    crate::push_merge::DroppedBaseChanges {
                        file: String::from_utf8(crate::diff::quoted_diff_path(b"", path))
                            .map_err(failure)?,
                        first_dropped_hunk: "declared generator output differs from the merge"
                            .to_owned(),
                        truncated: false,
                    },
                ]));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_comparison_uses_the_repository_object_format() {
        for object_format in [git2::ObjectFormat::Sha1, git2::ObjectFormat::Sha256] {
            let workspace = tempfile::tempdir().expect("generated workspace");
            let path = PathBuf::from("generated.txt");
            let content = b"combined output\n";
            let expected = Oid::hash_object_ext(ObjectType::Blob, content, object_format)
                .expect("committed blob id");
            fs::write(workspace.path().join(&path), content).expect("generator output");
            let generated = GeneratedMerge {
                workspace,
                commands: Vec::new(),
                expected: vec![(path, Some(expected))],
                object_format,
            };

            generated
                .verify()
                .expect("same content has the repository's object identity");
        }
    }

    #[test]
    fn generator_success_without_output_does_not_accept_the_candidate() {
        let workspace = tempfile::tempdir().expect("generated workspace");
        let content = b"candidate content\n";
        let expected = Oid::hash_object(ObjectType::Blob, content).expect("candidate blob id");
        let generated = GeneratedMerge {
            workspace,
            commands: Vec::new(),
            expected: vec![(PathBuf::from("generated.txt"), Some(expected))],
            object_format: git2::ObjectFormat::Sha1,
        };

        assert!(
            matches!(
                generated.verify(),
                Err(GitPushFailure::MergeDroppedBaseChanges(_))
            ),
            "a successful no-op leaves the removed output missing"
        );
    }
}
