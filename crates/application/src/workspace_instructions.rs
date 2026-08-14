//! Deterministic filesystem discovery and registration validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use serde::Deserialize;
use signalbox_domain::{
    InstructionBundleKind, InstructionBundleRegistration, InstructionDigest,
    InstructionDiscoveryRootKind, InstructionPath, InstructionSkillMetadata,
};

/// One explicit authority root to scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoveryRoot {
    kind: InstructionDiscoveryRootKind,
    path: InstructionPath,
}

impl InstructionDiscoveryRoot {
    pub const fn new(kind: InstructionDiscoveryRootKind, path: InstructionPath) -> Self {
        Self { kind, path }
    }

    pub const fn kind(&self) -> InstructionDiscoveryRootKind {
        self.kind
    }

    pub const fn path(&self) -> &InstructionPath {
        &self.path
    }
}

/// Closed discovery and registration failures retained with a scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionDiscoveryFindingKind {
    RootUnavailable,
    EntryUnreadable,
    NonUtf8Source,
    InvalidSkill,
}

/// One visible discovery or registration rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoveryFinding {
    path: InstructionPath,
    kind: InstructionDiscoveryFindingKind,
}

impl InstructionDiscoveryFinding {
    pub const fn path(&self) -> &InstructionPath {
        &self.path
    }

    pub const fn kind(&self) -> InstructionDiscoveryFindingKind {
        self.kind
    }
}

/// One complete deterministic scan result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscoverySnapshot {
    roots: Box<[InstructionDiscoveryRoot]>,
    bundles: Box<[InstructionBundleRegistration]>,
    findings: Box<[InstructionDiscoveryFinding]>,
}

impl InstructionDiscoverySnapshot {
    pub fn roots(&self) -> &[InstructionDiscoveryRoot] {
        &self.roots
    }

    pub fn bundles(&self) -> &[InstructionBundleRegistration] {
        &self.bundles
    }

    pub fn findings(&self) -> &[InstructionDiscoveryFinding] {
        &self.findings
    }
}

/// Greedily walks every supplied root without following symbolic links.
pub fn discover_workspace_instructions(
    mut roots: Vec<InstructionDiscoveryRoot>,
) -> InstructionDiscoverySnapshot {
    roots.sort_by(|left, right| (left.kind(), left.path()).cmp(&(right.kind(), right.path())));
    let mut bundles = Vec::new();
    let mut findings = Vec::new();
    for root in &roots {
        walk_root(root, &mut bundles, &mut findings);
    }
    let mut seen_sources = BTreeSet::new();
    bundles.retain(|bundle| seen_sources.insert((bundle.source_path().clone(), bundle.kind())));
    InstructionDiscoverySnapshot {
        roots: roots.into_boxed_slice(),
        bundles: bundles.into_boxed_slice(),
        findings: findings.into_boxed_slice(),
    }
}

fn walk_root(
    root: &InstructionDiscoveryRoot,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
) {
    let root_path = PathBuf::from(root.path().as_str());
    match fs::symlink_metadata(&root_path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) | Err(_) => {
            findings.push(finding(
                root.path().clone(),
                InstructionDiscoveryFindingKind::RootUnavailable,
            ));
            return;
        }
    }
    let mut pending = vec![root_path];
    while let Some(directory) = pending.pop() {
        inspect_directory(root, &directory, bundles, findings);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                findings.push(finding_for_path(
                    &directory,
                    root.path(),
                    InstructionDiscoveryFindingKind::EntryUnreadable,
                ));
                continue;
            }
        };
        let mut children = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => children.push(entry),
                Err(_) => findings.push(finding_for_path(
                    &directory,
                    root.path(),
                    InstructionDiscoveryFindingKind::EntryUnreadable,
                )),
            }
        }
        children.sort_by_key(fs::DirEntry::file_name);
        for entry in children.into_iter().rev() {
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() && !file_type.is_symlink() => {
                    pending.push(entry.path());
                }
                Ok(_) => {}
                Err(_) => findings.push(finding_for_path(
                    &entry.path(),
                    root.path(),
                    InstructionDiscoveryFindingKind::EntryUnreadable,
                )),
            }
        }
    }
}

fn inspect_directory(
    root: &InstructionDiscoveryRoot,
    directory: &std::path::Path,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
) {
    let agents = directory.join("AGENTS.md");
    if is_regular_no_follow(&agents) {
        register_file(
            root,
            &agents,
            InstructionBundleKind::AgentDocument,
            None,
            bundles,
            findings,
        );
    }
    let is_skill = match root.kind() {
        InstructionDiscoveryRootKind::Configured => true,
        InstructionDiscoveryRootKind::Workspace => {
            directory
                .parent()
                .and_then(std::path::Path::file_name)
                .is_some_and(|name| name == "skills")
                && directory
                    .parent()
                    .and_then(std::path::Path::parent)
                    .and_then(std::path::Path::file_name)
                    .is_some_and(|name| name == ".agents")
        }
    };
    let skill = directory.join("SKILL.md");
    if is_skill && is_regular_no_follow(&skill) {
        let parent = directory.file_name().and_then(|name| name.to_str());
        register_file(
            root,
            &skill,
            InstructionBundleKind::AgentSkill,
            parent,
            bundles,
            findings,
        );
    }
}

fn register_file(
    root: &InstructionDiscoveryRoot,
    source: &std::path::Path,
    kind: InstructionBundleKind,
    skill_parent: Option<&str>,
    bundles: &mut Vec<InstructionBundleRegistration>,
    findings: &mut Vec<InstructionDiscoveryFinding>,
) {
    let source_path = match source
        .to_str()
        .and_then(|value| InstructionPath::try_new(value.to_owned()).ok())
    {
        Some(path) => path,
        None => {
            findings.push(finding_for_path(
                source,
                root.path(),
                InstructionDiscoveryFindingKind::EntryUnreadable,
            ));
            return;
        }
    };
    let bytes = match fs::read(source) {
        Ok(bytes) => bytes,
        Err(_) => {
            findings.push(finding(
                source_path,
                InstructionDiscoveryFindingKind::EntryUnreadable,
            ));
            return;
        }
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            findings.push(finding(
                source_path,
                InstructionDiscoveryFindingKind::NonUtf8Source,
            ));
            return;
        }
    };
    let skill = match skill_parent {
        Some(parent) => match parse_skill(text, parent) {
            Some(skill) => Some(skill),
            None => {
                findings.push(finding(
                    source_path,
                    InstructionDiscoveryFindingKind::InvalidSkill,
                ));
                return;
            }
        },
        None => None,
    };
    let Some(bundle) = InstructionBundleRegistration::new(
        kind,
        root.kind(),
        root.path().clone(),
        source_path.clone(),
        bytes.len() as u64,
        InstructionDigest::sha256(&bytes),
        skill,
    ) else {
        findings.push(finding(
            source_path,
            InstructionDiscoveryFindingKind::InvalidSkill,
        ));
        return;
    };
    bundles.push(bundle);
}

fn is_regular_no_follow(path: &std::path::Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn finding(
    path: InstructionPath,
    kind: InstructionDiscoveryFindingKind,
) -> InstructionDiscoveryFinding {
    InstructionDiscoveryFinding { path, kind }
}

fn finding_for_path(
    path: &std::path::Path,
    fallback: &InstructionPath,
    kind: InstructionDiscoveryFindingKind,
) -> InstructionDiscoveryFinding {
    let path = path
        .to_str()
        .and_then(|value| InstructionPath::try_new(value.to_owned()).ok())
        .unwrap_or_else(|| fallback.clone());
    finding(path, kind)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableSkillFrontmatter {
    name: String,
    description: String,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

fn parse_skill(text: &str, parent: &str) -> Option<InstructionSkillMetadata> {
    let body = text.strip_prefix("---\n")?;
    let boundary = body.find("\n---\n")?;
    let parsed: PortableSkillFrontmatter = serde_yaml_ng::from_str(&body[..boundary]).ok()?;
    if parsed
        .compatibility
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.chars().count() > 500)
        || parsed.license.as_ref().is_some_and(String::is_empty)
    {
        return None;
    }
    let _portable_optional_fields = (parsed.metadata, parsed.allowed_tools);
    InstructionSkillMetadata::try_new(parsed.name, parsed.description, parent).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greedy_discovery_finds_nested_documents_and_skills() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let nested = temporary.path().join("apps/api");
        let skill = nested.join(".agents/skills/review-rust");
        fs::create_dir_all(&skill).expect("nested skill directory exists");
        fs::write(nested.join("AGENTS.md"), "# API rules\n").expect("agent document is written");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\n# Review\n",
        )
        .expect("skill is written");
        let canonical = temporary.path().canonicalize().expect("root canonicalizes");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(canonical.to_string_lossy().into_owned())
                .expect("path is valid"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert_eq!(snapshot.bundles().len(), 2);
        assert!(snapshot.findings().is_empty());
        assert_eq!(
            snapshot.bundles()[0].kind(),
            InstructionBundleKind::AgentDocument
        );
        assert_eq!(
            snapshot.bundles()[1].kind(),
            InstructionBundleKind::AgentSkill
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_does_not_follow_a_skill_symlink() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root exists");
        let outside = tempfile::tempdir().expect("outside root exists");
        fs::write(outside.path().join("SKILL.md"), "not admitted").expect("outside file exists");
        let skills = temporary.path().join(".agents/skills");
        fs::create_dir_all(&skills).expect("skills root exists");
        symlink(outside.path(), skills.join("linked")).expect("skill link exists");
        let canonical = temporary.path().canonicalize().expect("root canonicalizes");
        let root = InstructionDiscoveryRoot::new(
            InstructionDiscoveryRootKind::Workspace,
            InstructionPath::try_new(canonical.to_string_lossy().into_owned())
                .expect("path is valid"),
        );

        let snapshot = discover_workspace_instructions(vec![root]);

        assert!(snapshot.bundles().is_empty());
    }

    #[test]
    fn overlapping_roots_emit_one_registration_for_the_same_source() {
        let temporary = tempfile::tempdir().expect("temporary root exists");
        let skills = temporary.path().join(".agents/skills/review-rust");
        fs::create_dir_all(&skills).expect("nested skill directory exists");
        fs::write(
            skills.join("SKILL.md"),
            "---\nname: review-rust\ndescription: Review Rust changes.\n---\n# Review\n",
        )
        .expect("skill is written");
        let workspace = temporary.path().canonicalize().expect("root canonicalizes");
        let configured = temporary
            .path()
            .join(".agents/skills")
            .canonicalize()
            .expect("configured root canonicalizes");

        let snapshot = discover_workspace_instructions(vec![
            InstructionDiscoveryRoot::new(
                InstructionDiscoveryRootKind::Configured,
                InstructionPath::try_new(configured.to_string_lossy().into_owned())
                    .expect("configured path is valid"),
            ),
            InstructionDiscoveryRoot::new(
                InstructionDiscoveryRootKind::Workspace,
                InstructionPath::try_new(workspace.to_string_lossy().into_owned())
                    .expect("workspace path is valid"),
            ),
        ]);

        assert_eq!(snapshot.roots().len(), 2);
        assert_eq!(snapshot.bundles().len(), 1);
        assert_eq!(
            snapshot.bundles()[0].root_kind(),
            InstructionDiscoveryRootKind::Workspace
        );
    }
}
