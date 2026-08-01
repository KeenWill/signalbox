fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = std::env::var("TARGET")?;
    println!("cargo:rustc-env=SIGNALBOX_EXECUTION_TARGET={target}");
    Ok(())
}
