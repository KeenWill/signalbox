#[allow(dead_code)]
#[path = "../bridge.rs"]
mod bridge;

fn main() -> std::process::ExitCode {
    bridge::run(std::env::args_os())
}
