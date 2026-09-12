use signalbox_file_media_runtime::{
    FileMediaProviderFailure, FileMediaProviderReadRequest, ProcessorReadOutput,
};

pub(super) struct PageImageOptions {
    page_index: usize,
    scale: f64,
}

impl PageImageOptions {
    pub(super) fn parse(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "page" | "scale"))
        {
            return None;
        }
        let page_index = usize::try_from(object.get("page")?.as_u64()?.checked_sub(1)?).ok()?;
        let scale = match object.get("scale") {
            Some(value) => value.as_f64()?,
            None => 1.0,
        };
        (scale.is_finite() && scale > 0.0).then_some(Self { page_index, scale })
    }

    pub(super) fn render(
        self,
        bytes: Vec<u8>,
        page_count: usize,
        request: &FileMediaProviderReadRequest,
    ) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
        if self.page_index >= page_count {
            return Ok(ProcessorReadOutput::InvalidViewArguments);
        }
        let pdf =
            hayro::hayro_syntax::Pdf::new(bytes).map_err(|_| FileMediaProviderFailure::Failed)?;
        let page = pdf
            .pages()
            .get(self.page_index)
            .ok_or(FileMediaProviderFailure::Failed)?;
        let (width, height) = page.render_dimensions();
        let (width, height) = (f64::from(width), f64::from(height));
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(FileMediaProviderFailure::Failed);
        }
        let axis = f64::from(
            request
                .maximum_image_axis
                .min(signalbox_file_media_runtime::MAX_IMAGE_AXIS),
        );
        let pixels = request
            .maximum_decoded_image_pixels
            .min(signalbox_file_media_runtime::MAX_DECODED_IMAGE_PIXELS)
            as f64;
        let scale = self
            .scale
            .min(axis / width)
            .min(axis / height)
            .min((pixels / (width * height)).sqrt());
        let output_width = (width * scale).floor().max(1.0) as u16;
        let output_height = (height * scale).floor().max(1.0) as u16;
        if u32::from(output_width) > request.maximum_image_axis
            || u32::from(output_height) > request.maximum_image_axis
            || u64::from(output_width) * u64::from(output_height)
                > request.maximum_decoded_image_pixels
        {
            return Ok(ProcessorReadOutput::OutputUnitTooLarge);
        }
        let settings = hayro::RenderSettings {
            x_scale: scale as f32,
            y_scale: scale as f32,
            width: Some(output_width),
            height: Some(output_height),
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        };
        let pixmap = hayro::render(
            page,
            &hayro::RenderCache::new(),
            &Default::default(),
            &settings,
        );
        let rgba = pixmap
            .take_unpremultiplied()
            .into_iter()
            .flat_map(|pixel| [pixel.r, pixel.g, pixel.b, pixel.a])
            .collect();
        Ok(signalbox_file_media_adapters_image::RenderedRaster {
            width: u32::from(output_width),
            height: u32::from(output_height),
            rgba,
        }
        .into_generated_image()
        .unwrap_or(ProcessorReadOutput::OutputUnitTooLarge))
    }
}
