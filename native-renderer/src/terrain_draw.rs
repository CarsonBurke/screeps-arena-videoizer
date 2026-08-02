use crate::{
    Error, Result, SpriteBlendMode, TerrainFramePaint, TerrainGeometry, TerrainLightingMode,
    TerrainPaintStyle, TerrainTexturePaint, TextureAtlas,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainDrawPhase {
    Terrain,
    WallGraffiti,
    Lighting,
    Effects,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerrainCoverage {
    WallFill,
    WallStroke,
    SwampFill,
    SwampStroke,
    PrivateRampartFill(String),
    PrivateRampartStroke(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainPlacement {
    pub origin: [f32; 2],
    pub size: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainTextureSample {
    pub atlas_name: String,
    pub tint: u32,
    /// Pixi leaves this enabled for bundled textures, but explicitly disables
    /// it on landscape floor/wall tilers.
    pub mipmap: bool,
    /// `None` is a stretched Pixi Sprite; `Some` is a TilingSprite.
    pub tile_scale: Option<[f32; 2]>,
    pub tile_position: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub enum TerrainDrawSource {
    Solid {
        color: u32,
    },
    Texture(TerrainTextureSample),
    StyledCoverage {
        fill: TerrainCoverage,
        stroke: TerrainCoverage,
        fill_color: u32,
        stroke_color: u32,
        tint: u32,
        /// The wall processor precomposes its masked additive noise into the
        /// wall render texture before that texture enters the terrain layer.
        additive_texture: Option<(TerrainTextureSample, TerrainCoverage, f32)>,
    },
    Coverage {
        coverage: TerrainCoverage,
        color: u32,
        blur_pixels: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainDrawOp {
    pub phase: TerrainDrawPhase,
    pub z_index: i32,
    pub placement: TerrainPlacement,
    pub source: TerrainDrawSource,
    pub mask: Option<(TerrainCoverage, f32)>,
    pub alpha: f32,
    pub blend_mode: SpriteBlendMode,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainLayerComposite {
    pub alpha: f32,
    pub blend_mode: SpriteBlendMode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainDrawPlan {
    /// Stable Pixi display order inside the terrain layer.
    pub terrain: Vec<TerrainDrawOp>,
    /// Landscape wall foreground. Dynamic wall graffiti is compiled by the
    /// decoration adapter because it owns actions and per-item transforms.
    pub wall_graffiti: Vec<TerrainDrawOp>,
    /// Drawn into an intermediate layer before `lighting_composite` is applied.
    pub lighting: Vec<TerrainDrawOp>,
    pub lighting_composite: Option<TerrainLayerComposite>,
    pub effects: Vec<TerrainDrawOp>,
}

impl TerrainDrawPlan {
    pub fn compile(
        style: &TerrainPaintStyle,
        frame: &TerrainFramePaint,
        geometry: &TerrainGeometry,
        atlas: &TextureAtlas,
    ) -> Result<Self> {
        validate_style_geometry(style, geometry)?;
        let floor_board = TerrainPlacement {
            origin: [-style.half_cell_size, -style.half_cell_size],
            size: [style.view_box, style.view_box],
        };
        let terrain_board = TerrainPlacement {
            origin: [-style.cell_size / 2.0, -style.cell_size / 2.0],
            size: [style.view_box, style.view_box],
        };
        let mut terrain = vec![TerrainDrawOp {
            phase: TerrainDrawPhase::Terrain,
            z_index: 0,
            placement: floor_board,
            source: TerrainDrawSource::Solid {
                color: style.floor_background,
            },
            mask: None,
            alpha: 1.0,
            blend_mode: SpriteBlendMode::Normal,
        }];
        if style.floor_foreground.is_none() {
            terrain.push(texture_op(
                TextureOpParameters {
                    phase: TerrainDrawPhase::Terrain,
                    z_index: 0,
                    placement: floor_board,
                    paint: TerrainTexturePaint {
                        atlas_name: "ground-mask".to_owned(),
                        tint: 0xff_ff_ff,
                        alpha: style.default_ground_mask_alpha,
                        tile_scale: Some(7.0),
                    },
                    tile_position: [0.0, 0.0],
                    mask: None,
                    blend_mode: SpriteBlendMode::Multiply,
                },
                atlas,
            )?);
        }
        terrain.push(texture_op(
            TextureOpParameters {
                phase: TerrainDrawPhase::Terrain,
                z_index: 0,
                placement: if style.floor_foreground.is_some() {
                    terrain_board
                } else {
                    floor_board
                },
                paint: style
                    .floor_foreground
                    .clone()
                    .unwrap_or(TerrainTexturePaint {
                        atlas_name: "ground".to_owned(),
                        tint: 0xff_ff_ff,
                        alpha: style.default_ground_alpha,
                        tile_scale: Some(3.0),
                    }),
                tile_position: [0.0, 0.0],
                mask: None,
                blend_mode: SpriteBlendMode::Normal,
            },
            atlas,
        )?);
        for exit in exits(style, atlas)? {
            terrain.push(exit);
        }
        if geometry.swamp_path.is_some() {
            terrain.push(TerrainDrawOp {
                phase: TerrainDrawPhase::Terrain,
                z_index: 1,
                placement: terrain_board,
                source: TerrainDrawSource::StyledCoverage {
                    fill: TerrainCoverage::SwampFill,
                    stroke: TerrainCoverage::SwampStroke,
                    fill_color: frame.swamp_fill,
                    stroke_color: frame.swamp_stroke,
                    tint: style.swamp_tint,
                    additive_texture: None,
                },
                mask: None,
                alpha: frame.swamp_alpha,
                blend_mode: SpriteBlendMode::Normal,
            });
            for noise in &frame.swamp_noise {
                terrain.push(TerrainDrawOp {
                    phase: TerrainDrawPhase::Terrain,
                    z_index: 1,
                    placement: terrain_board,
                    source: TerrainDrawSource::Texture(texture_sample(
                        atlas,
                        noise.atlas_name,
                        noise.tint,
                        Some([noise.tile_scale; 2]),
                        noise.tile_position,
                    )?),
                    mask: Some((TerrainCoverage::SwampFill, noise.mask_alpha)),
                    alpha: noise.alpha,
                    blend_mode: SpriteBlendMode::Add,
                });
            }
        }
        if geometry.wall_path.is_some() {
            terrain.push(TerrainDrawOp {
                phase: TerrainDrawPhase::Terrain,
                z_index: 2,
                placement: terrain_board,
                source: TerrainDrawSource::StyledCoverage {
                    fill: TerrainCoverage::WallFill,
                    stroke: TerrainCoverage::WallStroke,
                    fill_color: style.wall_fill,
                    stroke_color: style.wall_stroke,
                    tint: 0xff_ff_ff,
                    additive_texture: if let Some(alpha) = style.wall_noise_alpha {
                        Some((
                            texture_sample(
                                atlas,
                                "noise1",
                                0xff_ff_ff,
                                Some([style.wall_noise_tile_scale; 2]),
                                [0.0, 0.0],
                            )?,
                            TerrainCoverage::WallFill,
                            alpha,
                        ))
                    } else {
                        None
                    },
                },
                mask: None,
                alpha: 1.0,
                blend_mode: SpriteBlendMode::Normal,
            });
        }

        let wall_graffiti = match (&style.wall_foreground, &geometry.wall_path) {
            (Some(foreground), Some(_)) => vec![texture_op(
                TextureOpParameters {
                    phase: TerrainDrawPhase::WallGraffiti,
                    z_index: 1,
                    placement: terrain_board,
                    paint: foreground.clone(),
                    tile_position: [0.0, 0.0],
                    mask: Some((TerrainCoverage::WallFill, 1.0)),
                    blend_mode: SpriteBlendMode::Normal,
                },
                atlas,
            )?],
            _ => Vec::new(),
        };

        let (lighting, lighting_composite) = if style.lighting == TerrainLightingMode::Disabled {
            (Vec::new(), None)
        } else {
            let mut lighting = vec![TerrainDrawOp {
                phase: TerrainDrawPhase::Lighting,
                z_index: 0,
                placement: floor_board,
                source: TerrainDrawSource::Solid { color: 0x80_80_80 },
                mask: None,
                alpha: if style.lighting == TerrainLightingMode::Low {
                    0.5
                } else {
                    1.0
                },
                blend_mode: SpriteBlendMode::Normal,
            }];
            if geometry.wall_path.is_some() {
                lighting.push(TerrainDrawOp {
                    phase: TerrainDrawPhase::Lighting,
                    z_index: 1,
                    placement: terrain_board,
                    source: TerrainDrawSource::Coverage {
                        coverage: TerrainCoverage::WallFill,
                        color: 0,
                        blur_pixels: style.wall_shadow_blur_pixels,
                    },
                    mask: None,
                    alpha: 1.0,
                    blend_mode: SpriteBlendMode::Multiply,
                });
                lighting.push(TerrainDrawOp {
                    phase: TerrainDrawPhase::Lighting,
                    z_index: 2,
                    placement: terrain_board,
                    source: TerrainDrawSource::StyledCoverage {
                        fill: TerrainCoverage::WallFill,
                        stroke: TerrainCoverage::WallStroke,
                        fill_color: style.wall_lighting_fill,
                        stroke_color: style.wall_lighting_stroke,
                        tint: 0xff_ff_ff,
                        additive_texture: None,
                    },
                    mask: None,
                    alpha: 1.0,
                    blend_mode: SpriteBlendMode::Screen,
                });
            }
            (
                lighting,
                Some(TerrainLayerComposite {
                    alpha: 1.0,
                    blend_mode: SpriteBlendMode::Multiply,
                }),
            )
        };

        let mut effects = Vec::new();
        for (user, paint) in style.ramparts(geometry) {
            effects.push(TerrainDrawOp {
                phase: TerrainDrawPhase::Effects,
                z_index: 0,
                placement: terrain_board,
                source: TerrainDrawSource::StyledCoverage {
                    fill: TerrainCoverage::PrivateRampartFill(user.clone()),
                    stroke: TerrainCoverage::PrivateRampartStroke(user),
                    fill_color: paint.fill,
                    stroke_color: paint.user,
                    tint: 0xff_ff_ff,
                    additive_texture: None,
                },
                mask: None,
                alpha: f32::from(paint.alpha_numerator) / f32::from(paint.alpha_denominator),
                blend_mode: SpriteBlendMode::Add,
            });
        }
        Ok(Self {
            terrain,
            wall_graffiti,
            lighting,
            lighting_composite,
            effects,
        })
    }
}

struct TextureOpParameters {
    phase: TerrainDrawPhase,
    z_index: i32,
    placement: TerrainPlacement,
    paint: TerrainTexturePaint,
    tile_position: [f32; 2],
    mask: Option<(TerrainCoverage, f32)>,
    blend_mode: SpriteBlendMode,
}

fn texture_op(parameters: TextureOpParameters, atlas: &TextureAtlas) -> Result<TerrainDrawOp> {
    let TextureOpParameters {
        phase,
        z_index,
        placement,
        paint,
        tile_position,
        mask,
        blend_mode,
    } = parameters;
    Ok(TerrainDrawOp {
        phase,
        z_index,
        placement,
        source: TerrainDrawSource::Texture(texture_sample(
            atlas,
            &paint.atlas_name,
            paint.tint,
            paint.tile_scale.map(|scale| [scale; 2]),
            tile_position,
        )?),
        mask,
        alpha: paint.alpha,
        blend_mode,
    })
}

fn exits(style: &TerrainPaintStyle, atlas: &TextureAtlas) -> Result<Vec<TerrainDrawOp>> {
    let cell = style.cell_size;
    let full = style.view_box;
    let first = -cell / 2.0;
    let last = first + full - cell;
    let tint = match style.lighting {
        TerrainLightingMode::Normal => 0xff_ff_ff,
        TerrainLightingMode::Low => 0xc0_c0_c0,
        TerrainLightingMode::Disabled => 0xa0_a0_a0,
    };
    [
        ("exit-left", [first, first], [cell, full]),
        ("exit-bottom", [first, last], [full, cell]),
        ("exit-top", [first, first], [full, cell]),
        ("exit-right", [last, first], [cell, full]),
    ]
    .into_iter()
    .map(|(atlas_name, origin, size)| {
        let entry = atlas_entry(atlas, atlas_name)?;
        Ok(TerrainDrawOp {
            phase: TerrainDrawPhase::Terrain,
            z_index: 1,
            placement: TerrainPlacement { origin, size },
            source: TerrainDrawSource::Texture(TerrainTextureSample {
                atlas_name: atlas_name.to_owned(),
                tint,
                mipmap: true,
                tile_scale: Some([cell / entry.logical_width, cell / entry.logical_height]),
                tile_position: [0.0, 0.0],
            }),
            mask: None,
            alpha: 0.5,
            blend_mode: SpriteBlendMode::Add,
        })
    })
    .collect()
}

fn texture_sample(
    atlas: &TextureAtlas,
    atlas_name: &str,
    tint: u32,
    tile_scale: Option<[f32; 2]>,
    tile_position: [f32; 2],
) -> Result<TerrainTextureSample> {
    atlas_entry(atlas, atlas_name)?;
    Ok(TerrainTextureSample {
        atlas_name: atlas_name.to_owned(),
        tint,
        mipmap: tile_scale.is_none() || !atlas_name.starts_with("$decoration["),
        tile_scale,
        tile_position,
    })
}

fn atlas_entry<'a>(atlas: &'a TextureAtlas, name: &str) -> Result<&'a crate::AtlasEntry> {
    atlas.entries.get(name).ok_or_else(|| {
        Error::Invalid(format!(
            "terrain draw plan references missing atlas texture {name}"
        ))
    })
}

fn validate_style_geometry(style: &TerrainPaintStyle, geometry: &TerrainGeometry) -> Result<()> {
    if style.view_box != geometry.view_box as f32 {
        return Err(Error::Invalid(
            "terrain paint and geometry VIEW_BOX values disagree".to_owned(),
        ));
    }
    if style.cell_size <= 0.0
        || !style.cell_size.is_finite()
        || !style.half_cell_size.is_finite()
        || style.view_box <= 0.0
        || !style.view_box.is_finite()
    {
        return Err(Error::Invalid(
            "terrain draw dimensions must be finite and positive".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::artifact::{Nullable, RendererContract, RendererInventory};
    use crate::{
        AtlasEntry, SpriteBlendMode, TerrainCoverage, TerrainDrawPlan, TerrainDrawSource,
        TerrainGeometry, TerrainPaintStyle, TerrainSwampTexture, TextureAtlas,
    };

    fn contract(lighting: &str) -> RendererContract {
        RendererContract {
            schema: "screeps-arena-renderer-contract".to_owned(),
            version: 5,
            renderer_version: Nullable(Some("test".to_owned())),
            metadata: json!({}),
            resources: json!({}),
            decorations: Vec::new(),
            terrain: Vec::new(),
            world_options: json!({
                "CELL_SIZE": 100,
                "ROOM_SIZE": 100,
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

    fn geometry() -> TerrainGeometry {
        TerrainGeometry {
            room_size: 100,
            view_box: 10_000,
            wall_path: Some("wall".to_owned()),
            swamp_path: Some("swamp".to_owned()),
            private_rampart_paths: BTreeMap::from([("owner".to_owned(), "rampart".to_owned())]),
            private_rampart_colors: BTreeMap::from([("owner".to_owned(), 0x11_22_33)]),
            swamp_texture: TerrainSwampTexture::Animated,
            fingerprint: "cd".repeat(32),
        }
    }

    fn atlas() -> TextureAtlas {
        let entries = [
            "exit-bottom",
            "exit-left",
            "exit-right",
            "exit-top",
            "ground",
            "ground-mask",
            "noise1",
            "noise2",
        ]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                AtlasEntry {
                    page: 0,
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 25,
                    logical_width: 20.0,
                    logical_height: 25.0,
                    u_min: 0.0,
                    v_min: 0.0,
                    u_max: 1.0,
                    v_max: 1.0,
                },
            )
        })
        .collect();
        TextureAtlas {
            entries,
            pages: Vec::new(),
            padding: 1,
        }
    }

    #[test]
    fn compiles_exact_default_layer_order_blends_masks_and_placements() {
        let style = TerrainPaintStyle::compile(&contract("normal"), 2_048).unwrap();
        let frame = style.frame(&geometry(), 2.0).unwrap();
        let plan = TerrainDrawPlan::compile(&style, &frame, &geometry(), &atlas()).unwrap();

        assert_eq!(plan.terrain.len(), 11);
        assert!(matches!(
            plan.terrain[1].source,
            TerrainDrawSource::Texture(crate::TerrainTextureSample {
                ref atlas_name,
                mipmap: true,
                tile_scale: Some([7.0, 7.0]),
                ..
            }) if atlas_name == "ground-mask"
        ));
        assert_eq!(plan.terrain[1].blend_mode, SpriteBlendMode::Multiply);
        assert_eq!(plan.terrain[3].placement.origin, [-50.0, -50.0]);
        assert_eq!(plan.terrain[4].placement.origin, [-50.0, 9_850.0]);
        assert_eq!(plan.terrain[5].placement.origin, [-50.0, -50.0]);
        assert_eq!(plan.terrain[6].placement.origin, [9_850.0, -50.0]);
        assert!(matches!(
            plan.terrain[3].source,
            TerrainDrawSource::Texture(crate::TerrainTextureSample {
                tile_scale: Some([5.0, 4.0]),
                ..
            })
        ));
        assert_eq!(
            plan.terrain[8].mask,
            Some((TerrainCoverage::SwampFill, 0.25))
        );
        assert_eq!(plan.terrain[8].blend_mode, SpriteBlendMode::Add);
        assert!(plan.wall_graffiti.is_empty());
        assert_eq!(plan.lighting.len(), 3);
        assert_eq!(
            plan.lighting_composite.unwrap().blend_mode,
            SpriteBlendMode::Multiply
        );
        assert_eq!(plan.effects.len(), 1);
        assert_eq!(plan.effects[0].blend_mode, SpriteBlendMode::Add);
    }

    #[test]
    fn disabled_lighting_has_no_intermediate_layer() {
        let style = TerrainPaintStyle::compile(&contract("disabled"), 1_000).unwrap();
        let frame = style.frame(&geometry(), 0.0).unwrap();
        let plan = TerrainDrawPlan::compile(&style, &frame, &geometry(), &atlas()).unwrap();
        assert!(plan.lighting.is_empty());
        assert!(plan.lighting_composite.is_none());
        assert!(plan.terrain.iter().any(|operation| {
            matches!(
                operation.source,
                TerrainDrawSource::Texture(crate::TerrainTextureSample {
                    tint: 0xa0_a0_a0,
                    ..
                })
            )
        }));
    }

    #[test]
    fn only_bundled_terrain_textures_inherit_pixis_global_mipmaps() {
        let mut atlas = atlas();
        let decoration = "$decoration[0].decoration.floorForegroundUrl";
        atlas
            .entries
            .insert(decoration.to_owned(), atlas.entries["ground"]);
        assert!(
            super::texture_sample(&atlas, "ground", 0, Some([1.0; 2]), [0.0; 2])
                .unwrap()
                .mipmap
        );
        assert!(
            !super::texture_sample(&atlas, decoration, 0, Some([1.0; 2]), [0.0; 2])
                .unwrap()
                .mipmap
        );
        assert!(
            super::texture_sample(&atlas, decoration, 0, None, [0.0; 2])
                .unwrap()
                .mipmap
        );
    }

    #[test]
    fn low_lighting_dims_only_the_layer_owned_gray_background() {
        let style = TerrainPaintStyle::compile(&contract("low"), 1_000).unwrap();
        let frame = style.frame(&geometry(), 0.0).unwrap();
        let plan = TerrainDrawPlan::compile(&style, &frame, &geometry(), &atlas()).unwrap();
        assert_eq!(plan.lighting[0].alpha, 0.5);
        assert_eq!(plan.lighting[1].alpha, 1.0);
        assert_eq!(plan.lighting[2].alpha, 1.0);
        assert_eq!(plan.lighting_composite.unwrap().alpha, 1.0);
    }
}
