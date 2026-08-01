use std::collections::{BTreeMap, HashMap};

use crate::{
    ActionManagerRuntime, ActionTarget, Affine2, AtlasEntry, BoardTransform, Error, ProcessorKind,
    ResolvedActivation, ResolvedScene, ResolvedValue, Result, SpriteBlendMode, SpriteInstance,
    TextureAtlas,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeTransform {
    pub position: [f64; 2],
    pub scale: [f64; 2],
    pub rotation: f64,
    /// Pixi pivot in unscaled local units.
    pub pivot: [f64; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub enum SceneNodeKind {
    Container,
    Vector {
        program: crate::VectorProgram,
        mesh: crate::VectorMesh,
        tint: u32,
        blend_mode: SpriteBlendMode,
        blur: Option<f64>,
    },
    Sprite {
        texture: String,
        atlas: AtlasEntry,
        natural_size: [f64; 2],
        anchor: [f64; 2],
        tint: u32,
        blend_mode: SpriteBlendMode,
        blur: Option<f64>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneNodeTemplate {
    pub entity_id: String,
    pub definition_id: String,
    pub scope_id: String,
    pub node_id: String,
    pub is_root: bool,
    pub parent_id: Option<String>,
    pub layer: Option<String>,
    pub z_index: f64,
    pub activation_order: u32,
    pub start_tick: u32,
    pub end_tick: u32,
    pub transform: NodeTransform,
    pub alpha: f64,
    pub visible: bool,
    pub kind: SceneNodeKind,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SceneNodeKey {
    pub entity_id: String,
    pub node_id: String,
    pub is_root: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneNodeTemplates {
    pub nodes: Vec<SceneNodeTemplate>,
    nodes_by_activation: BTreeMap<u32, usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedSprite {
    pub entity_id: String,
    pub node_id: String,
    pub layer: Option<String>,
    pub z_index: f64,
    pub activation_order: u32,
    pub transform: Affine2,
    pub natural_size: [f64; 2],
    pub anchor: [f64; 2],
    pub atlas: AtlasEntry,
    pub alpha: f64,
    pub tint: u32,
    pub visible: bool,
    pub blend_mode: SpriteBlendMode,
    pub blur: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedVector<'a> {
    pub entity_id: &'a str,
    pub node_id: &'a str,
    pub layer: Option<&'a str>,
    pub layer_order: u32,
    pub z_index: f64,
    pub activation_order: u32,
    pub transform: Affine2,
    pub mesh: &'a crate::VectorMesh,
    pub alpha: f64,
    pub tint: u32,
    pub visible: bool,
    pub blend_mode: SpriteBlendMode,
    pub blur: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreparedSpriteInstance {
    pub activation_order: u32,
    pub layer_order: u32,
    pub blend_mode: SpriteBlendMode,
    pub instance: SpriteInstance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpriteDisplayEntry {
    pub activation_order: u32,
    pub layer_order: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneDrawableKind {
    Sprite,
    Vector,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneDisplayEntry {
    pub activation_order: u32,
    pub layer_order: u32,
    pub kind: SceneDrawableKind,
}

#[derive(Debug, Default)]
pub struct SceneFrameScratch {
    transforms: HashMap<u32, ActiveTransform>,
    instances: Vec<PreparedSpriteInstance>,
}

impl SceneFrameScratch {
    pub fn instances(&self) -> &[PreparedSpriteInstance] {
        &self.instances
    }
}

impl SceneNodeTemplates {
    pub fn vector_meshes(&self) -> impl Iterator<Item = &crate::VectorMesh> {
        self.nodes.iter().filter_map(|node| match &node.kind {
            SceneNodeKind::Vector { mesh, .. } => Some(mesh),
            SceneNodeKind::Container | SceneNodeKind::Sprite { .. } => None,
        })
    }

    /// Lower retained containers, textured sprites, and rasterized circle
    /// graphics. Other typed processors remain scheduled for their adapters.
    pub fn compile(scene: &ResolvedScene, atlas: &TextureAtlas) -> Result<Self> {
        let mut nodes = Vec::new();
        for activation in &scene.activations {
            if let ResolvedActivation::Object {
                entity_id,
                object_type,
                layer,
                z_index,
                activation_order,
                start_tick,
                end_tick,
                data,
            } = activation
            {
                let payload = data.as_object().ok_or_else(|| {
                    Error::Invalid(format!(
                        "renderer object {object_type} data must resolve to an object"
                    ))
                })?;
                let common = CommonNode::parse(payload, "__root__")?;
                nodes.push(SceneNodeTemplate {
                    entity_id: entity_id.clone(),
                    definition_id: format!("auto:$.objects.{object_type}"),
                    scope_id: "__root__".to_owned(),
                    node_id: "__root__".to_owned(),
                    is_root: true,
                    parent_id: None,
                    layer: layer.clone(),
                    z_index: *z_index,
                    activation_order: *activation_order,
                    start_tick: *start_tick,
                    end_tick: *end_tick,
                    transform: NodeTransform {
                        position: common.position,
                        scale: common.scale,
                        rotation: common.rotation,
                        pivot: vector(payload.get("pivot"), [0.0, 0.0], "object pivot")?,
                    },
                    alpha: common.alpha,
                    visible: common.visible,
                    kind: SceneNodeKind::Container,
                });
                continue;
            }
            let ResolvedActivation::Processor {
                entity_id,
                definition_id,
                scope_id,
                kind,
                layer,
                z_index,
                activation_order,
                start_tick,
                end_tick,
                payload,
                object_texture,
                temporary_node,
                ..
            } = activation
            else {
                continue;
            };
            if !matches!(
                kind,
                ProcessorKind::Circle
                    | ProcessorKind::Container
                    | ProcessorKind::Draw
                    | ProcessorKind::ResourceCircle
                    | ProcessorKind::SiteProgress
                    | ProcessorKind::Sprite
                    | ProcessorKind::UserBadge
            ) {
                continue;
            }
            if matches!(
                kind,
                ProcessorKind::ResourceCircle | ProcessorKind::SiteProgress
            ) && matches!(payload, ResolvedValue::Undefined)
            {
                continue;
            }
            let Some(payload) = payload.as_object() else {
                return Err(Error::Invalid(format!(
                    "{} payload must resolve to an object",
                    kind.as_str()
                )));
            };
            if !payload
                .get("shouldCreate")
                .is_none_or(crate::value_plan::resolved_js_truthy)
            {
                continue;
            }
            let empty_payload = BTreeMap::new();
            let common_payload = if *kind == ProcessorKind::SiteProgress {
                &empty_payload
            } else {
                payload
            };
            let common = CommonNode::parse(common_payload, scope_id)?;
            let node_id = if *temporary_node {
                format!("$temporary.processor.{activation_order}")
            } else {
                common.node_id.clone()
            };
            let node_kind = match kind {
                ProcessorKind::Container => SceneNodeKind::Container,
                ProcessorKind::Draw => {
                    let program = crate::VectorProgram::from_draw_payload(&ResolvedValue::Object(
                        payload.clone(),
                    ))?;
                    let mesh = crate::tessellate_vector_program(&program)?;
                    SceneNodeKind::Vector {
                        program,
                        mesh,
                        tint: color(payload.get("tint"), 0x00ff_ffff, "draw tint")?,
                        blend_mode: blend_mode(payload.get("blendMode"))?,
                        blur: optional_optional_number(payload.get("blur"), "draw blur")?,
                    }
                }
                ProcessorKind::SiteProgress => {
                    let program =
                        crate::site_progress_program(&ResolvedValue::Object(payload.clone()))?;
                    let mesh = crate::tessellate_vector_program(&program)?;
                    SceneNodeKind::Vector {
                        program,
                        mesh,
                        tint: 0x00ff_ffff,
                        blend_mode: SpriteBlendMode::Normal,
                        blur: None,
                    }
                }
                ProcessorKind::Sprite | ProcessorKind::UserBadge
                    if payload.contains_key("texture") =>
                {
                    let texture = effective_texture(payload, object_texture.as_ref())?;
                    let Some(texture) = texture else {
                        continue;
                    };
                    let entry = atlas.entries.get(texture).copied().ok_or_else(|| {
                        Error::Invalid(format!(
                            "sprite processor references missing atlas texture {texture}"
                        ))
                    })?;
                    sprite_kind(payload, texture, entry, common.scale)?
                }
                ProcessorKind::Circle
                | ProcessorKind::ResourceCircle
                | ProcessorKind::UserBadge => {
                    let (texture, natural_size) =
                        crate::procedural_graphics::circle_asset_geometry(payload)?;
                    let entry = atlas.entries.get(&texture).copied().ok_or_else(|| {
                        Error::Invalid(format!(
                            "circle processor references missing procedural atlas texture {texture}"
                        ))
                    })?;
                    SceneNodeKind::Sprite {
                        texture,
                        atlas: entry,
                        natural_size,
                        anchor: [0.5, 0.5],
                        tint: color(payload.get("tint"), 0x00ff_ffff, "circle tint")?,
                        blend_mode: blend_mode(payload.get("blendMode"))?,
                        blur: optional_optional_number(payload.get("blur"), "circle blur")?,
                    }
                }
                _ => unreachable!("dedicated processors were filtered above"),
            };
            let scale = match &node_kind {
                SceneNodeKind::Sprite { natural_size, .. } => {
                    dimension_scale(payload, *natural_size, common.scale)?
                }
                SceneNodeKind::Container | SceneNodeKind::Vector { .. } => common.scale,
            };
            let pivot = match &node_kind {
                SceneNodeKind::Sprite { .. } => parsed_pivot(payload, scale)?,
                SceneNodeKind::Container | SceneNodeKind::Vector { .. } => {
                    parsed_pivot(common_payload, scale)?
                }
            };
            let transform = NodeTransform {
                position: common.position,
                scale,
                rotation: if *kind == ProcessorKind::SiteProgress {
                    -std::f64::consts::FRAC_PI_2
                } else {
                    common.rotation
                },
                pivot,
            };
            nodes.push(SceneNodeTemplate {
                entity_id: entity_id.clone(),
                definition_id: definition_id.clone(),
                scope_id: scope_id.clone(),
                node_id,
                is_root: false,
                parent_id: common.parent_id,
                layer: layer.clone(),
                z_index: *z_index,
                activation_order: *activation_order,
                start_tick: *start_tick,
                end_tick: *end_tick,
                transform,
                alpha: common.alpha,
                visible: common.visible,
                kind: node_kind,
            });
        }
        let nodes_by_activation = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.activation_order, index))
            .collect::<BTreeMap<_, _>>();
        if nodes_by_activation.len() != nodes.len() {
            return Err(Error::Invalid(
                "scene nodes repeat a renderer activation order".to_owned(),
            ));
        }
        Ok(Self {
            nodes,
            nodes_by_activation,
        })
    }

    pub fn prepare_at(&self, tick: u32, board: BoardTransform) -> Result<Vec<PreparedSprite>> {
        self.prepare_nodes(
            board,
            self.nodes
                .iter()
                .filter(|node| node.start_tick <= tick && tick < node.end_tick),
            |_| None,
        )
    }

    /// Prepare one timestamp after applying precompiled action state. The
    /// action-track compiler owns temporal stepping; this stage remains
    /// stateless and only turns the selected node values into composed affine
    /// sprite instances.
    pub fn prepare_with_action_targets_at(
        &self,
        tick: u32,
        board: BoardTransform,
        action_targets: &BTreeMap<u32, ActionTarget>,
    ) -> Result<Vec<PreparedSprite>> {
        self.prepare_nodes(
            board,
            self.nodes
                .iter()
                .filter(|node| node.start_tick <= tick && tick < node.end_tick),
            |node| action_targets.get(&node.activation_order),
        )
    }

    pub fn prepare_with_action_manager_at(
        &self,
        _tick: u32,
        board: BoardTransform,
        action_manager: &ActionManagerRuntime,
    ) -> Result<Vec<PreparedSprite>> {
        let mut transforms = HashMap::new();
        let mut sprites = Vec::new();
        self.visit_action_manager_nodes(
            board,
            action_manager,
            &mut transforms,
            |node, prepared, target| {
                if let Some(sprite) = prepared_sprite(node, prepared, Some(target)) {
                    sprites.push(sprite);
                }
                Ok(())
            },
        )?;
        Ok(sprites)
    }

    pub fn prepare_vectors_with_action_manager_at<'a>(
        &'a self,
        _tick: u32,
        board: BoardTransform,
        action_manager: &ActionManagerRuntime,
    ) -> Result<Vec<PreparedVector<'a>>> {
        let mut transforms = HashMap::new();
        let mut vectors = Vec::new();
        self.compose_action_manager_transforms(board, action_manager, &mut transforms)?;
        for activation in action_manager.visible_activation_ids() {
            let Some(node) = self
                .nodes_by_activation
                .get(&activation)
                .map(|index| &self.nodes[*index])
            else {
                continue;
            };
            let Some(prepared) = transforms.get(&activation).copied() else {
                continue;
            };
            let target = action_manager.target(activation).ok_or_else(|| {
                Error::Invalid(format!(
                    "visible vector activation {activation} lacks an action target"
                ))
            })?;
            if let Some(vector) = prepared_vector(node, prepared, Some(target)) {
                vectors.push(vector);
            }
        }
        Ok(vectors)
    }

    /// Compose only currently visible nodes directly into a reusable,
    /// allocation-stable GPU staging buffer. This is the hot path used while
    /// compiling temporal frame batches.
    pub fn prepare_gpu_instances_with_action_manager<'s>(
        &self,
        board: BoardTransform,
        action_manager: &ActionManagerRuntime,
        sprite_display_order: &[SpriteDisplayEntry],
        scratch: &'s mut SceneFrameScratch,
    ) -> Result<&'s [PreparedSpriteInstance]> {
        scratch.transforms.clear();
        scratch.instances.clear();
        scratch
            .instances
            .reserve(action_manager.visible_activation_ids().len());
        self.compose_action_manager_transforms(board, action_manager, &mut scratch.transforms)?;
        for entry in sprite_display_order {
            let activation = entry.activation_order;
            let Some(node) = self
                .nodes_by_activation
                .get(&activation)
                .map(|index| &self.nodes[*index])
            else {
                return Err(Error::Invalid(format!(
                    "display order references unknown scene activation {activation}"
                )));
            };
            let SceneNodeKind::Sprite {
                atlas,
                natural_size,
                anchor,
                tint: _,
                blend_mode,
                blur,
                ..
            } = &node.kind
            else {
                return Err(Error::Invalid(format!(
                    "display order references non-sprite scene activation {activation}"
                )));
            };
            let Some(prepared) = scratch.transforms.get(&activation).copied() else {
                continue;
            };
            let target = action_manager.target(activation).ok_or_else(|| {
                Error::Invalid(format!(
                    "displayed sprite activation {activation} lacks an action target"
                ))
            })?;
            let tint = target.tint;
            let blur = effective_blur(Some(target), *blur);
            scratch.instances.push(PreparedSpriteInstance {
                activation_order: node.activation_order,
                layer_order: entry.layer_order,
                blend_mode: *blend_mode,
                instance: sprite_instance(SpriteInstanceParameters {
                    transform: prepared.transform,
                    natural_size: *natural_size,
                    anchor: *anchor,
                    atlas: *atlas,
                    alpha: prepared.alpha,
                    tint,
                    visible: prepared.visible,
                    blur,
                })?,
            });
        }
        Ok(&scratch.instances)
    }

    fn prepare_nodes<'a, 'n>(
        &'n self,
        board: BoardTransform,
        nodes: impl Iterator<Item = &'n SceneNodeTemplate>,
        mut action_target_for: impl FnMut(&SceneNodeTemplate) -> Option<&'a ActionTarget>,
    ) -> Result<Vec<PreparedSprite>> {
        let mut active = BTreeMap::<SceneNodeKey, ActiveTransform>::new();
        let mut sprites = Vec::new();
        for node in nodes {
            let action_target = action_target_for(node);
            let parent = if node.is_root {
                ActiveTransform {
                    transform: board.affine(),
                    alpha: 1.0,
                    visible: true,
                }
            } else {
                let parent_id = node.parent_id.as_deref().unwrap_or("__root__");
                let Some(parent) = active.get(&SceneNodeKey {
                    entity_id: node.entity_id.clone(),
                    node_id: parent_id.to_owned(),
                    is_root: node.parent_id.is_none(),
                }) else {
                    // The retained object helper warns and returns no node when
                    // an explicit parent is unavailable.
                    continue;
                };
                *parent
            };
            let transform = action_target.map_or(node.transform, |target| NodeTransform {
                position: [target.x, target.y],
                scale: [target.scale_x, target.scale_y],
                rotation: target.rotation,
                pivot: node.transform.pivot,
            });
            let local = Affine2::from_components(
                transform.position,
                transform.scale,
                transform.rotation,
                transform.pivot,
            );
            let prepared = ActiveTransform {
                transform: parent.transform.then(local),
                alpha: parent.alpha * action_target.map_or(node.alpha, |target| target.alpha),
                visible: parent.visible && node.visible,
            };
            active.insert(node.key(), prepared);
            if let Some(sprite) = prepared_sprite(node, prepared, action_target) {
                sprites.push(sprite);
            }
        }
        Ok(sprites)
    }

    fn visit_action_manager_nodes(
        &self,
        board: BoardTransform,
        action_manager: &ActionManagerRuntime,
        transforms: &mut HashMap<u32, ActiveTransform>,
        mut visit: impl FnMut(&SceneNodeTemplate, ActiveTransform, &ActionTarget) -> Result<()>,
    ) -> Result<()> {
        self.compose_action_manager_transforms(board, action_manager, transforms)?;
        for activation in action_manager.visible_activation_ids() {
            let Some(node) = self
                .nodes_by_activation
                .get(&activation)
                .map(|index| &self.nodes[*index])
            else {
                continue;
            };
            let target = action_manager.target(activation).ok_or_else(|| {
                Error::Invalid(format!(
                    "visible scene activation {activation} lacks an action target"
                ))
            })?;
            let Some(prepared) = transforms.get(&activation).copied() else {
                continue;
            };
            visit(node, prepared, target)?;
        }
        Ok(())
    }

    fn compose_action_manager_transforms(
        &self,
        board: BoardTransform,
        action_manager: &ActionManagerRuntime,
        transforms: &mut HashMap<u32, ActiveTransform>,
    ) -> Result<()> {
        transforms.clear();
        for activation in action_manager.visible_activation_ids() {
            let Some(node) = self
                .nodes_by_activation
                .get(&activation)
                .map(|index| &self.nodes[*index])
            else {
                continue;
            };
            let target = action_manager.target(activation).ok_or_else(|| {
                Error::Invalid(format!(
                    "visible scene activation {activation} lacks an action target"
                ))
            })?;
            let parent = match action_manager.parent_activation(activation) {
                Some(None) if node.node_id == "__root__" && node.parent_id.is_none() => {
                    ActiveTransform {
                        transform: board.affine(),
                        alpha: 1.0,
                        visible: true,
                    }
                }
                Some(Some(parent)) => {
                    let Some(parent) = transforms.get(&parent) else {
                        // A child can remain scope-addressable after its parent
                        // display object is destroyed, but is not rendered.
                        continue;
                    };
                    *parent
                }
                Some(None) => {
                    return Err(Error::Invalid(format!(
                        "non-root scene activation {activation} lacks a parent identity"
                    )));
                }
                None => {
                    return Err(Error::Invalid(format!(
                        "scene activation {activation} lacks parent bookkeeping"
                    )));
                }
            };
            let prepared = compose_node(node, parent, Some(target));
            transforms.insert(activation, prepared);
        }
        Ok(())
    }
}

fn compose_node(
    node: &SceneNodeTemplate,
    parent: ActiveTransform,
    action_target: Option<&ActionTarget>,
) -> ActiveTransform {
    let transform = action_target.map_or(node.transform, |target| NodeTransform {
        position: [target.x, target.y],
        scale: [target.scale_x, target.scale_y],
        rotation: target.rotation,
        pivot: node.transform.pivot,
    });
    let local = Affine2::from_components(
        transform.position,
        transform.scale,
        transform.rotation,
        transform.pivot,
    );
    ActiveTransform {
        transform: parent.transform.then(local),
        alpha: parent.alpha * action_target.map_or(node.alpha, |target| target.alpha),
        visible: parent.visible && node.visible,
    }
}

fn prepared_sprite(
    node: &SceneNodeTemplate,
    prepared: ActiveTransform,
    action_target: Option<&ActionTarget>,
) -> Option<PreparedSprite> {
    let SceneNodeKind::Sprite {
        atlas,
        natural_size,
        anchor,
        tint,
        blend_mode,
        blur,
        ..
    } = &node.kind
    else {
        return None;
    };
    Some(PreparedSprite {
        entity_id: node.entity_id.clone(),
        node_id: node.node_id.clone(),
        layer: node.layer.clone(),
        z_index: node.z_index,
        activation_order: node.activation_order,
        transform: prepared.transform,
        natural_size: *natural_size,
        anchor: *anchor,
        atlas: *atlas,
        alpha: prepared.alpha,
        tint: action_target.map_or(*tint, |target| target.tint),
        visible: prepared.visible,
        blend_mode: *blend_mode,
        blur: effective_blur(action_target, *blur),
    })
}

fn prepared_vector<'a>(
    node: &'a SceneNodeTemplate,
    prepared: ActiveTransform,
    action_target: Option<&ActionTarget>,
) -> Option<PreparedVector<'a>> {
    let SceneNodeKind::Vector {
        mesh,
        tint,
        blend_mode,
        blur,
        ..
    } = &node.kind
    else {
        return None;
    };
    Some(PreparedVector {
        entity_id: &node.entity_id,
        node_id: &node.node_id,
        layer: node.layer.as_deref(),
        layer_order: 0,
        z_index: node.z_index,
        activation_order: node.activation_order,
        transform: prepared.transform,
        mesh,
        alpha: prepared.alpha,
        tint: action_target.map_or(*tint, |target| target.tint),
        visible: prepared.visible,
        blend_mode: *blend_mode,
        blur: effective_blur(action_target, *blur),
    })
}

fn effective_blur(action_target: Option<&ActionTarget>, blur: Option<f64>) -> Option<f64> {
    action_target
        .and_then(|target| {
            target
                .filters
                .first()
                .and_then(|filter| filter.get("blur"))
                .copied()
        })
        .or(blur)
}

impl SceneNodeTemplate {
    pub fn key(&self) -> SceneNodeKey {
        SceneNodeKey {
            entity_id: self.entity_id.clone(),
            node_id: self.node_id.clone(),
            is_root: self.is_root,
        }
    }

    pub fn initial_action_target(&self) -> ActionTarget {
        let (tint, filters) = match &self.kind {
            SceneNodeKind::Sprite { tint, blur, .. } => {
                let filters = blur
                    .map(|blur| BTreeMap::from([("blur".to_owned(), blur)]))
                    .into_iter()
                    .collect();
                (*tint, filters)
            }
            SceneNodeKind::Vector { tint, blur, .. } => {
                let filters = blur
                    .map(|blur| BTreeMap::from([("blur".to_owned(), blur)]))
                    .into_iter()
                    .collect();
                (*tint, filters)
            }
            SceneNodeKind::Container => (0x00ff_ffff, Vec::new()),
        };
        ActionTarget {
            x: self.transform.position[0],
            y: self.transform.position[1],
            scale_x: self.transform.scale[0],
            scale_y: self.transform.scale[1],
            rotation: self.transform.rotation,
            alpha: self.alpha,
            tint,
            filters,
        }
    }
}

impl PreparedSprite {
    pub fn gpu_instance(&self) -> Result<SpriteInstance> {
        sprite_instance(SpriteInstanceParameters {
            transform: self.transform,
            natural_size: self.natural_size,
            anchor: self.anchor,
            atlas: self.atlas,
            alpha: self.alpha,
            tint: self.tint,
            visible: self.visible,
            blur: self.blur,
        })
    }
}

struct SpriteInstanceParameters {
    transform: Affine2,
    natural_size: [f64; 2],
    anchor: [f64; 2],
    atlas: AtlasEntry,
    alpha: f64,
    tint: u32,
    visible: bool,
    blur: Option<f64>,
}

fn sprite_instance(parameters: SpriteInstanceParameters) -> Result<SpriteInstance> {
    let SpriteInstanceParameters {
        transform,
        natural_size,
        anchor,
        atlas,
        alpha,
        tint,
        visible,
        blur,
    } = parameters;
    if blur.is_some_and(|blur| !blur.is_finite()) {
        return Err(Error::Invalid(
            "prepared sprite blur must be finite".to_owned(),
        ));
    }
    let values = [
        transform.a,
        transform.b,
        transform.c,
        transform.d,
        transform.tx,
        transform.ty,
        natural_size[0],
        natural_size[1],
        anchor[0],
        anchor[1],
        alpha,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return Err(Error::Invalid(
            "prepared sprite contains a non-finite GPU value".to_owned(),
        ));
    }
    let red = f64::from((tint >> 16) & 0xff) / 255.0;
    let green = f64::from((tint >> 8) & 0xff) / 255.0;
    let blue = f64::from(tint & 0xff) / 255.0;
    Ok(SpriteInstance {
        transform_x: [
            transform.a as f32,
            transform.c as f32,
            transform.tx as f32,
            0.0,
        ],
        transform_y: [
            transform.b as f32,
            transform.d as f32,
            transform.ty as f32,
            0.0,
        ],
        size_anchor: [
            natural_size[0] as f32,
            natural_size[1] as f32,
            anchor[0] as f32,
            anchor[1] as f32,
        ],
        uv_rect: [atlas.u_min, atlas.v_min, atlas.u_max, atlas.v_max],
        tint_alpha: [red as f32, green as f32, blue as f32, alpha as f32],
        atlas_page: atlas.page,
        visible: u32::from(visible),
        blur: blur.unwrap_or(0.0) as f32,
        has_blur_filter: u32::from(blur.is_some()),
    })
}

#[derive(Clone, Copy, Debug)]
struct ActiveTransform {
    transform: Affine2,
    alpha: f64,
    visible: bool,
}

struct CommonNode {
    node_id: String,
    parent_id: Option<String>,
    position: [f64; 2],
    scale: [f64; 2],
    rotation: f64,
    alpha: f64,
    visible: bool,
}

impl CommonNode {
    fn parse(
        payload: &std::collections::BTreeMap<String, ResolvedValue>,
        scope_id: &str,
    ) -> Result<Self> {
        let node_id = match payload.get("id") {
            None | Some(ResolvedValue::Undefined) => scope_id.to_owned(),
            Some(value) => crate::value_plan::js_property_key(value)?,
        };
        let parent_id = payload
            .get("parentId")
            .filter(|value| crate::value_plan::resolved_js_truthy(value))
            .map(crate::value_plan::js_property_key)
            .transpose()?;
        Ok(Self {
            node_id,
            parent_id,
            position: [
                optional_number(payload.get("x"), 0.0, "sprite/container x")?,
                optional_number(payload.get("y"), 0.0, "sprite/container y")?,
            ],
            scale: vector(payload.get("scale"), [1.0, 1.0], "sprite/container scale")?,
            rotation: optional_number(payload.get("rotation"), 0.0, "sprite/container rotation")?,
            alpha: optional_number(payload.get("alpha"), 1.0, "sprite/container alpha")?,
            visible: payload
                .get("visible")
                .is_none_or(crate::value_plan::resolved_js_truthy),
        })
    }
}

fn sprite_kind(
    payload: &std::collections::BTreeMap<String, ResolvedValue>,
    texture: &str,
    entry: AtlasEntry,
    initial_scale: [f64; 2],
) -> Result<SceneNodeKind> {
    let natural_size = [
        f64::from(entry.logical_width),
        f64::from(entry.logical_height),
    ];
    let scale = dimension_scale(payload, natural_size, initial_scale)?;
    let anchor = parsed_anchor(payload)?;
    // Validate the pivot while both the dimension-derived scale and payload
    // are available. It is stored on the common transform below.
    let _ = parsed_pivot(payload, scale)?;
    Ok(SceneNodeKind::Sprite {
        texture: texture.to_owned(),
        atlas: entry,
        natural_size,
        anchor,
        tint: color(payload.get("tint"), 0x00ff_ffff, "sprite tint")?,
        blend_mode: blend_mode(payload.get("blendMode"))?,
        blur: optional_optional_number(payload.get("blur"), "sprite blur")?,
    })
}

fn effective_texture<'a>(
    payload: &'a std::collections::BTreeMap<String, ResolvedValue>,
    object_texture: Option<&'a ResolvedValue>,
) -> Result<Option<&'a str>> {
    match payload
        .get("texture")
        .filter(|value| !matches!(value, ResolvedValue::Undefined))
        .or(object_texture)
    {
        None | Some(ResolvedValue::Undefined) | Some(ResolvedValue::Null) => Ok(None),
        Some(ResolvedValue::String(texture)) if texture.is_empty() => Ok(None),
        Some(ResolvedValue::String(texture)) => Ok(Some(texture)),
        Some(_) => Err(Error::Invalid(
            "native sprite texture must resolve to a resource name".to_owned(),
        )),
    }
}

fn dimension_scale(
    payload: &std::collections::BTreeMap<String, ResolvedValue>,
    natural: [f64; 2],
    mut scale: [f64; 2],
) -> Result<[f64; 2]> {
    let width = optional_optional_number(payload.get("width"), "sprite width")?;
    let height = optional_optional_number(payload.get("height"), "sprite height")?;
    if let Some(width) = width {
        scale[0] = width / natural[0];
        if height.is_none() {
            scale[1] = scale[0];
        }
    }
    if let Some(height) = height {
        scale[1] = height / natural[1];
        if width.is_none() {
            scale[0] = scale[1];
        }
    }
    if scale.iter().any(|value| !value.is_finite()) {
        return Err(Error::Invalid("sprite scale is not finite".to_owned()));
    }
    Ok(scale)
}

fn parsed_anchor(payload: &std::collections::BTreeMap<String, ResolvedValue>) -> Result<[f64; 2]> {
    let anchor = optional_vector(payload.get("anchor"), "sprite anchor")?;
    let pivot = optional_vector(payload.get("pivot"), "sprite pivot")?;
    Ok([
        anchor.and_then(|value| value[0]).unwrap_or_else(|| {
            if pivot.and_then(|value| value[0]).is_some() {
                0.0
            } else {
                0.5
            }
        }),
        anchor.and_then(|value| value[1]).unwrap_or_else(|| {
            if pivot.and_then(|value| value[1]).is_some() {
                0.0
            } else {
                0.5
            }
        }),
    ])
}

fn parsed_pivot(
    payload: &std::collections::BTreeMap<String, ResolvedValue>,
    scale: [f64; 2],
) -> Result<[f64; 2]> {
    let pivot = optional_vector(payload.get("pivot"), "sprite pivot")?.unwrap_or([None, None]);
    let mut result = [0.0, 0.0];
    for axis in 0..2 {
        if let Some(value) = pivot[axis] {
            result[axis] = value / scale[axis];
            if !result[axis].is_finite() {
                return Err(Error::Invalid("sprite pivot is not finite".to_owned()));
            }
        }
    }
    Ok(result)
}

fn optional_vector(value: Option<&ResolvedValue>, label: &str) -> Result<Option<[Option<f64>; 2]>> {
    let Some(value) = value.filter(|value| !matches!(value, ResolvedValue::Undefined)) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("{label} must be an object")))?;
    Ok(Some([
        optional_optional_number(object.get("x"), &format!("{label}.x"))?,
        optional_optional_number(object.get("y"), &format!("{label}.y"))?,
    ]))
}

fn vector(value: Option<&ResolvedValue>, default: [f64; 2], label: &str) -> Result<[f64; 2]> {
    let Some(vector) = optional_vector(value, label)? else {
        return Ok(default);
    };
    Ok([
        vector[0].unwrap_or(default[0]),
        vector[1].unwrap_or(default[1]),
    ])
}

fn optional_number(value: Option<&ResolvedValue>, default: f64, label: &str) -> Result<f64> {
    optional_optional_number(value, label).map(|value| value.unwrap_or(default))
}

fn optional_optional_number(value: Option<&ResolvedValue>, label: &str) -> Result<Option<f64>> {
    match value {
        None | Some(ResolvedValue::Undefined) => Ok(None),
        Some(ResolvedValue::Number(value)) if value.is_finite() => Ok(Some(*value)),
        Some(_) => Err(Error::Invalid(format!("{label} must resolve to a number"))),
    }
}

fn color(value: Option<&ResolvedValue>, default: u32, label: &str) -> Result<u32> {
    let value = optional_number(value, f64::from(default), label)?;
    Ok(value.floor().clamp(0.0, 16_777_215.0) as u32)
}

fn blend_mode(value: Option<&ResolvedValue>) -> Result<SpriteBlendMode> {
    match optional_number(value, 0.0, "sprite blendMode")? {
        0.0 => Ok(SpriteBlendMode::Normal),
        1.0 => Ok(SpriteBlendMode::Add),
        2.0 => Ok(SpriteBlendMode::Multiply),
        3.0 => Ok(SpriteBlendMode::Screen),
        value => Err(Error::Invalid(format!(
            "native sprite path does not implement Pixi blend mode {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{
        ActionManagerRuntime, AtlasEntry, BoardTransform, ProcessorKind, ResolvedActivation,
        ResolvedScene, ResolvedValue, SceneFrameScratch, SceneNodeKind, SceneNodeTemplates,
        SpriteBlendMode, SpriteDisplayEntry, TextureAtlas, TextureAtlasPage, VectorCommand,
    };

    #[test]
    fn lowers_sprite_dimensions_anchor_pivot_color_and_blend_like_pixi() {
        let entry = AtlasEntry {
            page: 0,
            x: 0,
            y: 0,
            width: 400,
            height: 200,
            logical_width: 100.0,
            logical_height: 50.0,
            u_min: 0.0,
            v_min: 0.0,
            u_max: 1.0,
            v_max: 1.0,
        };
        let atlas = TextureAtlas {
            entries: BTreeMap::from([("unit".to_owned(), entry)]),
            pages: vec![TextureAtlasPage {
                width: 400,
                height: 200,
                rgba: vec![0; 400 * 200 * 4],
            }],
            padding: 1,
        };
        let payload = ResolvedValue::Object(BTreeMap::from([
            (
                "id".to_owned(),
                ResolvedValue::String("__root__".to_owned()),
            ),
            (
                "texture".to_owned(),
                ResolvedValue::String("unit".to_owned()),
            ),
            ("width".to_owned(), ResolvedValue::Number(200.0)),
            (
                "pivot".to_owned(),
                ResolvedValue::Object(BTreeMap::from([(
                    "y".to_owned(),
                    ResolvedValue::Number(25.0),
                )])),
            ),
            ("x".to_owned(), ResolvedValue::Number(12.0)),
            ("tint".to_owned(), ResolvedValue::Number(20_000_000.9)),
            ("blendMode".to_owned(), ResolvedValue::Number(3.0)),
        ]));
        let scene = ResolvedScene {
            activations: vec![
                ResolvedActivation::Object {
                    entity_id: "one".to_owned(),
                    object_type: "unit".to_owned(),
                    layer: Some("objects".to_owned()),
                    z_index: 2.0,
                    activation_order: 0,
                    start_tick: 1,
                    end_tick: 5,
                    data: ResolvedValue::Object(BTreeMap::from([
                        ("x".to_owned(), ResolvedValue::Number(100.0)),
                        ("y".to_owned(), ResolvedValue::Number(50.0)),
                    ])),
                },
                ResolvedActivation::Processor {
                    entity_id: "one".to_owned(),
                    object_type: "unit".to_owned(),
                    definition_id: "sprite-def".to_owned(),
                    scope_id: "body".to_owned(),
                    kind: ProcessorKind::Sprite,
                    layer: Some("objects".to_owned()),
                    z_index: 4.0,
                    activation_order: 3,
                    start_tick: 1,
                    end_tick: 5,
                    payload,
                    object_texture: None,
                    node_id: Some("__root__".to_owned()),
                    target_is_root: false,
                    touches_node: true,
                    temporary_node: false,
                    actions: vec![],
                },
            ],
            final_random_state: 0,
        };
        let templates = SceneNodeTemplates::compile(&scene, &atlas).unwrap();
        assert_eq!(templates.nodes.len(), 2);
        let node = &templates.nodes[1];
        assert_eq!(node.node_id, "__root__");
        assert!(!node.is_root);
        assert_eq!(node.transform.position, [12.0, 0.0]);
        assert_eq!(node.transform.scale, [2.0, 2.0]);
        assert_eq!(node.transform.pivot, [0.0, 12.5]);
        let SceneNodeKind::Sprite {
            natural_size,
            anchor,
            tint,
            blend_mode,
            ..
        } = &node.kind
        else {
            panic!("expected sprite")
        };
        assert_eq!(*natural_size, [100.0, 50.0]);
        assert_eq!(*anchor, [0.5, 0.0]);
        assert_eq!(*tint, 0x00ff_ffff);
        assert_eq!(*blend_mode, SpriteBlendMode::Screen);

        let prepared = templates
            .prepare_at(
                2,
                BoardTransform {
                    zoom: 2.0,
                    position: [10.0, 20.0],
                    pivot: [0.0, 0.0],
                },
            )
            .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].transform.a, 4.0);
        assert_eq!(prepared[0].transform.d, 4.0);
        assert_eq!(prepared[0].transform.tx, 234.0);
        assert_eq!(prepared[0].transform.ty, 70.0);
        let instance = prepared[0].gpu_instance().unwrap();
        assert_eq!(instance.transform_x, [4.0, 0.0, 234.0, 0.0]);
        assert_eq!(instance.transform_y, [0.0, 4.0, 70.0, 0.0]);
        assert_eq!(instance.size_anchor, [100.0, 50.0, 0.5, 0.0]);

        let mut target = node.initial_action_target();
        target.x = 20.0;
        target.scale_x = 3.0;
        target.alpha = 0.25;
        target.tint = 0x0012_3456;
        target.filters = vec![BTreeMap::from([("blur".to_owned(), 0.0)])];
        let prepared = templates
            .prepare_with_action_targets_at(
                2,
                BoardTransform {
                    zoom: 2.0,
                    position: [10.0, 20.0],
                    pivot: [0.0, 0.0],
                },
                &BTreeMap::from([(node.activation_order, target)]),
            )
            .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].transform.a, 6.0);
        assert_eq!(prepared[0].transform.d, 4.0);
        assert_eq!(prepared[0].transform.tx, 250.0);
        assert_eq!(prepared[0].alpha, 0.25);
        assert_eq!(prepared[0].tint, 0x0012_3456);
        assert_eq!(prepared[0].blur, Some(0.0));
        let filtered = prepared[0].gpu_instance().unwrap();
        assert_eq!(filtered.blur, 0.0);
        assert_eq!(filtered.has_blur_filter, 1);

        let root = &templates.nodes[0];
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(
                root.activation_order,
                root.key(),
                root.initial_action_target(),
            )
            .unwrap();
        manager
            .create_target_with_parent(
                node.activation_order,
                node.key(),
                node.initial_action_target(),
                Some(root.activation_order),
            )
            .unwrap();
        let board = BoardTransform {
            zoom: 2.0,
            position: [10.0, 20.0],
            pivot: [0.0, 0.0],
        };
        let prepared = templates
            .prepare_with_action_manager_at(2, board, &manager)
            .unwrap();
        assert_eq!(prepared.len(), 1);
        let mut scratch = SceneFrameScratch::default();
        let direct = templates
            .prepare_gpu_instances_with_action_manager(
                board,
                &manager,
                &[SpriteDisplayEntry {
                    activation_order: node.activation_order,
                    layer_order: 0,
                }],
                &mut scratch,
            )
            .unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].activation_order, node.activation_order);
        assert_eq!(direct[0].blend_mode, SpriteBlendMode::Screen);
        assert_eq!(direct[0].instance, prepared[0].gpu_instance().unwrap());

        manager.destroy_target(node.activation_order).unwrap();
        assert!(
            templates
                .prepare_with_action_manager_at(2, board, &manager)
                .unwrap()
                .is_empty()
        );
        assert!(
            templates
                .prepare_gpu_instances_with_action_manager(
                    board,
                    &manager,
                    &[SpriteDisplayEntry {
                        activation_order: node.activation_order,
                        layer_order: 0,
                    }],
                    &mut scratch,
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lowers_draw_and_changed_site_progress_to_action_addressable_vector_nodes() {
        let drawing = ResolvedValue::Object(BTreeMap::from([
            (
                "method".to_owned(),
                ResolvedValue::String("drawCircle".to_owned()),
            ),
            (
                "params".to_owned(),
                ResolvedValue::Array(vec![
                    ResolvedValue::Number(3.0),
                    ResolvedValue::Number(4.0),
                    ResolvedValue::Number(5.0),
                ]),
            ),
        ]));
        let scene = ResolvedScene {
            activations: vec![
                ResolvedActivation::Object {
                    entity_id: "site-1".to_owned(),
                    object_type: "constructionSite".to_owned(),
                    layer: Some("objects".to_owned()),
                    z_index: 2.0,
                    activation_order: 0,
                    start_tick: 1,
                    end_tick: 5,
                    data: ResolvedValue::Object(BTreeMap::new()),
                },
                ResolvedActivation::Processor {
                    entity_id: "site-1".to_owned(),
                    object_type: "constructionSite".to_owned(),
                    definition_id: "draw-def".to_owned(),
                    scope_id: "body".to_owned(),
                    kind: ProcessorKind::Draw,
                    layer: Some("objects".to_owned()),
                    z_index: 2.0,
                    activation_order: 1,
                    start_tick: 1,
                    end_tick: 5,
                    payload: ResolvedValue::Object(BTreeMap::from([
                        ("drawings".to_owned(), ResolvedValue::Array(vec![drawing])),
                        ("x".to_owned(), ResolvedValue::Number(11.0)),
                        ("y".to_owned(), ResolvedValue::Number(12.0)),
                        (
                            "scale".to_owned(),
                            ResolvedValue::Object(BTreeMap::from([
                                ("x".to_owned(), ResolvedValue::Number(2.0)),
                                ("y".to_owned(), ResolvedValue::Number(2.0)),
                            ])),
                        ),
                        ("tint".to_owned(), ResolvedValue::Number(0x12_34_56 as f64)),
                        ("blendMode".to_owned(), ResolvedValue::Number(1.0)),
                        ("blur".to_owned(), ResolvedValue::Number(3.0)),
                    ])),
                    object_texture: None,
                    node_id: Some("body".to_owned()),
                    target_is_root: false,
                    touches_node: true,
                    temporary_node: false,
                    actions: vec![],
                },
                ResolvedActivation::Processor {
                    entity_id: "site-1".to_owned(),
                    object_type: "constructionSite".to_owned(),
                    definition_id: "progress-def".to_owned(),
                    scope_id: "progress".to_owned(),
                    kind: ProcessorKind::SiteProgress,
                    layer: Some("objects".to_owned()),
                    z_index: 2.0,
                    activation_order: 2,
                    start_tick: 1,
                    end_tick: 5,
                    payload: ResolvedValue::Object(BTreeMap::from([
                        ("progress".to_owned(), ResolvedValue::Number(25.0)),
                        ("progressTotal".to_owned(), ResolvedValue::Number(100.0)),
                        ("color".to_owned(), ResolvedValue::Number(0xaa_bb_cc as f64)),
                        ("radius".to_owned(), ResolvedValue::Number(10.0)),
                        ("lineWidth".to_owned(), ResolvedValue::Number(2.0)),
                    ])),
                    object_texture: None,
                    node_id: Some("progress".to_owned()),
                    target_is_root: false,
                    touches_node: true,
                    temporary_node: false,
                    actions: vec![],
                },
                ResolvedActivation::Processor {
                    entity_id: "site-1".to_owned(),
                    object_type: "constructionSite".to_owned(),
                    definition_id: "progress-early-return".to_owned(),
                    scope_id: "progress".to_owned(),
                    kind: ProcessorKind::SiteProgress,
                    layer: Some("objects".to_owned()),
                    z_index: 2.0,
                    activation_order: 3,
                    start_tick: 2,
                    end_tick: 5,
                    payload: ResolvedValue::Undefined,
                    object_texture: None,
                    node_id: Some("progress".to_owned()),
                    target_is_root: false,
                    touches_node: false,
                    temporary_node: false,
                    actions: vec![],
                },
            ],
            final_random_state: 0,
        };
        let atlas = TextureAtlas {
            entries: BTreeMap::new(),
            pages: Vec::new(),
            padding: 1,
        };

        let templates = SceneNodeTemplates::compile(&scene, &atlas).unwrap();
        assert_eq!(templates.nodes.len(), 3);

        let draw = &templates.nodes[1];
        assert_eq!(draw.node_id, "body");
        assert_eq!(draw.transform.position, [11.0, 12.0]);
        assert_eq!(draw.transform.scale, [2.0, 2.0]);
        let SceneNodeKind::Vector {
            program,
            mesh,
            tint,
            blend_mode,
            blur,
        } = &draw.kind
        else {
            panic!("expected draw vector node")
        };
        assert_eq!(
            program.commands,
            vec![VectorCommand::Circle {
                center: [3.0, 4.0],
                radius: 5.0,
            }]
        );
        assert!(mesh.vertices().is_empty());
        assert_eq!(*tint, 0x12_34_56);
        assert_eq!(*blend_mode, SpriteBlendMode::Add);
        assert_eq!(*blur, Some(3.0));
        let draw_target = draw.initial_action_target();
        assert_eq!(draw_target.tint, 0x12_34_56);
        assert_eq!(
            draw_target.filters,
            vec![BTreeMap::from([("blur".to_owned(), 3.0)])]
        );

        let progress = &templates.nodes[2];
        assert_eq!(progress.node_id, "progress");
        assert_eq!(progress.transform.position, [0.0, 0.0]);
        assert_eq!(progress.transform.scale, [1.0, 1.0]);
        assert_eq!(progress.transform.rotation, -std::f64::consts::FRAC_PI_2);
        let SceneNodeKind::Vector {
            program,
            mesh,
            tint,
            blend_mode,
            blur,
        } = &progress.kind
        else {
            panic!("expected site progress vector node")
        };
        assert_eq!(*tint, 0x00ff_ffff);
        assert_eq!(*blend_mode, SpriteBlendMode::Normal);
        assert_eq!(*blur, None);
        assert!(!mesh.vertices().is_empty());
        assert_eq!(program.commands.len(), 10);
        assert!(matches!(
            program.commands[2],
            VectorCommand::Circle { radius: 11.0, .. }
        ));
        assert!(matches!(
            program.commands[7],
            VectorCommand::Arc { end_angle, .. }
                if end_angle == std::f64::consts::FRAC_PI_2
        ));
    }
}
