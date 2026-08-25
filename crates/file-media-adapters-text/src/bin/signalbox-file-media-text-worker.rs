use std::error::Error;

use signalbox_file_media_adapters_text::TextFamilyProvider;
use signalbox_file_media_processor_runtime::{WorkerCatalog, serve_one};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let catalog = WorkerCatalog::try_new(vec![Box::new(TextFamilyProvider)])?;
    serve_one(&catalog).await?;
    Ok(())
}
