//! Dedicated supervised archive worker governed by `docs/spec/file-and-media.md`.

use std::error::Error;

use signalbox_file_media_adapter_archives::ArchiveProvider;
use signalbox_file_media_processor_runtime::{WorkerCatalog, serve_one};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let catalog = WorkerCatalog::try_new(vec![Box::new(ArchiveProvider::new())])?;
    serve_one(&catalog).await?;
    Ok(())
}
