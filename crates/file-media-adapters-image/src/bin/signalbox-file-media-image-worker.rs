use std::error::Error;

use signalbox_file_media_adapters_image::ImageFamilyProvider;
use signalbox_file_media_processor_runtime::{WorkerCatalog, serve_one};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn Error>> {
    let catalog = WorkerCatalog::try_new(vec![Box::new(ImageFamilyProvider)])?;
    serve_one(&catalog).await?;
    Ok(())
}
