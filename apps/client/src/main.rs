use std::{env, io, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    signalbox_client::run(
        env::args_os().skip(1),
        env::var_os("SIGNALBOX_SOCKET_PATH"),
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
    .await
}
