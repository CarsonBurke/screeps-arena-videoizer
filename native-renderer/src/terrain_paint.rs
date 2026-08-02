use serde_json::Value;

use crate::{
    Error, RendererContract, Result, TerrainGeometry, TerrainSwampTexture, decoration_asset_name,
};

const PIXI_TARGET_FPS: f64 = 60.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainLightingMode {
    Normal,
    Low,
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainTexturePaint {
    pub atlas_name: String,
    pub tint: u32,
    pub alpha: f32,
    pub tile_scale: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainPaintStyle {
    pub cell_size: f32,
    pub half_cell_size: f32,
    pub view_box: f32,
    pub lighting: TerrainLightingMode,
    pub wall_fill: u32,
    pub wall_stroke: u32,
    pub wall_lighting_fill: u32,
    pub wall_lighting_stroke: u32,
    pub wall_noise_alpha: Option<f32>,
    pub wall_noise_tile_scale: f32,
    pub wall_shadow_blur_pixels: f32,
    pub wall_foreground: Option<TerrainTexturePaint>,
    pub swamp_decoration_fill: Option<u32>,
    pub swamp_decoration_stroke: Option<u32>,
    pub swamp_tint: u32,
    pub swamp_noise_alpha: f32,
    pub floor_background: u32,
    pub floor_foreground: Option<TerrainTexturePaint>,
    pub default_ground_alpha: f32,
    pub default_ground_mask_alpha: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainNoisePaint {
    pub atlas_name: &'static str,
    pub tint: u32,
    /// Pixi sprite alpha before masking.
    pub alpha: f32,
    /// The retained processor gives each generated mask sprite alpha 0.25.
    pub mask_alpha: f32,
    pub tile_scale: f32,
    pub tile_position: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainFramePaint {
    pub swamp_fill: u32,
    pub swamp_stroke: u32,
    pub swamp_alpha: f32,
    pub swamp_noise: Vec<TerrainNoisePaint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerrainRampartPaint {
    pub user: u32,
    pub fill: u32,
    pub alpha_numerator: u8,
    pub alpha_denominator: u8,
}

impl TerrainPaintStyle {
    pub fn compile(contract: &RendererContract, raster_width: u32) -> Result<Self> {
        let options = contract
            .world_options
            .as_object()
            .ok_or_else(|| Error::Invalid("renderer worldOptions must be an object".to_owned()))?;
        let lighting = match options.get("lighting").and_then(Value::as_str) {
            None | Some("normal") => TerrainLightingMode::Normal,
            Some("low") => TerrainLightingMode::Low,
            Some("disabled") => TerrainLightingMode::Disabled,
            Some(value) => {
                return Err(Error::Invalid(format!(
                    "unsupported renderer lighting mode {value}"
                )));
            }
        };
        let view_box = match options.get("VIEW_BOX") {
            Some(value) => positive_f64(Some(value), "worldOptions.VIEW_BOX")?,
            None => {
                let room_size = positive_f64(options.get("ROOM_SIZE"), "worldOptions.ROOM_SIZE")?;
                let cell_size = positive_f64(options.get("CELL_SIZE"), "worldOptions.CELL_SIZE")?;
                let view_box = room_size * cell_size;
                if !view_box.is_finite() {
                    return Err(Error::Invalid(
                        "renderer default VIEW_BOX exceeds the supported range".to_owned(),
                    ));
                }
                view_box
            }
        };
        let cell_size = positive_f64(options.get("CELL_SIZE"), "worldOptions.CELL_SIZE")?;
        let half_cell_size = match options.get("HALF_CELL_SIZE") {
            Some(value) => finite_f64(Some(value), "worldOptions.HALF_CELL_SIZE")?,
            None => cell_size / 2.0,
        };
        let wall = find_landscape(&contract.decorations, &["landscape", "wallLandscape"]);
        let floor = find_landscape(&contract.decorations, &["landscape", "floorLandscape"]);

        let (wall_fill, wall_stroke, wall_lighting_stroke, wall_foreground) =
            if let Some((index, decoration)) = wall {
                let mut fill = color_brightness(
                    required_color(decoration, "backgroundColor")?,
                    unit_number(decoration, "backgroundBrightness")?,
                );
                let mut stroke = color_brightness(
                    required_color(decoration, "strokeColor")?,
                    unit_number(decoration, "strokeBrightness")?,
                );
                if lighting == TerrainLightingMode::Disabled {
                    fill = multiply(fill, 0.5);
                    stroke = multiply(stroke, 0.3);
                }
                let light = grayscale(unit_number(decoration, "strokeLighting")?);
                let foreground = TerrainTexturePaint {
                    atlas_name: decoration_asset_name(index, &["decoration", "foregroundUrl"]),
                    tint: {
                        let tint = color_brightness(
                            required_color(decoration, "foregroundColor")?,
                            unit_number(decoration, "foregroundBrightness")?,
                        );
                        if lighting == TerrainLightingMode::Disabled {
                            multiply(tint, 0.6)
                        } else {
                            tint
                        }
                    },
                    alpha: unit_number(decoration, "foregroundAlpha")? as f32,
                    tile_scale: None,
                };
                (fill, stroke, light, Some(foreground))
            } else {
                (
                    if lighting == TerrainLightingMode::Disabled {
                        0x18_18_18
                    } else {
                        0x11_11_11
                    },
                    0,
                    0,
                    None,
                )
            };

        let (swamp_decoration_fill, swamp_decoration_stroke, floor_background, floor_foreground) =
            if let Some((index, decoration)) = floor {
                let mut background = color_brightness(
                    required_color(decoration, "floorBackgroundColor")?,
                    unit_number(decoration, "floorBackgroundBrightness")?,
                );
                let mut foreground_tint = color_brightness(
                    required_color(decoration, "floorForegroundColor")?,
                    unit_number(decoration, "floorForegroundBrightness")?,
                );
                let light_factor = match lighting {
                    TerrainLightingMode::Normal => 1.0,
                    TerrainLightingMode::Low => 0.65,
                    TerrainLightingMode::Disabled => 0.5,
                };
                if light_factor != 1.0 {
                    background = multiply(background, light_factor);
                    foreground_tint = multiply(foreground_tint, light_factor);
                }
                let tile_scale = decoration
                    .get("decoration")
                    .and_then(|value| value.get("tileScale"))
                    .map(tile_scale)
                    .transpose()?
                    .flatten();
                (
                    Some(required_color(decoration, "swampColor")?),
                    Some(required_color(decoration, "swampStrokeColor")?),
                    background,
                    Some(TerrainTexturePaint {
                        atlas_name: decoration_asset_name(
                            index,
                            &["decoration", "floorForegroundUrl"],
                        ),
                        tint: foreground_tint,
                        alpha: unit_number(decoration, "floorForegroundAlpha")? as f32,
                        tile_scale,
                    }),
                )
            } else {
                (
                    None,
                    None,
                    match lighting {
                        TerrainLightingMode::Normal => 0x55_55_55,
                        TerrainLightingMode::Low => 0x35_35_35,
                        TerrainLightingMode::Disabled => 0x20_20_20,
                    },
                    None,
                )
            };

        Ok(Self {
            cell_size: cell_size as f32,
            half_cell_size: half_cell_size as f32,
            view_box: view_box as f32,
            lighting,
            wall_fill,
            wall_stroke,
            wall_lighting_fill: 0x80_80_80,
            wall_lighting_stroke,
            wall_noise_alpha: (lighting != TerrainLightingMode::Disabled).then_some(0.2),
            // Pixi first tiles noise1 into the wall-sized render texture, then
            // stretches that texture back over VIEW_BOX. Those two scale
            // factors cancel, so a direct world-space draw must stay at 8.
            wall_noise_tile_scale: 8.0,
            wall_shadow_blur_pixels: (f64::from(raster_width) * 0.006_f64) as f32,
            wall_foreground,
            swamp_decoration_fill,
            swamp_decoration_stroke,
            swamp_tint: match lighting {
                TerrainLightingMode::Normal => 0xff_ff_ff,
                TerrainLightingMode::Low => 0xa0_a0_a0,
                TerrainLightingMode::Disabled => 0x80_80_80,
            },
            swamp_noise_alpha: if lighting == TerrainLightingMode::Normal {
                0.3
            } else {
                0.15
            },
            floor_background,
            floor_foreground,
            default_ground_alpha: match lighting {
                TerrainLightingMode::Normal => 0.3,
                TerrainLightingMode::Low => 0.1,
                TerrainLightingMode::Disabled => 0.2,
            },
            default_ground_mask_alpha: match lighting {
                TerrainLightingMode::Normal => 0.15,
                TerrainLightingMode::Low => 0.05,
                TerrainLightingMode::Disabled => 0.1,
            },
        })
    }

    /// Compile time-varying terrain paint. `phase_seconds` is measured from
    /// the geometry span's `swamp_animation_start_tick`, because the retained
    /// Pixi processor recreates and rephases its tiling sprites only when the
    /// swamp path changes.
    pub fn frame(
        &self,
        geometry: &TerrainGeometry,
        phase_seconds: f64,
    ) -> Result<TerrainFramePaint> {
        if !phase_seconds.is_finite() || phase_seconds < 0.0 {
            return Err(Error::Invalid(
                "terrain animation phase must be finite and nonnegative".to_owned(),
            ));
        }
        let textured = geometry.swamp_texture != TerrainSwampTexture::Disabled;
        let swamp_fill =
            self.swamp_decoration_fill
                .unwrap_or(if textured { 0x4a_50_1e } else { 0x46_5c_03 });
        let swamp_stroke =
            self.swamp_decoration_stroke
                .unwrap_or(if textured { 0x4a_50_1e } else { 0x3b_40_19 });
        let swamp_noise = if textured {
            let animated = geometry.swamp_texture == TerrainSwampTexture::Animated;
            [(10.0, 1.5), (14.0, -1.35)]
                .into_iter()
                .map(|(tile_scale, velocity)| {
                    let position = if animated {
                        (velocity * PIXI_TARGET_FPS * phase_seconds) as f32
                    } else {
                        0.0
                    };
                    TerrainNoisePaint {
                        atlas_name: "noise2",
                        tint: 0x66_ff_00,
                        alpha: self.swamp_noise_alpha,
                        mask_alpha: 0.25,
                        tile_scale,
                        tile_position: [position, position],
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(TerrainFramePaint {
            swamp_fill,
            swamp_stroke,
            swamp_alpha: 0.4,
            swamp_noise,
        })
    }

    pub fn ramparts(&self, geometry: &TerrainGeometry) -> Vec<(String, TerrainRampartPaint)> {
        geometry
            .private_rampart_colors
            .iter()
            .map(|(user, color)| {
                (
                    user.clone(),
                    TerrainRampartPaint {
                        user: *color,
                        fill: multiply(*color, 0.3),
                        alpha_numerator: 2,
                        alpha_denominator: 5,
                    },
                )
            })
            .collect()
    }
}

fn find_landscape<'a>(decorations: &'a [Value], kinds: &[&str]) -> Option<(usize, &'a Value)> {
    decorations.iter().enumerate().find(|(_, item)| {
        item.get("decoration")
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| kinds.contains(&kind))
    })
}

fn required_color(object: &Value, field: &str) -> Result<u32> {
    object
        .get(field)
        .and_then(Value::as_str)
        .and_then(parse_color)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "renderer landscape decoration {field} must be #RRGGBB"
            ))
        })
}

fn parse_color(value: &str) -> Option<u32> {
    let digits = value.strip_prefix('#')?;
    (digits.len() == 6)
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
}

fn unit_number(object: &Value, field: &str) -> Result<f64> {
    object
        .get(field)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .ok_or_else(|| {
            Error::Invalid(format!(
                "renderer landscape decoration {field} must be within 0..=1"
            ))
        })
}

fn positive_f64(value: Option<&Value>, field: &str) -> Result<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| Error::Invalid(format!("renderer {field} must be positive")))
}

fn finite_f64(value: Option<&Value>, field: &str) -> Result<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| Error::Invalid(format!("renderer {field} must be finite")))
}

fn tile_scale(value: &Value) -> Result<Option<f32>> {
    value
        .as_f64()
        .filter(|value| value.is_finite())
        .map(|value| (value != 0.0).then_some(value as f32))
        .ok_or_else(|| Error::Invalid("renderer decoration.tileScale must be finite".to_owned()))
}

fn multiply(color: u32, factor: f64) -> u32 {
    let [red, green, blue] = channels(color);
    pack([
        (f64::from(red) * factor).round() as u8,
        (f64::from(green) * factor).round() as u8,
        (f64::from(blue) * factor).round() as u8,
    ])
}

fn color_brightness(color: u32, brightness: f64) -> u32 {
    let [red, green, blue] = channels(color);
    let red = f64::from(red) / 255.0;
    let green = f64::from(green) / 255.0;
    let blue = f64::from(blue) / 255.0;
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let lightness = (max + min) / 2.0;
    let (hue, saturation) = if max == min {
        (0.0, 0.0)
    } else {
        let delta = max - min;
        let saturation = if lightness > 0.5 {
            delta / (2.0 - max - min)
        } else {
            delta / (max + min)
        };
        let hue = if max == red {
            (green - blue) / delta + if green < blue { 6.0 } else { 0.0 }
        } else if max == green {
            (blue - red) / delta + 2.0
        } else {
            (red - green) / delta + 4.0
        } / 6.0;
        (hue, saturation)
    };
    pack(hsl_to_rgb(hue, saturation, lightness * brightness))
}

fn grayscale(lightness: f64) -> u32 {
    pack(hsl_to_rgb(0.0, 0.0, lightness))
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> [u8; 3] {
    let (red, green, blue) = if saturation == 0.0 {
        (lightness, lightness, lightness)
    } else {
        let q = if lightness < 0.5 {
            lightness * (1.0 + saturation)
        } else {
            lightness + saturation - lightness * saturation
        };
        let p = 2.0 * lightness - q;
        (
            hue_to_rgb(p, q, hue + 1.0 / 3.0),
            hue_to_rgb(p, q, hue),
            hue_to_rgb(p, q, hue - 1.0 / 3.0),
        )
    };
    [
        (red * 255.0).round() as u8,
        (green * 255.0).round() as u8,
        (blue * 255.0).round() as u8,
    ]
}

fn hue_to_rgb(p: f64, q: f64, mut value: f64) -> f64 {
    if value < 0.0 {
        value += 1.0;
    }
    if value > 1.0 {
        value -= 1.0;
    }
    if value < 1.0 / 6.0 {
        p + (q - p) * 6.0 * value
    } else if value < 0.5 {
        q
    } else if value < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - value) * 6.0
    } else {
        p
    }
}

fn channels(color: u32) -> [u8; 3] {
    [
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
    ]
}

fn pack([red, green, blue]: [u8; 3]) -> u32 {
    (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::artifact::{Nullable, RendererContract, RendererInventory};
    use crate::{
        TerrainGeometry, TerrainLightingMode, TerrainPaintStyle, TerrainSwampTexture,
        decoration_asset_name,
    };

    fn contract(lighting: &str, decorations: Vec<serde_json::Value>) -> RendererContract {
        RendererContract {
            schema: "screeps-arena-renderer-contract".to_owned(),
            version: 5,
            renderer_version: Nullable(Some("test".to_owned())),
            metadata: json!({}),
            resources: json!({}),
            decorations,
            terrain: Vec::new(),
            world_options: json!({
                "CELL_SIZE": 100,
                "ROOM_SIZE": 100,
                "VIEW_BOX": 10000,
                "lighting": lighting
            }),
            inventory: RendererInventory {
                object_types: Vec::new(),
                processor_types: Vec::new(),
                action_types: Vec::new(),
                preprocessors: Vec::new(),
                calculation_ids: Vec::new(),
                drawing_methods: Vec::new(),
                expression_operators: Vec::new(),
                function_semantics: Vec::new(),
                layer_ids: Vec::new(),
                renderer_implementation_fingerprints: Vec::new(),
            },
            fingerprint: "ab".repeat(32),
        }
    }

    fn contract_without_view_box(
        lighting: &str,
        decorations: Vec<serde_json::Value>,
    ) -> RendererContract {
        let mut contract = contract(lighting, decorations);
        contract.world_options = json!({
            "CELL_SIZE": 100,
            "ROOM_SIZE": 100,
            "lighting": lighting
        });
        contract
    }

    fn geometry(mode: TerrainSwampTexture) -> TerrainGeometry {
        TerrainGeometry {
            room_size: 100,
            view_box: 10_000,
            wall_path: None,
            swamp_path: Some("M 0 0".to_owned()),
            private_rampart_paths: BTreeMap::from([("owner".to_owned(), "M 0 0".to_owned())]),
            private_rampart_colors: BTreeMap::from([("owner".to_owned(), 0x83_2b_cd)]),
            swamp_texture: mode,
            fingerprint: "cd".repeat(32),
        }
    }

    #[test]
    fn compiles_official_default_modes_and_animation_phase() {
        let style = TerrainPaintStyle::compile(&contract("normal", Vec::new()), 2_048).unwrap();
        assert_eq!(style.lighting, TerrainLightingMode::Normal);
        assert_eq!(style.wall_fill, 0x11_11_11);
        assert_eq!(style.wall_noise_alpha, Some(0.2));
        assert_eq!(style.floor_background, 0x55_55_55);
        assert_eq!(style.wall_noise_tile_scale, 8.0);
        let frame = style
            .frame(&geometry(TerrainSwampTexture::Animated), 2.0)
            .unwrap();
        assert_eq!(frame.swamp_fill, 0x4a_50_1e);
        assert_eq!(frame.swamp_alpha, 0.4);
        assert_eq!(frame.swamp_noise.len(), 2);
        assert_eq!(frame.swamp_noise[0].mask_alpha, 0.25);
        assert_eq!(frame.swamp_noise[0].tile_position, [180.0, 180.0]);
        assert_eq!(frame.swamp_noise[1].tile_position, [-162.0, -162.0]);
        assert_eq!(
            style.ramparts(&geometry(TerrainSwampTexture::Animated))[0]
                .1
                .fill,
            0x27_0d_3e
        );
    }

    #[test]
    fn disabled_mode_removes_noise_and_uses_dark_defaults() {
        let style = TerrainPaintStyle::compile(&contract("disabled", Vec::new()), 1_000).unwrap();
        assert_eq!(style.wall_fill, 0x18_18_18);
        assert_eq!(style.wall_noise_alpha, None);
        assert_eq!(style.floor_background, 0x20_20_20);
        let frame = style
            .frame(&geometry(TerrainSwampTexture::Disabled), 5.0)
            .unwrap();
        assert_eq!(frame.swamp_fill, 0x46_5c_03);
        assert_eq!(frame.swamp_stroke, 0x3b_40_19);
        assert!(frame.swamp_noise.is_empty());
    }

    #[test]
    fn wall_blur_uses_javascripts_double_then_webgl_float_cast() {
        let style = TerrainPaintStyle::compile(&contract("normal", Vec::new()), 1_365).unwrap();
        assert_eq!(style.wall_shadow_blur_pixels, (1_365.0_f64 * 0.006) as f32);
    }

    #[test]
    fn defaults_view_box_and_treats_zero_tile_scale_as_an_untiled_sprite() {
        let decorations = vec![json!({
            "backgroundColor": "#111111",
            "backgroundBrightness": 1.0,
            "strokeColor": "#000000",
            "strokeBrightness": 1.0,
            "strokeLighting": 0.0,
            "foregroundColor": "#FFFFFF",
            "foregroundBrightness": 1.0,
            "foregroundAlpha": 1.0,
            "floorBackgroundColor": "#555555",
            "floorBackgroundBrightness": 1.0,
            "floorForegroundColor": "#FFFFFF",
            "floorForegroundBrightness": 1.0,
            "floorForegroundAlpha": 1.0,
            "swampColor": "#4A501E",
            "swampStrokeColor": "#4A501E",
            "decoration": {
                "type": "landscape",
                "foregroundUrl": "data:image/png;base64,AA==",
                "floorForegroundUrl": "data:image/png;base64,AA==",
                "tileScale": 0
            }
        })];
        let style =
            TerrainPaintStyle::compile(&contract_without_view_box("normal", decorations), 2_048)
                .unwrap();
        assert_eq!(style.wall_noise_tile_scale, 8.0);
        assert_eq!(style.floor_foreground.unwrap().tile_scale, None);
    }

    #[test]
    fn direct_wall_noise_scale_cancels_the_intermediate_render_texture_size() {
        for raster_width in [640, 1_080, 2_048, 4_096] {
            let style =
                TerrainPaintStyle::compile(&contract("normal", Vec::new()), raster_width).unwrap();
            assert_eq!(style.wall_noise_tile_scale, 8.0);
        }
    }

    #[test]
    fn first_landscape_decoration_controls_exact_paint_and_asset_keys() {
        let style = TerrainPaintStyle::compile(
            &contract(
                "low",
                vec![json!({
                    "backgroundColor": "#03142D",
                    "backgroundBrightness": 1.0,
                    "strokeColor": "#111A28",
                    "strokeBrightness": 1.0,
                    "strokeLighting": 0.5,
                    "foregroundColor": "#597AA1",
                    "foregroundBrightness": 0.6,
                    "foregroundAlpha": 1.0,
                    "floorBackgroundColor": "#3B6EA9",
                    "floorBackgroundBrightness": 1.0,
                    "floorForegroundColor": "#479CFF",
                    "floorForegroundBrightness": 1.0,
                    "floorForegroundAlpha": 1.0,
                    "swampColor": "#FF1F1F",
                    "swampStrokeColor": "#C70F0F",
                    "decoration": {
                        "type": "landscape",
                        "foregroundUrl": "data:image/png;base64,AA==",
                        "floorForegroundUrl": "data:image/png;base64,AA==",
                        "tileScale": 2
                    }
                })],
            ),
            2_048,
        )
        .unwrap();
        assert_eq!(style.wall_fill, 0x03_14_2d);
        assert_eq!(style.wall_lighting_stroke, 0x80_80_80);
        assert_eq!(style.swamp_decoration_fill, Some(0xff_1f_1f));
        assert_eq!(style.floor_background, 0x26_48_6e);
        assert_eq!(
            style.wall_foreground.unwrap().atlas_name,
            decoration_asset_name(0, &["decoration", "foregroundUrl"])
        );
        assert_eq!(
            style.floor_foreground.unwrap().atlas_name,
            decoration_asset_name(0, &["decoration", "floorForegroundUrl"])
        );
    }
}
