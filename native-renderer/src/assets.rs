use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::{ImageReader, Limits};

use crate::{Error, RendererContract, Result};

#[derive(Clone, Copy, Debug)]
pub struct AtlasOptions {
    /// Raster scale relative to each SVG's intrinsic dimensions.
    pub svg_scale: f32,
    pub max_asset_dimension: u32,
    pub max_texture_dimension: u32,
    /// Wrapped texel gutter around every sprite for seam-free repeat sampling
    /// and isolated mipmaps. Non-repeating shader paths clamp per entry.
    pub padding: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasEntry {
    pub page: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Intrinsic Pixi texture dimensions before SVG supersampling.
    pub logical_width: f32,
    pub logical_height: f32,
    pub u_min: f32,
    pub v_min: f32,
    pub u_max: f32,
    pub v_max: f32,
}

#[derive(Debug)]
pub struct TextureAtlasPage {
    pub width: u32,
    pub height: u32,
    /// Premultiplied 8-bit RGBA in sRGB, matching Pixi's blend convention.
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
pub struct TextureAtlas {
    pub entries: BTreeMap<String, AtlasEntry>,
    pub pages: Vec<TextureAtlasPage>,
    /// Power-of-two texel gutter retained around each entry. The GPU uploader
    /// uses it to build the largest atlas-safe mip chain.
    pub padding: u32,
}

#[derive(Debug)]
pub struct AtlasRasterAsset {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub logical_width: f32,
    pub logical_height: f32,
    /// Premultiplied 8-bit RGBA.
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
struct RasterAsset {
    name: String,
    width: u32,
    height: u32,
    logical_width: f32,
    logical_height: f32,
    rgba: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Placement {
    page: usize,
    outer_x: u32,
    outer_y: u32,
}

#[derive(Clone, Copy, Debug)]
struct Shelf {
    y: u32,
    height: u32,
    next_x: u32,
}

#[derive(Debug, Default)]
struct PagePlan {
    shelves: Vec<Shelf>,
    used_width: u32,
    used_height: u32,
}

impl Default for AtlasOptions {
    fn default() -> Self {
        Self {
            svg_scale: 4.0,
            max_asset_dimension: 1_024,
            max_texture_dimension: 4_096,
            padding: 64,
        }
    }
}

impl TextureAtlas {
    pub fn build(contract: &RendererContract, options: AtlasOptions) -> Result<Self> {
        Self::build_with_raster_assets(contract, options, Vec::new())
    }

    pub fn build_with_raster_assets(
        contract: &RendererContract,
        options: AtlasOptions,
        raster_assets: Vec<AtlasRasterAsset>,
    ) -> Result<Self> {
        options.validate()?;
        let resources = contract.resources.as_object().ok_or_else(|| {
            Error::Invalid("renderer contract resources must be an object".to_owned())
        })?;
        let mut assets = resources
            .iter()
            .map(|(name, source)| {
                let source = source.as_str().ok_or_else(|| {
                    Error::Invalid(format!("renderer resource {name} must be a string"))
                })?;
                rasterize(name, source, options, options.max_asset_dimension)
            })
            .collect::<Result<Vec<_>>>()?;
        collect_decoration_assets(contract, options, &mut assets)?;
        let mut names = assets
            .iter()
            .map(|asset| asset.name.as_str())
            .collect::<BTreeSet<_>>();
        for asset in &raster_assets {
            let expected_bytes = (asset.width as usize)
                .checked_mul(asset.height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(Error::ArithmeticOverflow)?;
            if asset.name.is_empty()
                || !names.insert(&asset.name)
                || asset.width == 0
                || asset.height == 0
                || asset.width > options.max_asset_dimension
                || asset.height > options.max_asset_dimension
                || !asset.logical_width.is_finite()
                || !asset.logical_height.is_finite()
                || asset.logical_width < 0.0
                || asset.logical_height < 0.0
                || asset.rgba.len() != expected_bytes
            {
                return Err(Error::Invalid(format!(
                    "procedural atlas asset {} is invalid or duplicated",
                    asset.name
                )));
            }
        }
        drop(names);
        assets.extend(raster_assets.into_iter().map(|asset| RasterAsset {
            name: asset.name,
            width: asset.width,
            height: asset.height,
            logical_width: asset.logical_width,
            logical_height: asset.logical_height,
            rgba: asset.rgba,
        }));
        assets.sort_by(|left, right| {
            right
                .height
                .cmp(&left.height)
                .then_with(|| right.width.cmp(&left.width))
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut plans = Vec::<PagePlan>::new();
        let mut placements = Vec::with_capacity(assets.len());
        for asset in &assets {
            let outer_width = asset
                .width
                .checked_add(options.padding * 2)
                .and_then(|value| value.checked_next_multiple_of(options.padding))
                .ok_or(Error::ArithmeticOverflow)?;
            let outer_height = asset
                .height
                .checked_add(options.padding * 2)
                .and_then(|value| value.checked_next_multiple_of(options.padding))
                .ok_or(Error::ArithmeticOverflow)?;
            if outer_width > options.max_texture_dimension
                || outer_height > options.max_texture_dimension
            {
                return Err(Error::Invalid(format!(
                    "renderer resource {} does not fit the texture atlas",
                    asset.name
                )));
            }
            let placement = place_asset(
                &mut plans,
                outer_width,
                outer_height,
                options.max_texture_dimension,
            );
            placements.push(placement);
        }
        if plans.is_empty() {
            plans.push(PagePlan::default());
        }

        // A GPU `texture_2d_array` requires every layer to have identical
        // dimensions. Use the smallest common extent that contains every page
        // plan so atlas coordinates remain directly usable by the shader.
        let page_width = plans
            .iter()
            .map(|plan| plan.used_width)
            .max()
            .unwrap_or(0)
            .max(1);
        let page_height = plans
            .iter()
            .map(|plan| plan.used_height)
            .max()
            .unwrap_or(0)
            .max(1);
        let mut pages = plans
            .iter()
            .map(|_| {
                let width = page_width;
                let height = page_height;
                let byte_count = (width as usize)
                    .checked_mul(height as usize)
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or(Error::ArithmeticOverflow)?;
                Ok(TextureAtlasPage {
                    width,
                    height,
                    rgba: vec![0; byte_count],
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut entries = BTreeMap::new();
        for (asset, placement) in assets.iter().zip(placements) {
            let page = &mut pages[placement.page];
            let x = placement.outer_x + options.padding;
            let y = placement.outer_y + options.padding;
            blit_wrapped(page, asset, x, y, options.padding);
            entries.insert(
                asset.name.clone(),
                AtlasEntry {
                    page: placement.page as u32,
                    x,
                    y,
                    width: asset.width,
                    height: asset.height,
                    logical_width: asset.logical_width,
                    logical_height: asset.logical_height,
                    u_min: x as f32 / page.width as f32,
                    v_min: y as f32 / page.height as f32,
                    u_max: (x + asset.width) as f32 / page.width as f32,
                    v_max: (y + asset.height) as f32 / page.height as f32,
                },
            );
        }
        Ok(Self {
            entries,
            pages,
            padding: options.padding,
        })
    }
}

pub fn decoration_asset_name(index: usize, property_path: &[&str]) -> String {
    let mut name = format!("$decoration[{index}]");
    for property in property_path {
        name.push('.');
        name.push_str(property);
    }
    name
}

pub(crate) fn expected_atlas_asset_names(contract: &RendererContract) -> Result<BTreeSet<String>> {
    let resources = contract.resources.as_object().ok_or_else(|| {
        Error::Invalid("renderer contract resources must be an object".to_owned())
    })?;
    let mut names = resources.keys().cloned().collect::<BTreeSet<_>>();
    for (index, decoration) in contract.decorations.iter().enumerate() {
        collect_decoration_names(decoration, index, &mut Vec::new(), &mut names)?;
    }
    Ok(names)
}

fn collect_decoration_names(
    value: &serde_json::Value,
    index: usize,
    path: &mut Vec<String>,
    names: &mut BTreeSet<String>,
) -> Result<()> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                path.push(key.clone());
                if is_decoration_asset_key(key) {
                    if !value.is_string() {
                        return Err(Error::Invalid(format!(
                            "renderer decoration URL {} must be a string",
                            decoration_name_from_strings(index, path)
                        )));
                    }
                    names.insert(decoration_name_from_strings(index, path));
                } else {
                    collect_decoration_names(value, index, path, names)?;
                }
                path.pop();
            }
        }
        serde_json::Value::Array(values) => {
            for (array_index, value) in values.iter().enumerate() {
                path.push(format!("[{array_index}]"));
                collect_decoration_names(value, index, path, names)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

fn decoration_name_from_strings(index: usize, path: &[String]) -> String {
    decoration_asset_name(index, &path.iter().map(String::as_str).collect::<Vec<_>>())
}

fn collect_decoration_assets(
    contract: &RendererContract,
    options: AtlasOptions,
    assets: &mut Vec<RasterAsset>,
) -> Result<()> {
    let maximum_dimension = options
        .padding
        .checked_mul(2)
        .and_then(|padding| options.max_texture_dimension.checked_sub(padding))
        .ok_or_else(|| Error::Invalid("atlas decoration dimensions are invalid".to_owned()))?;
    for (index, decoration) in contract.decorations.iter().enumerate() {
        collect_decoration_value(
            decoration,
            index,
            &mut Vec::new(),
            options,
            maximum_dimension,
            assets,
        )?;
    }
    Ok(())
}

fn collect_decoration_value(
    value: &serde_json::Value,
    index: usize,
    path: &mut Vec<String>,
    options: AtlasOptions,
    maximum_dimension: u32,
    assets: &mut Vec<RasterAsset>,
) -> Result<()> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                path.push(key.clone());
                if is_decoration_asset_key(key) {
                    let source = value.as_str().ok_or_else(|| {
                        Error::Invalid(format!(
                            "renderer decoration URL {} must be a string",
                            decoration_name_from_strings(index, path)
                        ))
                    })?;
                    let name = decoration_name_from_strings(index, path);
                    assets.push(rasterize(&name, source, options, maximum_dimension)?);
                } else {
                    collect_decoration_value(
                        value,
                        index,
                        path,
                        options,
                        maximum_dimension,
                        assets,
                    )?;
                }
                path.pop();
            }
        }
        serde_json::Value::Array(values) => {
            for (array_index, value) in values.iter().enumerate() {
                path.push(format!("[{array_index}]"));
                collect_decoration_value(value, index, path, options, maximum_dimension, assets)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_decoration_asset_key(key: &str) -> bool {
    key == "url" || key.ends_with("Url")
}

impl AtlasOptions {
    pub(crate) fn validate(self) -> Result<()> {
        if !self.svg_scale.is_finite() || self.svg_scale <= 0.0 {
            return Err(Error::Invalid(
                "SVG atlas scale must be positive".to_owned(),
            ));
        }
        if self.max_asset_dimension == 0 || self.max_texture_dimension == 0 {
            return Err(Error::Invalid(
                "atlas dimensions must be positive".to_owned(),
            ));
        }
        if !self.padding.is_power_of_two()
            || self
                .padding
                .checked_mul(2)
                .is_none_or(|padding| padding >= self.max_texture_dimension)
        {
            return Err(Error::Invalid(
                "atlas padding must be a power of two consistent with the texture limit".to_owned(),
            ));
        }
        Ok(())
    }
}

fn rasterize(
    name: &str,
    source: &str,
    options: AtlasOptions,
    maximum_dimension: u32,
) -> Result<RasterAsset> {
    let (mime_type, bytes) = decode_data_uri(name, source)?;
    match mime_type.as_str() {
        "image/svg+xml" => rasterize_svg(name, &bytes, options, maximum_dimension),
        "image/jpeg" | "image/png" | "image/webp" => {
            rasterize_image(name, &bytes, maximum_dimension)
        }
        other => Err(Error::Invalid(format!(
            "renderer resource {name} uses unsupported media type {other}"
        ))),
    }
}

pub(crate) fn rasterize_materialized_asset(
    name: String,
    source: &str,
    options: AtlasOptions,
) -> Result<AtlasRasterAsset> {
    let asset = rasterize(&name, source, options, options.max_asset_dimension)?;
    Ok(AtlasRasterAsset {
        name: asset.name,
        width: asset.width,
        height: asset.height,
        logical_width: asset.logical_width,
        logical_height: asset.logical_height,
        rgba: asset.rgba,
    })
}

fn decode_data_uri(name: &str, source: &str) -> Result<(String, Vec<u8>)> {
    let (header, payload) = source.split_once(',').ok_or_else(|| {
        Error::Invalid(format!(
            "renderer resource {name} is not a self-contained data URI"
        ))
    })?;
    let header = header.strip_prefix("data:").ok_or_else(|| {
        Error::Invalid(format!(
            "renderer resource {name} must use a base64 data URI"
        ))
    })?;
    let mut parts = header.split(';');
    let mime_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let parameters = parts.collect::<Vec<_>>();
    if parameters
        .last()
        .is_none_or(|parameter| !parameter.trim().eq_ignore_ascii_case("base64"))
    {
        return Err(Error::Invalid(format!(
            "renderer resource {name} must use a base64 data URI"
        )));
    }
    if mime_type.is_empty() {
        return Err(Error::Invalid(format!(
            "renderer resource {name} lacks a media type"
        )));
    }
    let bytes = STANDARD.decode(payload).map_err(|error| {
        Error::Invalid(format!(
            "renderer resource {name} has invalid base64: {error}"
        ))
    })?;
    Ok((mime_type, bytes))
}

fn rasterize_svg(
    name: &str,
    bytes: &[u8],
    options: AtlasOptions,
    maximum_dimension: u32,
) -> Result<RasterAsset> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default())
        .map_err(|error| Error::Invalid(format!("renderer SVG {name} is invalid: {error}")))?;
    let size = tree.size();
    let (width, height) = scaled_svg_dimensions(
        size.width(),
        size.height(),
        options.svg_scale,
        maximum_dimension,
    )?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| Error::Invalid(format!("renderer SVG {name} has an invalid raster size")))?;
    let transform = resvg::tiny_skia::Transform::from_scale(
        width as f32 / size.width(),
        height as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(RasterAsset {
        name: name.to_owned(),
        width,
        height,
        logical_width: size.width(),
        logical_height: size.height(),
        rgba: pixmap.take(),
    })
}

fn rasterize_image(name: &str, bytes: &[u8], maximum_dimension: u32) -> Result<RasterAsset> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| Error::Invalid(format!("renderer image {name} is invalid: {error}")))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(maximum_dimension);
    limits.max_image_height = Some(maximum_dimension);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| Error::Invalid(format!("renderer image {name} is invalid: {error}")))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(Error::Invalid(format!(
            "renderer image {name} has invalid dimensions"
        )));
    }
    let mut rgba = image.into_raw();
    premultiply_rgba(&mut rgba);
    Ok(RasterAsset {
        name: name.to_owned(),
        width,
        height,
        logical_width: width as f32,
        logical_height: height as f32,
        rgba,
    })
}

fn scaled_svg_dimensions(
    intrinsic_width: f32,
    intrinsic_height: f32,
    svg_scale: f32,
    maximum_dimension: u32,
) -> Result<(u32, u32)> {
    if !intrinsic_width.is_finite()
        || !intrinsic_height.is_finite()
        || intrinsic_width <= 0.0
        || intrinsic_height <= 0.0
    {
        return Err(Error::Invalid(
            "renderer SVG has invalid intrinsic dimensions".to_owned(),
        ));
    }
    let maximum = maximum_dimension as f32;
    let scale = svg_scale
        .min(maximum / intrinsic_width)
        .min(maximum / intrinsic_height);
    let width = (intrinsic_width * scale).ceil() as u32;
    let height = (intrinsic_height * scale).ceil() as u32;
    if width == 0 || height == 0 {
        return Err(Error::Invalid(
            "renderer SVG has invalid raster dimensions".to_owned(),
        ));
    }
    Ok((width.min(maximum_dimension), height.min(maximum_dimension)))
}

fn premultiply_rgba(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3] as u16;
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u16 * alpha + 127) / 255) as u8;
        }
    }
}

fn place_asset(plans: &mut Vec<PagePlan>, width: u32, height: u32, limit: u32) -> Placement {
    for (page_index, plan) in plans.iter_mut().enumerate() {
        for shelf in &mut plan.shelves {
            if height <= shelf.height && shelf.next_x + width <= limit {
                let placement = Placement {
                    page: page_index,
                    outer_x: shelf.next_x,
                    outer_y: shelf.y,
                };
                shelf.next_x += width;
                plan.used_width = plan.used_width.max(shelf.next_x);
                return placement;
            }
        }
        if plan.used_height + height <= limit {
            let y = plan.used_height;
            plan.shelves.push(Shelf {
                y,
                height,
                next_x: width,
            });
            plan.used_width = plan.used_width.max(width);
            plan.used_height += height;
            return Placement {
                page: page_index,
                outer_x: 0,
                outer_y: y,
            };
        }
    }
    plans.push(PagePlan {
        shelves: vec![Shelf {
            y: 0,
            height,
            next_x: width,
        }],
        used_width: width,
        used_height: height,
    });
    Placement {
        page: plans.len() - 1,
        outer_x: 0,
        outer_y: 0,
    }
}

fn blit_wrapped(page: &mut TextureAtlasPage, asset: &RasterAsset, x: u32, y: u32, padding: u32) {
    let start_x = x - padding;
    let start_y = y - padding;
    let outer_width = asset.width + padding * 2;
    let outer_height = asset.height + padding * 2;
    for destination_y in 0..outer_height {
        let relative_y = i64::from(destination_y) - i64::from(padding);
        let source_y = relative_y.rem_euclid(i64::from(asset.height)) as u32;
        for destination_x in 0..outer_width {
            let relative_x = i64::from(destination_x) - i64::from(padding);
            let source_x = relative_x.rem_euclid(i64::from(asset.width)) as u32;
            let source = ((source_y * asset.width + source_x) * 4) as usize;
            let destination =
                (((start_y + destination_y) * page.width + start_x + destination_x) * 4) as usize;
            page.rgba[destination..destination + 4]
                .copy_from_slice(&asset.rgba[source..source + 4]);
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use image::ImageEncoder;
    use serde_json::json;

    use crate::artifact::tests::{artifact_json, signed};
    use crate::{AtlasOptions, AtlasRasterAsset, ReplayArtifact, TextureAtlas};

    use super::scaled_svg_dimensions;

    fn with_resources(resources: serde_json::Value) -> ReplayArtifact {
        with_resources_and_decorations(resources, json!([]))
    }

    fn with_resources_and_decorations(
        resources: serde_json::Value,
        decorations: serde_json::Value,
    ) -> ReplayArtifact {
        let mut root: serde_json::Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = root["rendererContract"].as_object_mut().unwrap();
        contract.insert("resources".to_owned(), resources);
        contract.insert("decorations".to_owned(), decorations);
        contract.remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let fingerprint = root["rendererContract"]["fingerprint"].clone();
        let replay = root["replay"].as_object_mut().unwrap();
        replay.insert("rendererContractFingerprint".to_owned(), fingerprint);
        replay.remove("fingerprint");
        root["replay"] = signed(root["replay"].take());
        ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap()
    }

    #[test]
    fn builds_deterministic_premultiplied_svg_and_png_atlas() {
        let svg = STANDARD.encode(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="3"><rect width="2" height="3" fill="#ff0000"/></svg>"##,
        );
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[200, 100, 50, 128], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let artifact = with_resources(json!({
            "png": format!("data:image/png;base64,{}", STANDARD.encode(png)),
            "svg": format!("data:image/svg+xml;base64,{svg}")
        }));
        let options = AtlasOptions {
            svg_scale: 2.0,
            max_asset_dimension: 64,
            max_texture_dimension: 32,
            padding: 1,
        };
        let atlas = TextureAtlas::build(&artifact.renderer_contract, options).unwrap();
        assert_eq!(atlas.entries["svg"].width, 4);
        assert_eq!(atlas.entries["svg"].height, 6);
        assert_eq!(atlas.entries["svg"].logical_width, 2.0);
        assert_eq!(atlas.entries["svg"].logical_height, 3.0);
        assert_eq!(atlas.entries["png"].width, 1);
        assert_eq!(atlas.entries["png"].logical_width, 1.0);
        assert_eq!(atlas.pages.len(), 1);

        let png_entry = atlas.entries["png"];
        let page = &atlas.pages[png_entry.page as usize];
        let offset = ((png_entry.y * page.width + png_entry.x) * 4) as usize;
        assert_eq!(&page.rgba[offset..offset + 4], &[100, 50, 25, 128]);
        assert_eq!(
            TextureAtlas::build(&artifact.renderer_contract, options)
                .unwrap()
                .entries,
            atlas.entries,
        );
    }

    #[test]
    fn rejects_non_self_contained_or_unknown_resources() {
        let artifact = with_resources(json!({"bad": "texture.png"}));
        assert!(TextureAtlas::build(&artifact.renderer_contract, AtlasOptions::default()).is_err());
    }

    #[test]
    fn packs_materialized_landscape_urls_under_deterministic_names() {
        let png = STANDARD.encode({
            let mut bytes = Vec::new();
            image::codecs::png::PngEncoder::new(&mut bytes)
                .write_image(&[10, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .unwrap();
            bytes
        });
        let artifact = with_resources_and_decorations(
            json!({}),
            json!([{
                "decoration": {
                    "type": "floorLandscape",
                    "floorForegroundUrl": format!("data:image/png;base64,{png}")
                }
            }]),
        );
        let atlas =
            TextureAtlas::build(&artifact.renderer_contract, AtlasOptions::default()).unwrap();
        let name = crate::decoration_asset_name(0, &["decoration", "floorForegroundUrl"]);
        assert_eq!(atlas.entries[&name].width, 1);
        assert_eq!(atlas.entries[&name].height, 1);
    }

    #[test]
    fn preserves_official_2049_pixel_landscape_assets_without_relaxing_sprite_limits() {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &vec![255; 2_049 * 4],
                2_049,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let source = format!("data:image/png;base64,{}", STANDARD.encode(png));
        let decorated = with_resources_and_decorations(
            json!({}),
            json!([{
                "decoration": {
                    "type": "floorLandscape",
                    "floorForegroundUrl": source
                }
            }]),
        );
        let atlas =
            TextureAtlas::build(&decorated.renderer_contract, AtlasOptions::default()).unwrap();
        let name = crate::decoration_asset_name(0, &["decoration", "floorForegroundUrl"]);
        assert_eq!(atlas.entries[&name].width, 2_049);

        let ordinary = with_resources(json!({
            "oversized": decorated.renderer_contract.decorations[0]["decoration"]
                ["floorForegroundUrl"]
                .clone()
        }));
        assert!(TextureAtlas::build(&ordinary.renderer_contract, AtlasOptions::default()).is_err());
    }

    #[test]
    fn decodes_materialized_jpeg_and_webp_assets() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
            .write_image(&[20, 40, 60], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(&[10, 20, 30, 128], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let artifact = with_resources(json!({
            "jpeg": format!("data:image/jpeg;base64,{}", STANDARD.encode(jpeg)),
            "webp": format!("data:image/webp;base64,{}", STANDARD.encode(webp))
        }));
        let atlas =
            TextureAtlas::build(&artifact.renderer_contract, AtlasOptions::default()).unwrap();
        assert_eq!(atlas.entries["jpeg"].width, 1);
        assert_eq!(atlas.entries["webp"].width, 1);
        let webp_entry = atlas.entries["webp"];
        let page = &atlas.pages[webp_entry.page as usize];
        let offset = ((webp_entry.y * page.width + webp_entry.x) * 4) as usize;
        assert_eq!(&page.rgba[offset..offset + 4], &[5, 10, 15, 128]);
    }

    #[test]
    fn accepts_parameterized_case_insensitive_materialized_media_types() {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[10, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let artifact = with_resources(json!({
            "parameterized": format!(
                "data:IMAGE/PNG; charset=binary;BASE64,{}",
                STANDARD.encode(png)
            )
        }));
        let atlas =
            TextureAtlas::build(&artifact.renderer_contract, AtlasOptions::default()).unwrap();
        assert_eq!(atlas.entries["parameterized"].width, 1);
    }

    #[test]
    fn rejects_unmaterialized_decoration_urls() {
        let artifact = with_resources_and_decorations(
            json!({}),
            json!([{
                "decoration": {
                    "type": "floorLandscape",
                    "floorForegroundUrl": "https://example.invalid/floor.png"
                }
            }]),
        );
        assert!(TextureAtlas::build(&artifact.renderer_contract, AtlasOptions::default()).is_err());
    }

    #[test]
    fn empty_resource_contract_still_builds_a_bindable_transparent_page() {
        let artifact = ReplayArtifact::from_slice(&artifact_json()).unwrap();
        let atlas =
            TextureAtlas::build(&artifact.renderer_contract, AtlasOptions::default()).unwrap();
        assert!(atlas.entries.is_empty());
        assert_eq!(atlas.pages.len(), 1);
        assert_eq!(atlas.pages[0].rgba, vec![0, 0, 0, 0]);
    }

    #[test]
    fn packs_validated_procedural_rasters_with_contract_resources() {
        let artifact = ReplayArtifact::from_slice(&artifact_json()).unwrap();
        let asset = AtlasRasterAsset {
            name: "$graphics.test".to_owned(),
            width: 1,
            height: 1,
            logical_width: 0.0,
            logical_height: 0.0,
            rgba: vec![0, 0, 0, 0],
        };
        let atlas = TextureAtlas::build_with_raster_assets(
            &artifact.renderer_contract,
            AtlasOptions::default(),
            vec![asset],
        )
        .unwrap();
        assert_eq!(atlas.entries["$graphics.test"].logical_width, 0.0);

        let duplicate = || AtlasRasterAsset {
            name: "$graphics.duplicate".to_owned(),
            width: 1,
            height: 1,
            logical_width: 1.0,
            logical_height: 1.0,
            rgba: vec![0, 0, 0, 0],
        };
        assert!(
            TextureAtlas::build_with_raster_assets(
                &artifact.renderer_contract,
                AtlasOptions::default(),
                vec![duplicate(), duplicate()],
            )
            .is_err()
        );
    }

    #[test]
    fn normalizes_multiple_pages_for_texture_array_upload() {
        let large = STANDARD.encode(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="6" height="6"><rect width="6" height="6" fill="#fff"/></svg>"##,
        );
        let small = STANDARD.encode(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="#fff"/></svg>"##,
        );
        let artifact = with_resources(json!({
            "large": format!("data:image/svg+xml;base64,{large}"),
            "small": format!("data:image/svg+xml;base64,{small}")
        }));
        let atlas = TextureAtlas::build(
            &artifact.renderer_contract,
            AtlasOptions {
                svg_scale: 1.0,
                max_asset_dimension: 8,
                max_texture_dimension: 8,
                padding: 1,
            },
        )
        .unwrap();

        assert_eq!(atlas.pages.len(), 2);
        assert!(
            atlas
                .pages
                .iter()
                .all(|page| page.width == 8 && page.height == 8)
        );
        assert_eq!(atlas.entries["large"].page, 0);
        assert_eq!(atlas.entries["small"].page, 1);
        assert_eq!(atlas.entries["small"].u_max, 3.0 / 8.0);
        assert_eq!(atlas.entries["small"].v_max, 3.0 / 8.0);
    }

    #[test]
    fn oversized_svg_uses_one_aspect_preserving_scale() {
        let dimensions = scaled_svg_dimensions(2.0, 4.0, 10.0, 20).unwrap();
        assert_eq!(dimensions, (10, 20));
    }
}
