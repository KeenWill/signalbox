use signalbox_file_media_runtime::{
    FileMediaProviderFailure, FileMediaProviderReadRequest, ProcessorReadOutput,
};

pub(super) fn render(
    bytes: &[u8],
    request: &FileMediaProviderReadRequest,
) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    let (xml, _) = super::decode_xml(bytes, false).map_err(|_| FileMediaProviderFailure::Failed)?;
    let mut options = resvg::usvg::Options {
        font_family: String::from("DejaVu Sans"),
        ..Default::default()
    };
    options
        .fontdb_mut()
        .load_font_data(include_bytes!("../fonts/DejaVuSans.ttf").to_vec());
    options.fontdb_mut().set_sans_serif_family("DejaVu Sans");
    options.fontdb_mut().set_serif_family("DejaVu Sans");
    options.fontdb_mut().set_monospace_family("DejaVu Sans");
    let tree = resvg::usvg::Tree::from_str(&xml, &options)
        .map_err(|_| FileMediaProviderFailure::Failed)?;
    let size = tree.size();
    let width = f64::from(size.width());
    let height = f64::from(size.height());
    let axis = f64::from(
        request
            .maximum_image_axis
            .min(signalbox_file_media_runtime::MAX_IMAGE_AXIS),
    );
    let pixels = request
        .maximum_decoded_image_pixels
        .min(signalbox_file_media_runtime::MAX_DECODED_IMAGE_PIXELS) as f64;
    let scale = 1.0_f64
        .min(axis / width)
        .min(axis / height)
        .min((pixels / (width * height)).sqrt());
    let output_width = (width * scale).floor().max(1.0) as u32;
    let output_height = (height * scale).floor().max(1.0) as u32;
    if output_width > request.maximum_image_axis
        || output_height > request.maximum_image_axis
        || u64::from(output_width) * u64::from(output_height) > request.maximum_decoded_image_pixels
    {
        return Ok(ProcessorReadOutput::OutputUnitTooLarge);
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(output_width, output_height)
        .ok_or(FileMediaProviderFailure::Failed)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale as f32, scale as f32),
        &mut pixmap.as_mut(),
    );
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let pixel = pixel.demultiply();
            [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
        })
        .collect();
    Ok(signalbox_file_media_adapters_image::RenderedRaster {
        width: output_width,
        height: output_height,
        rgba,
    }
    .into_generated_image()
    .unwrap_or(ProcessorReadOutput::OutputUnitTooLarge))
}
