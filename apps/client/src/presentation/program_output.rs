use super::*;
use signalbox_process_protocol::ProgramRunCancellationOutcome;

impl Output<'_> {
    pub(crate) fn program_run(
        &mut self,
        run_id: CanonicalUuid,
        run: signalbox_process_protocol::ProgramRun,
    ) -> io::Result<()> {
        writeln!(
            self.stdout,
            "run={run_id} {}",
            serde_json::to_string(&run).map_err(io::Error::other)?
        )
    }
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
