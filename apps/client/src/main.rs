use std::{env, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    signalbox_client::run_terminal(env::args_os().skip(1), env::var_os("SIGNALBOX_SOCKET_PATH"))
        .await
}
