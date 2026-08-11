//! Binary entrypoint for the `signalbox-client` terminal.
//!
//! Parses no arguments itself: forwards `argv` and `SIGNALBOX_SOCKET_PATH` to
//! `signalbox_client::run_terminal` (see `lib.rs`).

use std::{env, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    signalbox_client::run_terminal(env::args_os().skip(1), env::var_os("SIGNALBOX_SOCKET_PATH"))
        .await
}
