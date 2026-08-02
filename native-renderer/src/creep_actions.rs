use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ActionKind, ActionManagerRuntime, Error, ResolvedActionNode, ResolvedActionParameter,
    ResolvedActivation, ResolvedScene, ResolvedValue, Result, SceneNodeKey, SceneNodeKind,
    SceneNodeTemplate, SpriteBlendMode, TextureAtlas, VectorCommand, VectorLineStyle,
    VectorProgram,
};

pub const ADAPTER_MARKER: &str = "$nativeCreepActions";
const CAPTURED_ACTION_KEYS: [&str; 6] = [
    "attack",
    "attacked",
    "heal",
    "healed",
    "rangedAttack",
    "rangedHeal",
];
const ACTION_GROUP_NAMESPACE: u64 = 1_u64 << 62;
const GROUP_ORDINAL_BITS: u32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShotColor {
    Attack,
    Heal,
}

impl ShotColor {
    pub const fn tint(self) -> u32 {
        match self {
            Self::Attack => 0x3c_75_c7,
            Self::Heal => 0x2c_e3_28,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShotPlan {
    pub target: [f64; 2],
    pub color: ShotColor,
    pub width: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BitePlan {
    pub target: [f64; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub enum CapturedEffect {
    Shot(ShotPlan),
    Bite(BitePlan),
    Flash(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreepActionsPlan {
    pub parent_id: Option<String>,
    pub first_run: bool,
    pub state_position: [f64; 2],
    pub tick_duration: f64,
    pub effects: Vec<CapturedEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShotActivations {
    pub crisp: u32,
    pub blurred: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PersistentActivations {
    pub external_container: u32,
    pub cover: u32,
    pub flare: u32,
    pub lighting: u32,
    pub child_cover: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreepActionsRun {
    pub plan: CreepActionsPlan,
    pub persistent: Option<PersistentActivations>,
    pub shots: Vec<ShotActivations>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CreepActionsTemplates {
    pub runs: BTreeMap<u32, CreepActionsRun>,
}

#[derive(Clone, Debug)]
struct LiveShot {
    activation: u32,
    source: [f64; 2],
    target: [f64; 2],
    duration_ms: f64,
    rest_ms: f64,
}

#[derive(Clone, Debug, Default)]
pub struct CreepActionsRuntime {
    active_persistent: BTreeMap<String, PersistentActivations>,
    live_shots: Vec<LiveShot>,
}

pub fn compile_templates(
    scene: &ResolvedScene,
    atlas: &TextureAtlas,
    first_synthetic_activation: u32,
) -> Result<(CreepActionsTemplates, Vec<SceneNodeTemplate>)> {
    let mut next_activation = first_synthetic_activation;
    let mut active_persistent = BTreeMap::<String, Option<PersistentActivations>>::new();
    let mut templates = CreepActionsTemplates::default();
    let mut nodes = Vec::new();
    for activation in &scene.activations {
        let ResolvedActivation::Processor {
            entity_id,
            definition_id,
            scope_id,
            kind: crate::ProcessorKind::CreepActions,
            activation_order,
            start_tick,
            end_tick,
            payload,
            ..
        } = activation
        else {
            continue;
        };
        let Some(plan) = decode_plan(payload)? else {
            continue;
        };
        if plan.first_run {
            active_persistent.insert(entity_id.clone(), None);
            templates.runs.insert(
                *activation_order,
                CreepActionsRun {
                    plan,
                    persistent: None,
                    shots: Vec::new(),
                },
            );
            continue;
        }
        let persistent = match active_persistent.get(entity_id).copied().flatten() {
            Some(activations) => activations,
            None => {
                let cover = atlas.entries.get("cover").copied().ok_or_else(|| {
                    Error::Invalid("creepActions references missing atlas texture cover".to_owned())
                })?;
                let flare = atlas.entries.get("flare2").copied().ok_or_else(|| {
                    Error::Invalid(
                        "creepActions references missing atlas texture flare2".to_owned(),
                    )
                })?;
                let glow = atlas.entries.get("glow").copied().ok_or_else(|| {
                    Error::Invalid("creepActions references missing atlas texture glow".to_owned())
                })?;
                let activations = PersistentActivations {
                    external_container: take_activation(&mut next_activation)?,
                    cover: take_activation(&mut next_activation)?,
                    flare: take_activation(&mut next_activation)?,
                    lighting: take_activation(&mut next_activation)?,
                    child_cover: take_activation(&mut next_activation)?,
                };
                active_persistent.insert(entity_id.clone(), Some(activations));
                let external_entity = format!(
                    "$native.stage.creepActions.persistent.{}",
                    activations.external_container
                );
                nodes.push(SceneNodeTemplate {
                    entity_id: external_entity.clone(),
                    definition_id: definition_id.clone(),
                    scope_id: scope_id.clone(),
                    node_id: "__root__".to_owned(),
                    is_root: true,
                    parent_id: None,
                    layer: None,
                    z_index: 0.0,
                    activation_order: activations.external_container,
                    start_tick: *start_tick,
                    end_tick: u32::MAX,
                    transform: crate::NodeTransform {
                        position: [0.0, 0.0],
                        scale: [1.0, 1.0],
                        rotation: 0.0,
                        pivot: [0.0, 0.0],
                    },
                    alpha: 0.0,
                    visible: true,
                    kind: SceneNodeKind::Container,
                });
                for (activation, texture, entry, size, alpha, layer, blend_mode) in [
                    (
                        activations.cover,
                        "cover",
                        cover,
                        None,
                        0.3,
                        "effects",
                        SpriteBlendMode::Normal,
                    ),
                    (
                        activations.flare,
                        "flare2",
                        flare,
                        Some(300.0),
                        0.05,
                        "effects",
                        SpriteBlendMode::Add,
                    ),
                    (
                        activations.lighting,
                        "glow",
                        glow,
                        Some(500.0),
                        0.5,
                        "lighting",
                        SpriteBlendMode::Normal,
                    ),
                ] {
                    let natural_size = [
                        f64::from(entry.logical_width),
                        f64::from(entry.logical_height),
                    ];
                    let scale = size.map_or([1.0, 1.0], |size| {
                        [size / natural_size[0], size / natural_size[1]]
                    });
                    nodes.push(SceneNodeTemplate {
                        entity_id: external_entity.clone(),
                        definition_id: definition_id.clone(),
                        scope_id: scope_id.clone(),
                        node_id: format!("$native.creepActions.{texture}.{activation}"),
                        is_root: false,
                        parent_id: Some("__root__".to_owned()),
                        layer: Some(layer.to_owned()),
                        z_index: 0.0,
                        activation_order: activation,
                        start_tick: *start_tick,
                        end_tick: u32::MAX,
                        transform: crate::NodeTransform {
                            position: [0.0, 0.0],
                            scale,
                            rotation: 0.0,
                            pivot: [0.0, 0.0],
                        },
                        alpha,
                        visible: true,
                        kind: SceneNodeKind::Sprite {
                            texture: texture.to_owned(),
                            atlas: entry,
                            natural_size,
                            anchor: [0.5, 0.5],
                            pixel_snap: false,
                            tint: 0x00ff_ffff,
                            blend_mode,
                            blur: None,
                        },
                    });
                }
                nodes.push(SceneNodeTemplate {
                    entity_id: entity_id.clone(),
                    definition_id: definition_id.clone(),
                    scope_id: scope_id.clone(),
                    node_id: format!(
                        "$native.creepActions.childCover.{}",
                        activations.child_cover
                    ),
                    is_root: false,
                    parent_id: plan.parent_id.clone(),
                    layer: Some("effects".to_owned()),
                    z_index: 0.0,
                    activation_order: activations.child_cover,
                    start_tick: *start_tick,
                    end_tick: u32::MAX,
                    transform: crate::NodeTransform {
                        position: [0.0, 0.0],
                        scale: [1.0, 1.0],
                        rotation: 0.0,
                        pivot: [0.0, 0.0],
                    },
                    alpha: 0.0,
                    visible: true,
                    kind: SceneNodeKind::Sprite {
                        texture: "cover".to_owned(),
                        atlas: cover,
                        natural_size: [
                            f64::from(cover.logical_width),
                            f64::from(cover.logical_height),
                        ],
                        anchor: [0.5, 0.5],
                        pixel_snap: false,
                        tint: 0x00ff_ffff,
                        blend_mode: SpriteBlendMode::Add,
                        blur: None,
                    },
                });
                activations
            }
        };
        let mut shots = Vec::new();
        let mut shot_ordinal = 0_u32;
        for effect in &plan.effects {
            let CapturedEffect::Shot(shot) = effect else {
                continue;
            };
            let crisp = take_activation(&mut next_activation)?;
            let blurred = take_activation(&mut next_activation)?;
            for (activation, alpha, blur) in [(crisp, 1.0, None), (blurred, 0.7, Some(5.0))] {
                let program = VectorProgram {
                    commands: vec![
                        VectorCommand::LineStyle(VectorLineStyle {
                            width: shot.width,
                            color: shot.color.tint(),
                            alpha: 1.0,
                            alignment: 0.5,
                            native: false,
                        }),
                        VectorCommand::MoveTo([0.0, 0.0]),
                        VectorCommand::LineTo([1.0, 0.0]),
                    ],
                };
                let mesh = crate::tessellate_vector_program(&program)?;
                nodes.push(SceneNodeTemplate {
                    entity_id: format!(
                        "$native.stage.creepActions.{activation_order}.{shot_ordinal}"
                    ),
                    definition_id: definition_id.clone(),
                    scope_id: scope_id.clone(),
                    node_id: "__root__".to_owned(),
                    is_root: true,
                    parent_id: None,
                    layer: Some("effects".to_owned()),
                    z_index: 0.0,
                    activation_order: activation,
                    start_tick: *start_tick,
                    end_tick: *end_tick,
                    transform: crate::NodeTransform {
                        position: [0.0, 0.0],
                        scale: [0.0, 1.0],
                        rotation: 0.0,
                        pivot: [0.0, 0.0],
                    },
                    alpha,
                    visible: true,
                    kind: SceneNodeKind::Vector {
                        program,
                        mesh,
                        tint: 0x00ff_ffff,
                        blend_mode: SpriteBlendMode::Add,
                        blur,
                    },
                });
                shot_ordinal = shot_ordinal
                    .checked_add(1)
                    .ok_or(Error::ArithmeticOverflow)?;
            }
            shots.push(ShotActivations { crisp, blurred });
        }
        if templates
            .runs
            .insert(
                *activation_order,
                CreepActionsRun {
                    plan,
                    persistent: Some(persistent),
                    shots,
                },
            )
            .is_some()
        {
            return Err(Error::Invalid(
                "creepActions repeats a source activation identity".to_owned(),
            ));
        }
    }
    Ok((templates, nodes))
}

fn take_activation(next: &mut u32) -> Result<u32> {
    let activation = *next;
    *next = next.checked_add(1).ok_or_else(|| {
        Error::Invalid("creepActions synthetic activation IDs overflow u32".to_owned())
    })?;
    Ok(activation)
}

pub fn lower_supported_payload(
    payload: &ResolvedValue,
    state: &ResolvedValue,
    previous_state: Option<&ResolvedValue>,
    tick_duration: f64,
    world_options: &serde_json::Value,
) -> Result<Option<ResolvedValue>> {
    let payload = payload.as_object().ok_or_else(|| {
        Error::Invalid("creepActions payload must resolve to an object".to_owned())
    })?;
    let parent_id = payload
        .get("parentId")
        .filter(|value| crate::value_plan::resolved_js_truthy(value))
        .map(crate::value_plan::js_property_key)
        .transpose()?;
    if !tick_duration.is_finite() || tick_duration <= 0.0 {
        return Err(Error::Invalid(
            "creepActions tick duration must be positive and finite".to_owned(),
        ));
    }
    let options = world_options
        .as_object()
        .ok_or_else(|| Error::Invalid("creepActions requires object worldOptions".to_owned()))?;
    let cell_size = json_positive_number(options.get("CELL_SIZE"), "worldOptions.CELL_SIZE")?;
    let attack_penetration = json_positive_number(
        options.get("ATTACK_PENETRATION"),
        "worldOptions.ATTACK_PENETRATION",
    )?;
    if attack_penetration != 10.0 {
        return Ok(None);
    }
    let state = state
        .as_object()
        .ok_or_else(|| Error::Invalid("creepActions state must resolve to an object".to_owned()))?;
    let state_position = game_position(state, cell_size, "creepActions state")?;
    let state_type = state
        .get("type")
        .and_then(ResolvedValue::as_string)
        .unwrap_or("");

    let Some(previous_state) = previous_state else {
        return Ok(Some(encode_plan(&CreepActionsPlan {
            parent_id,
            first_run: true,
            state_position,
            tick_duration,
            effects: Vec::new(),
        })));
    };
    let previous_state = previous_state.as_object().ok_or_else(|| {
        Error::Invalid("creepActions previous state must resolve to an object".to_owned())
    })?;
    let previous_position =
        game_position(previous_state, cell_size, "creepActions previous state")?;
    let position_changed = state_position != previous_position;
    let log = match state.get("actionLog") {
        None | Some(ResolvedValue::Undefined) => None,
        Some(value) => Some(value.as_object().ok_or_else(|| {
            Error::Invalid("creepActions actionLog must resolve to an object".to_owned())
        })?),
    };
    let Some(log) = log else {
        return Ok(Some(encode_plan(&CreepActionsPlan {
            parent_id,
            first_run: false,
            state_position,
            tick_duration,
            effects: Vec::new(),
        })));
    };
    if log.iter().any(|(key, value)| {
        !CAPTURED_ACTION_KEYS.contains(&key.as_str())
            && crate::value_plan::resolved_js_truthy(value)
    }) {
        return Ok(None);
    }

    let mut effects = Vec::new();
    if let Some(target) = action_target(log, "attack", cell_size)? {
        if state_type == "tower" {
            effects.push(CapturedEffect::Shot(ShotPlan {
                target,
                color: ShotColor::Attack,
                width: 18.0,
            }));
        } else if !position_changed && target != state_position {
            effects.push(CapturedEffect::Bite(BitePlan { target }));
        }
    }
    if let Some(target) = action_target(log, "heal", cell_size)? {
        if state_type == "tower" {
            effects.push(CapturedEffect::Shot(ShotPlan {
                target,
                color: ShotColor::Heal,
                width: 18.0,
            }));
        } else if !position_changed && target != state_position {
            effects.push(CapturedEffect::Bite(BitePlan { target }));
        }
    }
    let attacked = truthy(log.get("attacked"));
    let healed = truthy(log.get("healed"));
    if attacked || healed {
        effects.push(CapturedEffect::Flash(match (attacked, healed) {
            (true, true) => 0xff_ff_33,
            (false, true) => 0x2c_e3_28,
            (true, false) => 0xff_33_33,
            (false, false) => unreachable!(),
        }));
    }
    if let Some(target) = action_target(log, "rangedAttack", cell_size)? {
        effects.push(CapturedEffect::Shot(ShotPlan {
            target,
            color: ShotColor::Attack,
            width: 12.0,
        }));
    }
    if let Some(target) = action_target(log, "rangedHeal", cell_size)? {
        effects.push(CapturedEffect::Shot(ShotPlan {
            target,
            color: ShotColor::Heal,
            width: 12.0,
        }));
    }
    Ok(Some(encode_plan(&CreepActionsPlan {
        parent_id,
        first_run: false,
        state_position,
        tick_duration,
        effects,
    })))
}

pub fn decode_plan(payload: &ResolvedValue) -> Result<Option<CreepActionsPlan>> {
    let Some(payload) = payload.as_object() else {
        return Ok(None);
    };
    if payload.get(ADAPTER_MARKER) != Some(&ResolvedValue::Bool(true)) {
        return Ok(None);
    }
    let parent_id = match payload.get("parentId") {
        None | Some(ResolvedValue::Undefined) => None,
        Some(value) => Some(
            value
                .as_string()
                .ok_or_else(|| {
                    Error::Invalid("lowered creepActions parentId must be a string".to_owned())
                })?
                .to_owned(),
        ),
    };
    let first_run = payload.get("firstRun") == Some(&ResolvedValue::Bool(true));
    let state_position = decoded_pair(payload.get("statePosition"), "statePosition")?;
    let tick_duration = finite_number(payload.get("tickDuration"), "tickDuration")?;
    if tick_duration <= 0.0 {
        return Err(Error::Invalid(
            "lowered creepActions tickDuration must be positive".to_owned(),
        ));
    }
    let effects = match payload.get("effects") {
        Some(ResolvedValue::Array(effects)) => effects,
        _ => {
            return Err(Error::Invalid(
                "lowered creepActions effects must be an array".to_owned(),
            ));
        }
    };
    let effects = effects
        .iter()
        .map(decode_effect)
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(CreepActionsPlan {
        parent_id,
        first_run,
        state_position,
        tick_duration,
        effects,
    }))
}

impl CreepActionsRuntime {
    pub fn remove_entity(
        &mut self,
        entity_id: &str,
        actions: &mut ActionManagerRuntime,
        retain_child_cover: bool,
    ) -> Result<()> {
        let Some(persistent) = self.active_persistent.remove(entity_id) else {
            return Ok(());
        };
        for activation in [
            persistent.cover,
            persistent.flare,
            persistent.lighting,
            persistent.external_container,
        ] {
            if actions.target(activation).is_some() {
                actions.destroy_target(activation)?;
            }
        }
        if !retain_child_cover && actions.target(persistent.child_cover).is_some() {
            actions.cancel_for_target(persistent.child_cover);
            actions.destroy_target(persistent.child_cover)?;
        }
        Ok(())
    }

    pub fn advance(
        &mut self,
        delta_seconds: f64,
        actions: &mut ActionManagerRuntime,
    ) -> Result<bool> {
        let delta_ms = delta_seconds * 1_000.0;
        let mut destroyed = Vec::new();
        for shot in &mut self.live_shots {
            shot.rest_ms -= delta_ms;
            if shot.rest_ms <= 0.0 {
                destroyed.push(shot.activation);
                continue;
            }
            let target = actions.target_mut(shot.activation).ok_or_else(|| {
                Error::Invalid(format!(
                    "creepActions shot {} lost its action target",
                    shot.activation
                ))
            })?;
            let ratio = shot.rest_ms / shot.duration_ms;
            let (start_fraction, length_fraction) = if ratio < 0.5 {
                (1.0, ratio * 2.0)
            } else {
                let retract = (ratio - 0.5) * 2.0;
                (1.0 - retract, 1.0 - retract)
            };
            let dx = shot.source[0] - shot.target[0];
            let dy = shot.source[1] - shot.target[1];
            let length = dx.hypot(dy);
            target.x = shot.source[0] + (shot.target[0] - shot.source[0]) * start_fraction;
            target.y = shot.source[1] + (shot.target[1] - shot.source[1]) * start_fraction;
            target.rotation = dy.atan2(dx);
            target.scale_x = length * length_fraction;
            target.scale_y = 1.0;
        }
        if destroyed.is_empty() {
            return Ok(false);
        }
        let destroyed = destroyed.into_iter().collect::<BTreeSet<_>>();
        self.live_shots
            .retain(|shot| !destroyed.contains(&shot.activation));
        for activation in destroyed {
            if actions.target(activation).is_some() {
                actions.cancel_for_target(activation);
                actions.destroy_target(activation)?;
            }
        }
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &mut self,
        source_activation: u32,
        entity_id: &str,
        root_activation: u32,
        run: &CreepActionsRun,
        nodes: &BTreeMap<u32, &crate::SceneNodeTemplate>,
        actions: &mut ActionManagerRuntime,
    ) -> Result<()> {
        if run.plan.first_run {
            self.active_persistent.remove(entity_id);
            return Ok(());
        }
        let parent_key = SceneNodeKey {
            entity_id: entity_id.to_owned(),
            node_id: run
                .plan
                .parent_id
                .clone()
                .unwrap_or_else(|| "__root__".to_owned()),
            is_root: run.plan.parent_id.is_none(),
        };
        let Some(parent_activation) = actions.visible_activation(&parent_key) else {
            return Ok(());
        };
        let persistent = run.persistent.ok_or_else(|| {
            Error::Invalid("non-initial creepActions run lacks persistent identities".to_owned())
        })?;
        if !self.active_persistent.contains_key(entity_id) {
            let source = actions.target(root_activation).ok_or_else(|| {
                Error::Invalid("creepActions root target is unavailable".to_owned())
            })?;
            let external = nodes.get(&persistent.external_container).ok_or_else(|| {
                Error::Invalid("creepActions external-cover template is missing".to_owned())
            })?;
            let mut external_target = external.initial_action_target();
            external_target.x = source.x;
            external_target.y = source.y;
            actions.create_target(
                persistent.external_container,
                external.key(),
                external_target,
            )?;
            for activation in [persistent.cover, persistent.flare, persistent.lighting] {
                let node = nodes.get(&activation).ok_or_else(|| {
                    Error::Invalid("creepActions external child template is missing".to_owned())
                })?;
                actions.create_temporary_target_with_parent(
                    activation,
                    root_activation,
                    node.initial_action_target(),
                    persistent.external_container,
                )?;
            }
            let node = nodes.get(&persistent.child_cover).ok_or_else(|| {
                Error::Invalid("creepActions child-cover template is missing".to_owned())
            })?;
            actions.create_temporary_target_with_parent(
                persistent.child_cover,
                root_activation,
                node.initial_action_target(),
                parent_activation,
            )?;
            self.active_persistent
                .insert(entity_id.to_owned(), persistent);
        }

        let mut shot_index = 0_usize;
        let mut ordinal = 0_u8;
        for effect in &run.plan.effects {
            match effect {
                CapturedEffect::Shot(shot) => {
                    let activations = run.shots.get(shot_index).ok_or_else(|| {
                        Error::Invalid(
                            "creepActions shot identity count is inconsistent".to_owned(),
                        )
                    })?;
                    shot_index += 1;
                    let source_target = actions.target(root_activation).ok_or_else(|| {
                        Error::Invalid("creepActions root target is unavailable".to_owned())
                    })?;
                    let source = [source_target.x, source_target.y];
                    for activation in [activations.crisp, activations.blurred] {
                        let node = nodes.get(&activation).ok_or_else(|| {
                            Error::Invalid("creepActions shot template is missing".to_owned())
                        })?;
                        actions.create_target(
                            activation,
                            node.key(),
                            node.initial_action_target(),
                        )?;
                        let target = actions
                            .target_mut(activation)
                            .expect("just-created creepActions shot target");
                        target.x = source[0];
                        target.y = source[1];
                        target.scale_x = 0.0;
                        target.scale_y = 1.0;
                        self.live_shots.push(LiveShot {
                            activation,
                            source,
                            target: shot.target,
                            duration_ms: 600.0 * run.plan.tick_duration,
                            rest_ms: 600.0 * run.plan.tick_duration,
                        });
                    }
                }
                CapturedEffect::Bite(bite) => {
                    let state = run.plan.state_position;
                    let dx = bite.target[0] - state[0];
                    let dy = bite.target[1] - state[1];
                    let by_x = dx.signum() * 10.0;
                    let by_y = dy.signum() * 10.0;
                    let root = actions.target_mut(root_activation).ok_or_else(|| {
                        Error::Invalid("creepActions root target is unavailable".to_owned())
                    })?;
                    root.x = state[0];
                    root.y = state[1];
                    if let Some(main) = actions.addressable_activation(&SceneNodeKey {
                        entity_id: entity_id.to_owned(),
                        node_id: "mainContainer".to_owned(),
                        is_root: false,
                    }) {
                        let angle = normalized_bite_angle(state, bite.target);
                        actions.start_group(
                            action_group(source_activation, ordinal)?,
                            main,
                            &[action(
                                ActionKind::RotateTo,
                                vec![
                                    number(angle),
                                    number((run.plan.tick_duration / 5.0).max(0.4)),
                                ],
                            )],
                        )?;
                        ordinal = ordinal.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
                    }
                    let outward = action(
                        ActionKind::MoveBy,
                        vec![
                            number(by_x),
                            number(by_y),
                            number(run.plan.tick_duration / 4.0),
                        ],
                    );
                    let inward = action(
                        ActionKind::MoveBy,
                        vec![
                            number(-by_x),
                            number(-by_y),
                            number(3.0 * run.plan.tick_duration / 4.0),
                        ],
                    );
                    actions.start_group(
                        action_group(source_activation, ordinal)?,
                        root_activation,
                        &[action(
                            ActionKind::Sequence,
                            vec![action_array(vec![outward, inward])],
                        )],
                    )?;
                    ordinal = ordinal.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
                }
                CapturedEffect::Flash(tint) => {
                    let cover = self.active_persistent[entity_id].child_cover;
                    actions
                        .target_mut(cover)
                        .expect("active child-cover target")
                        .tint = *tint;
                    let phase = 0.9 * run.plan.tick_duration / 4.0;
                    let sequence = action(
                        ActionKind::Sequence,
                        vec![action_array(vec![
                            action(ActionKind::DelayTime, vec![number(0.0)]),
                            action(ActionKind::AlphaTo, vec![number(0.5), number(phase)]),
                            action(ActionKind::AlphaTo, vec![number(0.0), number(3.0 * phase)]),
                        ])],
                    );
                    actions.start_group(
                        action_group(source_activation, ordinal)?,
                        cover,
                        &[sequence],
                    )?;
                    ordinal = ordinal.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
                }
            }
        }
        if shot_index != run.shots.len() {
            return Err(Error::Invalid(
                "creepActions shot identity count is inconsistent".to_owned(),
            ));
        }
        Ok(())
    }
}

fn encode_plan(plan: &CreepActionsPlan) -> ResolvedValue {
    let mut output = BTreeMap::from([
        (ADAPTER_MARKER.to_owned(), ResolvedValue::Bool(true)),
        ("firstRun".to_owned(), ResolvedValue::Bool(plan.first_run)),
        ("statePosition".to_owned(), pair_value(plan.state_position)),
        (
            "tickDuration".to_owned(),
            ResolvedValue::Number(plan.tick_duration),
        ),
        (
            "effects".to_owned(),
            ResolvedValue::Array(plan.effects.iter().map(encode_effect).collect()),
        ),
    ]);
    if let Some(parent_id) = &plan.parent_id {
        output.insert(
            "parentId".to_owned(),
            ResolvedValue::String(parent_id.clone()),
        );
    }
    ResolvedValue::Object(output)
}

fn encode_effect(effect: &CapturedEffect) -> ResolvedValue {
    let values = match effect {
        CapturedEffect::Shot(shot) => BTreeMap::from([
            ("kind".to_owned(), ResolvedValue::String("shot".to_owned())),
            ("target".to_owned(), pair_value(shot.target)),
            (
                "color".to_owned(),
                ResolvedValue::Number(f64::from(shot.color.tint())),
            ),
            ("width".to_owned(), ResolvedValue::Number(shot.width)),
        ]),
        CapturedEffect::Bite(bite) => BTreeMap::from([
            ("kind".to_owned(), ResolvedValue::String("bite".to_owned())),
            ("target".to_owned(), pair_value(bite.target)),
        ]),
        CapturedEffect::Flash(tint) => BTreeMap::from([
            ("kind".to_owned(), ResolvedValue::String("flash".to_owned())),
            ("tint".to_owned(), ResolvedValue::Number(f64::from(*tint))),
        ]),
    };
    ResolvedValue::Object(values)
}

fn decode_effect(value: &ResolvedValue) -> Result<CapturedEffect> {
    let value = value.as_object().ok_or_else(|| {
        Error::Invalid("lowered creepActions effect must be an object".to_owned())
    })?;
    match value.get("kind").and_then(ResolvedValue::as_string) {
        Some("shot") => {
            let tint = finite_number(value.get("color"), "shot color")? as u32;
            let color = match tint {
                0x3c_75_c7 => ShotColor::Attack,
                0x2c_e3_28 => ShotColor::Heal,
                _ => {
                    return Err(Error::Invalid(format!(
                        "lowered creepActions shot has unsupported color {tint:#08x}"
                    )));
                }
            };
            let width = finite_number(value.get("width"), "shot width")?;
            if width != 12.0 && width != 18.0 {
                return Err(Error::Invalid(format!(
                    "lowered creepActions shot has unsupported width {width}"
                )));
            }
            Ok(CapturedEffect::Shot(ShotPlan {
                target: decoded_pair(value.get("target"), "shot target")?,
                color,
                width,
            }))
        }
        Some("bite") => Ok(CapturedEffect::Bite(BitePlan {
            target: decoded_pair(value.get("target"), "bite target")?,
        })),
        Some("flash") => {
            let tint = finite_number(value.get("tint"), "flash tint")? as u32;
            if ![0xff_ff_33, 0x2c_e3_28, 0xff_33_33].contains(&tint) {
                return Err(Error::Invalid(format!(
                    "lowered creepActions flash has unsupported tint {tint:#08x}"
                )));
            }
            Ok(CapturedEffect::Flash(tint))
        }
        _ => Err(Error::Invalid(
            "lowered creepActions effect kind is unsupported".to_owned(),
        )),
    }
}

fn game_position(
    state: &BTreeMap<String, ResolvedValue>,
    cell_size: f64,
    label: &str,
) -> Result<[f64; 2]> {
    Ok([
        finite_number(state.get("x"), &format!("{label}.x"))? * cell_size,
        finite_number(state.get("y"), &format!("{label}.y"))? * cell_size,
    ])
}

fn action_target(
    log: &BTreeMap<String, ResolvedValue>,
    key: &str,
    cell_size: f64,
) -> Result<Option<[f64; 2]>> {
    let Some(value) = log.get(key).filter(|value| truthy(Some(value))) else {
        return Ok(None);
    };
    let value = value
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("creepActions {key} target must be an object")))?;
    Ok(Some(game_position(
        value,
        cell_size,
        &format!("creepActions {key}"),
    )?))
}

fn truthy(value: Option<&ResolvedValue>) -> bool {
    value.is_some_and(crate::value_plan::resolved_js_truthy)
}

fn pair_value(value: [f64; 2]) -> ResolvedValue {
    ResolvedValue::Array(vec![
        ResolvedValue::Number(value[0]),
        ResolvedValue::Number(value[1]),
    ])
}

fn decoded_pair(value: Option<&ResolvedValue>, label: &str) -> Result<[f64; 2]> {
    let Some(ResolvedValue::Array(value)) = value else {
        return Err(Error::Invalid(format!(
            "lowered creepActions {label} must be a pair"
        )));
    };
    if value.len() != 2 {
        return Err(Error::Invalid(format!(
            "lowered creepActions {label} must be a pair"
        )));
    }
    Ok([
        finite_number(value.first(), label)?,
        finite_number(value.get(1), label)?,
    ])
}

fn finite_number(value: Option<&ResolvedValue>, label: &str) -> Result<f64> {
    let value = value
        .and_then(ResolvedValue::as_number)
        .ok_or_else(|| Error::Invalid(format!("lowered creepActions {label} must be a number")))?;
    if !value.is_finite() {
        return Err(Error::Invalid(format!(
            "lowered creepActions {label} must be finite"
        )));
    }
    Ok(value)
}

fn json_positive_number(value: Option<&serde_json::Value>, label: &str) -> Result<f64> {
    let value = value
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| Error::Invalid(format!("creepActions {label} must be a number")))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::Invalid(format!(
            "creepActions {label} must be positive and finite"
        )));
    }
    Ok(value)
}

fn normalized_bite_angle(from: [f64; 2], to: [f64; 2]) -> f64 {
    let mut angle = (to[1] - from[1]).atan2(to[0] - from[0]) + std::f64::consts::FRAC_PI_2;
    if angle > std::f64::consts::PI {
        angle -= std::f64::consts::TAU;
    } else if angle < -std::f64::consts::PI {
        angle += std::f64::consts::TAU;
    }
    angle
}

fn action_group(source_activation: u32, ordinal: u8) -> Result<u64> {
    let body = (u64::from(source_activation) << GROUP_ORDINAL_BITS) | u64::from(ordinal);
    if body & ACTION_GROUP_NAMESPACE != 0 {
        return Err(Error::ArithmeticOverflow);
    }
    Ok(ACTION_GROUP_NAMESPACE | body)
}

fn action(kind: ActionKind, params: Vec<ResolvedActionParameter>) -> ResolvedActionNode {
    ResolvedActionNode { kind, params }
}

fn number(value: f64) -> ResolvedActionParameter {
    ResolvedActionParameter::Value(ResolvedValue::Number(value))
}

fn action_array(actions: Vec<ResolvedActionNode>) -> ResolvedActionParameter {
    ResolvedActionParameter::Array(
        actions
            .into_iter()
            .map(|action| ResolvedActionParameter::Action(Box::new(action)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        CapturedEffect, CreepActionsRuntime, LiveShot, PersistentActivations, ShotColor,
        compile_templates, decode_plan, lower_supported_payload,
    };
    use crate::{
        ActionManagerRuntime, ActionTarget, AtlasEntry, ProcessorKind, ResolvedActivation,
        ResolvedScene, ResolvedValue, SceneNodeKey, TextureAtlas,
    };

    fn value(value: serde_json::Value) -> ResolvedValue {
        ResolvedValue::from_json(&value).unwrap()
    }

    #[test]
    fn disappearing_entity_retains_child_cover_until_root_cleanup() {
        fn key(entity_id: &str, node_id: &str, is_root: bool) -> SceneNodeKey {
            SceneNodeKey {
                entity_id: entity_id.to_owned(),
                node_id: node_id.to_owned(),
                is_root,
            }
        }

        let persistent = PersistentActivations {
            external_container: 10_000,
            cover: 10_001,
            flare: 10_002,
            lighting: 10_003,
            child_cover: 10_004,
        };
        let mut actions = ActionManagerRuntime::default();
        actions
            .create_target(1, key("one", "__root__", true), ActionTarget::default())
            .unwrap();
        actions
            .create_target(
                persistent.external_container,
                key("external", "__root__", true),
                ActionTarget::default(),
            )
            .unwrap();
        for activation in [persistent.cover, persistent.flare, persistent.lighting] {
            actions
                .create_temporary_target_with_parent(
                    activation,
                    1,
                    ActionTarget::default(),
                    persistent.external_container,
                )
                .unwrap();
        }
        actions
            .create_temporary_target_with_parent(
                persistent.child_cover,
                1,
                ActionTarget::default(),
                1,
            )
            .unwrap();
        let mut runtime = CreepActionsRuntime {
            active_persistent: BTreeMap::from([("one".to_owned(), persistent)]),
            live_shots: Vec::new(),
        };

        runtime.remove_entity("one", &mut actions, true).unwrap();

        assert!(actions.is_visible_activation(persistent.child_cover));
        assert!(actions.target(persistent.external_container).is_none());
        assert!(actions.target(persistent.cover).is_none());
        let retired = actions.retire_entity_scope("one", 1);
        assert!(retired.contains(&persistent.child_cover));
    }

    #[test]
    fn lowers_only_the_authenticated_action_subset_in_official_order() {
        let state = value(json!({
            "x": 3,
            "y": 4,
            "type": "tower",
            "actionLog": {
                "attack": {"x": 4, "y": 4},
                "heal": {"x": 3, "y": 5},
                "attacked": {"x": 1, "y": 1},
                "healed": {"x": 1, "y": 1},
                "rangedAttack": {"x": 2, "y": 4},
                "rangedHeal": {"x": 3, "y": 3}
            }
        }));
        let previous = value(json!({"x": 3, "y": 4, "type": "tower"}));
        let lowered = lower_supported_payload(
            &value(json!({"parentId": "mainContainer"})),
            &state,
            Some(&previous),
            4.0 / 15.0,
            &json!({"CELL_SIZE": 100, "ATTACK_PENETRATION": 10}),
        )
        .unwrap()
        .unwrap();
        let plan = decode_plan(&lowered).unwrap().unwrap();
        assert_eq!(plan.effects.len(), 5);
        assert!(matches!(
            &plan.effects[0],
            CapturedEffect::Shot(shot) if shot.color == ShotColor::Attack && shot.width == 18.0
        ));
        assert!(matches!(
            &plan.effects[1],
            CapturedEffect::Shot(shot) if shot.color == ShotColor::Heal && shot.width == 18.0
        ));
        assert!(matches!(plan.effects[2], CapturedEffect::Flash(0xff_ff_33)));
        assert!(matches!(
            &plan.effects[3],
            CapturedEffect::Shot(shot) if shot.color == ShotColor::Attack && shot.width == 12.0
        ));
        assert!(matches!(
            &plan.effects[4],
            CapturedEffect::Shot(shot) if shot.color == ShotColor::Heal && shot.width == 12.0
        ));
    }

    #[test]
    fn first_run_is_an_explicit_noop_and_unknown_live_branch_fails_closed() {
        let state = value(json!({"x": 1, "y": 2, "type": "creep", "actionLog": {}}));
        let first = lower_supported_payload(
            &value(json!({})),
            &state,
            None,
            0.25,
            &json!({"CELL_SIZE": 100, "ATTACK_PENETRATION": 10}),
        )
        .unwrap()
        .unwrap();
        assert!(decode_plan(&first).unwrap().unwrap().first_run);

        let unsupported = value(json!({
            "x": 1,
            "y": 2,
            "type": "creep",
            "actionLog": {"build": {"x": 3, "y": 4}}
        }));
        assert!(
            lower_supported_payload(
                &value(json!({})),
                &unsupported,
                Some(&state),
                0.25,
                &json!({"CELL_SIZE": 100, "ATTACK_PENETRATION": 10}),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn non_tower_stationary_actions_bite_but_movers_do_not() {
        let state = value(json!({
            "x": 4,
            "y": 5,
            "type": "creep",
            "actionLog": {"attack": {"x": 5, "y": 6}}
        }));
        let stationary = value(json!({"x": 4, "y": 5}));
        let moving = value(json!({"x": 3, "y": 5}));
        let options = json!({"CELL_SIZE": 100, "ATTACK_PENETRATION": 10});
        let stationary =
            lower_supported_payload(&value(json!({})), &state, Some(&stationary), 0.25, &options)
                .unwrap()
                .unwrap();
        assert!(matches!(
            decode_plan(&stationary).unwrap().unwrap().effects[..],
            [CapturedEffect::Bite(_)]
        ));
        let moving =
            lower_supported_payload(&value(json!({})), &state, Some(&moving), 0.25, &options)
                .unwrap()
                .unwrap();
        assert!(decode_plan(&moving).unwrap().unwrap().effects.is_empty());
    }

    #[test]
    fn user_supplied_marker_is_not_sufficient_without_internal_shape() {
        let forged = ResolvedValue::Object(BTreeMap::from([(
            super::ADAPTER_MARKER.to_owned(),
            ResolvedValue::Bool(true),
        )]));
        assert!(decode_plan(&forged).is_err());
    }

    #[test]
    fn shot_timer_grows_from_source_then_retracts_toward_target() {
        fn sampled(delta_seconds: f64) -> ActionTarget {
            let mut actions = ActionManagerRuntime::default();
            actions
                .create_target(
                    7,
                    SceneNodeKey {
                        entity_id: "shot".to_owned(),
                        node_id: "__root__".to_owned(),
                        is_root: true,
                    },
                    ActionTarget::default(),
                )
                .unwrap();
            let mut runtime = CreepActionsRuntime {
                active_persistent: BTreeMap::new(),
                live_shots: vec![LiveShot {
                    activation: 7,
                    source: [0.0, 0.0],
                    target: [100.0, 0.0],
                    duration_ms: 1_000.0,
                    rest_ms: 1_000.0,
                }],
            };
            assert!(!runtime.advance(delta_seconds, &mut actions).unwrap());
            actions.target(7).unwrap().clone()
        }

        let growing = sampled(0.1);
        assert!((growing.x - 20.0).abs() < 1.0e-9);
        assert!((growing.scale_x - 20.0).abs() < 1.0e-9);
        assert!((growing.rotation - std::f64::consts::PI).abs() < 1.0e-9);
        let retracting = sampled(0.75);
        assert!((retracting.x - 100.0).abs() < 1.0e-9);
        assert!((retracting.scale_x - 50.0).abs() < 1.0e-9);
    }

    #[test]
    fn template_compiler_allocates_four_persistent_drawables_and_shot_pair() {
        let first_payload = lower_supported_payload(
            &value(json!({})),
            &value(json!({"x": 1, "y": 2, "type": "tower", "actionLog": {}})),
            None,
            0.25,
            &json!({"CELL_SIZE": 100, "ATTACK_PENETRATION": 10}),
        )
        .unwrap()
        .unwrap();
        let run_payload = lower_supported_payload(
            &value(json!({})),
            &value(json!({
                "x": 1,
                "y": 2,
                "type": "tower",
                "actionLog": {"attack": {"x": 2, "y": 2}}
            })),
            Some(&value(json!({"x": 1, "y": 2, "type": "tower"}))),
            0.25,
            &json!({"CELL_SIZE": 100, "ATTACK_PENETRATION": 10}),
        )
        .unwrap()
        .unwrap();
        let processor = |activation_order, payload| ResolvedActivation::Processor {
            entity_id: "tower".to_owned(),
            object_type: "tower".to_owned(),
            definition_id: "creep-actions".to_owned(),
            scope_id: "creep-actions".to_owned(),
            kind: ProcessorKind::CreepActions,
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
        };
        let scene = ResolvedScene {
            activations: vec![processor(1, first_payload), processor(2, run_payload)],
            final_random_state: 0,
        };
        let entry = AtlasEntry {
            page: 0,
            x: 0,
            y: 0,
            width: 128,
            height: 128,
            logical_width: 128.0,
            logical_height: 128.0,
            u_min: 0.0,
            v_min: 0.0,
            u_max: 1.0,
            v_max: 1.0,
        };
        let atlas = TextureAtlas {
            entries: BTreeMap::from([
                ("cover".to_owned(), entry),
                ("flare2".to_owned(), entry),
                ("glow".to_owned(), entry),
            ]),
            pages: Vec::new(),
            padding: 0,
        };
        let (templates, nodes) = compile_templates(&scene, &atlas, 10).unwrap();
        assert_eq!(nodes.len(), 7); // container + four drawables + crisp/blurred shot
        let persistent = templates.runs[&2].persistent.unwrap();
        assert_eq!(
            [
                persistent.cover,
                persistent.flare,
                persistent.lighting,
                persistent.child_cover,
            ],
            [11, 12, 13, 14]
        );
        assert_eq!(templates.runs[&2].shots.len(), 1);
        assert_eq!(templates.runs[&2].shots[0].crisp, 15);
        assert_eq!(templates.runs[&2].shots[0].blurred, 16);
    }
}
