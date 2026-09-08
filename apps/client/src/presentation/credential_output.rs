use super::*;
use signalbox_process_protocol::{CredentialExclusionClearOutcome, CredentialExclusionTarget};

fn target_json(target: &CredentialExclusionTarget) -> io::Result<String> {
    let json = serde_json::to_string(target).map_err(io::Error::other)?;
    Ok(json
        .chars()
        .map(|character| {
            if character.is_control() {
                format!("\\u{:04x}", u32::from(character))
            } else {
                character.to_string()
            }
        })
        .collect())
}
impl Output<'_> {
    pub(crate) fn credential_exclusion(
        &mut self,
        target: &CredentialExclusionTarget,
    ) -> io::Result<()> {
        writeln!(self.stdout, "{}", target_json(target)?)
    }
    pub(crate) fn credential_exclusion_cursor(
        &mut self,
        target: &CredentialExclusionTarget,
    ) -> io::Result<()> {
        writeln!(self.stdout, "next_after {}", target_json(target)?)
    }
    pub(crate) fn credential_exclusion_cleared(
        &mut self,
        target: &CredentialExclusionTarget,
        outcome: CredentialExclusionClearOutcome,
    ) -> io::Result<()> {
        let outcome = match outcome {
            CredentialExclusionClearOutcome::Cleared => "cleared",
            CredentialExclusionClearOutcome::AlreadyCleared => "already_cleared",
        };
        writeln!(self.stdout, "{outcome} {}", target_json(target)?)
    }
}
