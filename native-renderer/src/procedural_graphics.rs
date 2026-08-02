use std::collections::BTreeMap;

use resvg::tiny_skia::{FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::{
    AtlasOptions, AtlasRasterAsset, Error, ProcessorKind, ResolvedActivation, ResolvedScene,
    ResolvedValue, Result,
};

#[derive(Clone, Copy, Debug, PartialEq)]
struct CircleSpec {
    fill: Option<u32>,
    radius: f64,
    stroke: Option<(u32, f64)>,
}

pub fn procedural_graphics_assets(
    scene: &ResolvedScene,
    options: AtlasOptions,
) -> Result<Vec<AtlasRasterAsset>> {
    options.validate()?;
    let mut circles = BTreeMap::<String, CircleSpec>::new();
    let mut materialized = BTreeMap::<String, String>::new();
    for activation in &scene.activations {
        let ResolvedActivation::Processor { kind, payload, .. } = activation else {
            continue;
        };
        if !matches!(
            kind,
            ProcessorKind::Circle | ProcessorKind::ResourceCircle | ProcessorKind::UserBadge
        ) {
            continue;
        }
        if matches!(payload, ResolvedValue::Undefined) {
            continue;
        }
        let payload = payload
            .as_object()
            .ok_or_else(|| Error::Invalid("circle payload must resolve to an object".to_owned()))?;
        if *kind == ProcessorKind::UserBadge
            && let Some(texture) = payload.get("texture")
        {
            if !crate::value_plan::resolved_js_truthy(texture) {
                continue;
            }
            let texture = texture.as_string().ok_or_else(|| {
                Error::Invalid("userBadge texture must resolve to a resource name".to_owned())
            })?;
            if texture.starts_with("data:") {
                materialized
                    .entry(texture.to_owned())
                    .or_insert_with(|| texture.to_owned());
            }
            continue;
        }
        let spec = CircleSpec::parse(payload)?;
        circles.entry(spec.asset_name()).or_insert(spec);
    }
    let mut assets = circles
        .into_iter()
        .map(|(name, spec)| spec.rasterize(name, options))
        .collect::<Result<Vec<_>>>()?;
    assets.extend(
        materialized
            .into_iter()
            .map(|(name, source)| {
                crate::assets::rasterize_materialized_asset(name, &source, options)
            })
            .collect::<Result<Vec<_>>>()?,
    );
    assets.extend(crate::text_raster::text_raster_assets(
        scene.activations.iter().filter_map(|activation| {
            let ResolvedActivation::Processor {
                kind: ProcessorKind::Text,
                payload,
                ..
            } = activation
            else {
                return None;
            };
            (payload.get("$nativeTextRaster") == Some(&ResolvedValue::Bool(true)))
                .then(|| payload.clone())
        }),
    )?);
    Ok(assets)
}

pub(crate) fn circle_asset_geometry(
    payload: &BTreeMap<String, ResolvedValue>,
) -> Result<(String, [f64; 2])> {
    let spec = CircleSpec::parse(payload)?;
    let size = spec.logical_size();
    Ok((spec.asset_name(), [size, size]))
}

impl CircleSpec {
    fn parse(payload: &BTreeMap<String, ResolvedValue>) -> Result<Self> {
        let fill = match payload.get("color") {
            None | Some(ResolvedValue::Undefined) | Some(ResolvedValue::Null) => None,
            Some(value) => Some(graphics_color(value, "circle color")?),
        };
        let radius = optional_number(payload.get("radius"), 25.0, "circle radius")?;
        if radius < 0.0 {
            return Err(Error::Invalid(
                "circle radius cannot be negative".to_owned(),
            ));
        }
        let stroke = match payload.get("strokeWidth") {
            Some(value) if crate::value_plan::resolved_js_truthy(value) => {
                let width = required_number(value, "circle strokeWidth")?;
                if width < 0.0 {
                    return Err(Error::Invalid(
                        "circle strokeWidth cannot be negative".to_owned(),
                    ));
                }
                let color = match payload.get("stroke") {
                    None | Some(ResolvedValue::Undefined) => 0,
                    Some(value) => graphics_color(value, "circle stroke")?,
                };
                Some((color, width))
            }
            _ => None,
        };
        Ok(Self {
            fill,
            radius,
            stroke,
        })
    }

    fn asset_name(self) -> String {
        let fill = self.fill.map_or(u32::MAX, |value| value);
        let extent = self.extent();
        let normalized_radius = if extent == 0.0 {
            0.0
        } else {
            self.radius / extent
        };
        let (stroke, normalized_stroke_width) =
            self.stroke.map_or((u32::MAX, 0_u64), |(color, width)| {
                (
                    color,
                    if extent == 0.0 { 0.0 } else { width / extent }.to_bits(),
                )
            });
        format!(
            "$graphics.circle:{fill:08x}:{:016x}:{stroke:08x}:{normalized_stroke_width:016x}",
            normalized_radius.to_bits()
        )
    }

    fn extent(self) -> f64 {
        self.radius + self.stroke.map_or(0.0, |(_, width)| width / 2.0)
    }

    fn logical_size(self) -> f64 {
        self.extent() * 2.0
    }

    fn rasterize(self, name: String, options: AtlasOptions) -> Result<AtlasRasterAsset> {
        let extent = self.extent();
        if !extent.is_finite() {
            return Err(Error::Invalid(
                "circle raster bounds are unsupported".to_owned(),
            ));
        }
        if extent == 0.0 {
            return Ok(AtlasRasterAsset {
                name,
                width: 1,
                height: 1,
                logical_width: 0.0,
                logical_height: 0.0,
                rgba: vec![0; 4],
            });
        }
        if self.fill.is_none() && self.stroke.is_none() {
            return Ok(AtlasRasterAsset {
                name,
                width: 1,
                height: 1,
                logical_width: 1.0,
                logical_height: 1.0,
                rgba: vec![0; 4],
            });
        }
        // The true logical bounds live on each scene node. This normalized
        // raster is shared by every absolute radius with the same fill/stroke
        // proportions, bounding atlas memory independently of replay length.
        let size = options.max_asset_dimension;
        let logical_size = size as f32 / options.svg_scale;
        let mut pixmap = Pixmap::new(size, size)
            .ok_or_else(|| Error::Invalid("circle raster dimensions are invalid".to_owned()))?;
        let center = size as f32 / 2.0;
        let radius = (self.radius / (extent * 2.0)) as f32 * size as f32;
        if radius > 0.0 {
            let path = PathBuilder::from_circle(center, center, radius)
                .ok_or_else(|| Error::Invalid("circle path dimensions are invalid".to_owned()))?;
            if let Some(fill) = self.fill {
                let mut paint = Paint::default();
                set_color(&mut paint, fill);
                pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            if let Some((color, width)) = self.stroke {
                let mut paint = Paint::default();
                set_color(&mut paint, color);
                let stroke = Stroke {
                    width: (width / (extent * 2.0)) as f32 * size as f32,
                    line_cap: LineCap::Butt,
                    ..Stroke::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        Ok(AtlasRasterAsset {
            name,
            width: size,
            height: size,
            logical_width: logical_size,
            logical_height: logical_size,
            rgba: pixmap.take(),
        })
    }
}

fn optional_number(value: Option<&ResolvedValue>, default: f64, label: &str) -> Result<f64> {
    match value {
        None | Some(ResolvedValue::Undefined) => Ok(default),
        Some(value) => required_number(value, label),
    }
}

fn required_number(value: &ResolvedValue, label: &str) -> Result<f64> {
    match value {
        ResolvedValue::Number(value) if value.is_finite() => Ok(*value),
        _ => Err(Error::Invalid(format!("{label} must resolve to a number"))),
    }
}

fn graphics_color(value: &ResolvedValue, label: &str) -> Result<u32> {
    Ok(required_number(value, label)?
        .floor()
        .clamp(0.0, 16_777_215.0) as u32)
}

fn set_color(paint: &mut Paint<'_>, color: u32) {
    paint.set_color_rgba8(
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
        255,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use image::ImageEncoder;

    use super::{CircleSpec, procedural_graphics_assets};
    use crate::{AtlasOptions, ProcessorKind, ResolvedActivation, ResolvedScene, ResolvedValue};

    fn resource_activation(activation_order: u32, payload: ResolvedValue) -> ResolvedActivation {
        ResolvedActivation::Processor {
            entity_id: "energy".to_owned(),
            object_type: "energy".to_owned(),
            definition_id: format!("resource-{activation_order}"),
            scope_id: "resource".to_owned(),
            kind: ProcessorKind::ResourceCircle,
            layer: None,
            z_index: 0.0,
            activation_order,
            start_tick: activation_order,
            end_tick: activation_order + 1,
            payload,
            object_texture: None,
            node_id: None,
            target_is_root: false,
            touches_node: false,
            temporary_node: false,
            actions: Vec::new(),
        }
    }

    #[test]
    fn resource_circle_assets_include_changes_and_skip_early_returns() {
        let payload = ResolvedValue::Object(BTreeMap::from([
            ("color".to_owned(), ResolvedValue::Number(0xff_e5_6d as f64)),
            ("radius".to_owned(), ResolvedValue::Number(30.0)),
        ]));
        let scene = ResolvedScene {
            activations: vec![
                resource_activation(1, payload),
                resource_activation(2, ResolvedValue::Undefined),
            ],
            final_random_state: 0,
        };
        let assets = procedural_graphics_assets(&scene, AtlasOptions::default()).unwrap();
        assert_eq!(assets.len(), 1);
        assert!(assets[0].rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn user_badge_assets_include_fallback_circles_and_materialized_images() {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[200, 100, 50, 128], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let source = format!("data:image/png;base64,{}", STANDARD.encode(png));
        let activation = |activation_order, payload: ResolvedValue| {
            let temporary_node = payload.get("texture").is_some();
            ResolvedActivation::Processor {
                entity_id: "badge".to_owned(),
                object_type: "badge".to_owned(),
                definition_id: format!("badge-{activation_order}"),
                scope_id: "badge".to_owned(),
                kind: ProcessorKind::UserBadge,
                layer: None,
                z_index: 0.0,
                activation_order,
                start_tick: activation_order,
                end_tick: activation_order + 1,
                payload,
                object_texture: None,
                node_id: (!temporary_node).then(|| "badge".to_owned()),
                target_is_root: false,
                touches_node: !temporary_node,
                temporary_node,
                actions: Vec::new(),
            }
        };
        let scene = ResolvedScene {
            activations: vec![
                activation(
                    1,
                    ResolvedValue::Object(BTreeMap::from([
                        ("texture".to_owned(), ResolvedValue::String(source.clone())),
                        ("width".to_owned(), ResolvedValue::Number(52.0)),
                    ])),
                ),
                activation(
                    2,
                    ResolvedValue::Object(BTreeMap::from([
                        ("color".to_owned(), ResolvedValue::Number(0x22_22_22 as f64)),
                        ("radius".to_owned(), ResolvedValue::Number(26.0)),
                    ])),
                ),
                activation(
                    3,
                    ResolvedValue::Object(BTreeMap::from([(
                        "texture".to_owned(),
                        ResolvedValue::Null,
                    )])),
                ),
            ],
            final_random_state: 0,
        };

        let assets = procedural_graphics_assets(&scene, AtlasOptions::default()).unwrap();
        assert_eq!(assets.len(), 2);
        assert!(assets.iter().any(|asset| asset.name == source));
        assert!(
            assets
                .iter()
                .any(|asset| asset.name.starts_with("$graphics.circle:"))
        );
    }

    #[test]
    fn circle_raster_is_deterministic_premultiplied_and_supersampled() {
        let payload = BTreeMap::from([
            ("color".to_owned(), ResolvedValue::Number(0xff_00_00 as f64)),
            ("radius".to_owned(), ResolvedValue::Number(10.0)),
            (
                "stroke".to_owned(),
                ResolvedValue::Number(0x00_ff_00 as f64),
            ),
            ("strokeWidth".to_owned(), ResolvedValue::Number(2.0)),
        ]);
        let spec = CircleSpec::parse(&payload).unwrap();
        let first = spec
            .rasterize(spec.asset_name(), AtlasOptions::default())
            .unwrap();
        let second = spec
            .rasterize(spec.asset_name(), AtlasOptions::default())
            .unwrap();
        assert_eq!(first.name, second.name);
        assert_eq!(first.width, AtlasOptions::default().max_asset_dimension);
        assert_eq!(
            first.logical_width,
            AtlasOptions::default().max_asset_dimension as f32 / AtlasOptions::default().svg_scale
        );
        assert_eq!(first.rgba, second.rgba);
        assert!(first.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
        assert!(
            first
                .rgba
                .chunks_exact(4)
                .all(|pixel| pixel[..3].iter().all(|channel| *channel <= pixel[3]))
        );
    }

    #[test]
    fn zero_radius_keeps_an_empty_action_target_asset() {
        let payload = BTreeMap::from([("radius".to_owned(), ResolvedValue::Number(0.0))]);
        let spec = CircleSpec::parse(&payload).unwrap();
        let asset = spec
            .rasterize(spec.asset_name(), AtlasOptions::default())
            .unwrap();
        assert_eq!(asset.rgba, [0, 0, 0, 0]);
        assert_eq!(asset.logical_width, 0.0);
    }

    #[test]
    fn absolute_radius_does_not_multiply_atlas_variants_or_exceed_limits() {
        let small = CircleSpec::parse(&BTreeMap::from([
            ("color".to_owned(), ResolvedValue::Number(1.0)),
            ("radius".to_owned(), ResolvedValue::Number(20.0)),
        ]))
        .unwrap();
        let nuke = CircleSpec::parse(&BTreeMap::from([
            ("color".to_owned(), ResolvedValue::Number(1.0)),
            ("radius".to_owned(), ResolvedValue::Number(600.0)),
        ]))
        .unwrap();
        assert_eq!(small.asset_name(), nuke.asset_name());
        assert_eq!(small.logical_size(), 40.0);
        assert_eq!(nuke.logical_size(), 1200.0);
        assert_eq!(
            nuke.rasterize(nuke.asset_name(), AtlasOptions::default())
                .unwrap()
                .width,
            AtlasOptions::default().max_asset_dimension
        );
    }
}
