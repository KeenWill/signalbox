//! Proof-archive companion entrypoint for the separately packaged supervisor.

#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    signalbox_tools_exec::exec_supervisor_entrypoint()
}
