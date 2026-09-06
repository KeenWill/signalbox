//! Resolves the executable emitted by one Cargo binary build.

use std::{
    env,
    ffi::OsString,
    io::{self, BufReader},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use cargo_metadata::{Message, MetadataCommand};

fn emitted_executable(reader: impl io::Read) -> Result<PathBuf, String> {
    Message::parse_stream(BufReader::new(reader))
        .filter_map(|message| match message {
            Ok(Message::CompilerArtifact(artifact)) => artifact.executable.map(Ok),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .try_fold(None, |_, executable| executable.map(Some))
        .map_err(|error| format!("invalid Cargo message stream: {error}"))?
        .map(Into::into)
        .ok_or_else(|| String::from("Cargo emitted no executable artifact"))
}

fn host_triple(output: &str) -> Option<&str> {
    output.lines().find_map(|line| line.strip_prefix("host: "))
}

fn executable_is_for_host(executable: &Path, target_dir: &Path, binary: &str, host: &str) -> bool {
    executable == target_dir.join("debug").join(binary)
        || executable == target_dir.join(host).join("debug").join(binary)
}

fn run(arguments: &[OsString]) -> Result<PathBuf, String> {
    let [manifest, package, binary] = arguments else {
        return Err(String::from(
            "usage: signalbox-cargo-bin-resolver <manifest-path> <package> <bin>",
        ));
    };
    let metadata = MetadataCommand::new()
        .manifest_path(manifest)
        .no_deps()
        .exec()
        .map_err(|error| format!("could not read Cargo metadata: {error}"))?;
    let target_dir = metadata.target_directory.into_std_path_buf();
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut child = Command::new(cargo)
        .args([
            OsString::from("build"),
            OsString::from("--manifest-path"),
            manifest.clone(),
            OsString::from("--target-dir"),
            target_dir.as_os_str().to_owned(),
            OsString::from("-p"),
            package.clone(),
            OsString::from("--bin"),
            binary.clone(),
            OsString::from("--message-format=json-render-diagnostics"),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start Cargo: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| String::from("Cargo stdout was unavailable"))?;
    let executable = emitted_executable(stdout)?;
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for Cargo: {error}"))?;
    if !status.success() {
        return Err(format!("Cargo build failed with {status}"));
    }

    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(rustc)
        .arg("-vV")
        .output()
        .map_err(|error| format!("could not inspect rustc: {error}"))?;
    if !output.status.success() {
        return Err(String::from("rustc -vV failed"));
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|_| String::from("rustc -vV emitted non-UTF-8 output"))?;
    let host =
        host_triple(&version).ok_or_else(|| String::from("rustc -vV omitted its host triple"))?;
    let binary = binary
        .to_str()
        .ok_or_else(|| String::from("binary name is not UTF-8"))?;
    if !executable_is_for_host(&executable, &target_dir, binary, host) {
        return Err(format!(
            "Cargo built {package:?} for a target this host cannot run: {}; unset build.target and CARGO_BUILD_TARGET, or set them to {host}",
            executable.display()
        ));
    }
    Ok(executable)
}

fn main() -> ExitCode {
    match run(&env::args_os().skip(1).collect::<Vec<_>>()) {
        Ok(executable) => {
            println!("{}", executable.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("resolve-cargo-bin: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{emitted_executable, executable_is_for_host, host_triple};
    use std::path::{Path, PathBuf};

    #[test]
    fn resolves_the_last_executable_artifact_from_cargo_messages() {
        let messages = concat!(
            "{\"reason\":\"build-script-executed\",\"package_id\":\"path+file:///x#x@0.0.0\",\"linked_libs\":[],\"linked_paths\":[],\"cfgs\":[],\"env\":[],\"out_dir\":\"/tmp/out\"}\n",
            "{\"reason\":\"compiler-artifact\",\"package_id\":\"path+file:///x#x@0.0.0\",\"manifest_path\":\"/x/Cargo.toml\",\"target\":{\"kind\":[\"bin\"],\"crate_types\":[\"bin\"],\"name\":\"x\",\"src_path\":\"/x/main.rs\",\"edition\":\"2024\",\"doc\":true,\"doctest\":false,\"test\":true},\"profile\":{\"opt_level\":\"0\",\"debuginfo\":0,\"debug_assertions\":true,\"overflow_checks\":true,\"test\":false},\"features\":[],\"filenames\":[\"/tmp/x\"],\"executable\":\"/tmp/x\",\"fresh\":false}\n"
        );
        assert_eq!(
            emitted_executable(messages.as_bytes()),
            Ok(PathBuf::from("/tmp/x"))
        );
    }

    #[test]
    fn accepts_plain_and_explicit_host_target_layouts() {
        let target = Path::new("/work/target");
        assert!(executable_is_for_host(
            Path::new("/work/target/debug/app"),
            target,
            "app",
            "x86_64-unknown-linux-gnu"
        ));
        assert!(executable_is_for_host(
            Path::new("/work/target/x86_64-unknown-linux-gnu/debug/app"),
            target,
            "app",
            "x86_64-unknown-linux-gnu"
        ));
        assert!(!executable_is_for_host(
            Path::new("/work/target/aarch64-unknown-linux-gnu/debug/app"),
            target,
            "app",
            "x86_64-unknown-linux-gnu"
        ));
    }

    #[test]
    fn reads_rustc_host_triple() {
        assert_eq!(
            host_triple("rustc 1.97.0\nhost: x86_64-unknown-linux-gnu\n"),
            Some("x86_64-unknown-linux-gnu")
        );
    }
}
