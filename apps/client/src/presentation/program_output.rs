use super::*;
use signalbox_process_protocol::ProgramRunCancellationOutcome;

impl Output<'_> {
    pub(crate) fn program_cancellation(
        &mut self,
        run_id: CanonicalUuid,
        outcome: ProgramRunCancellationOutcome,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "run={run_id} {}",
            serde_json::to_string(&outcome).map_err(io::Error::other)?
        )
    }
}
