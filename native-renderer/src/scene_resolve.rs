use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ActionInterval, EntityValueRoots, Error, ObjectInterval, ProcessorInterval, RendererPlan,
    RendererRandom, ReplayArtifact, ResolvedActionNode, ResolvedValue, Result, SceneActivation,
    SceneSchedule, TrackValue,
};

#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedActivation {
    Object {
        entity_id: String,
        object_type: String,
        layer: Option<String>,
        z_index: f64,
        activation_order: u32,
        start_tick: u32,
        end_tick: u32,
        data: ResolvedValue,
    },
    Processor {
        entity_id: String,
        object_type: String,
        definition_id: String,
        scope_id: String,
        kind: crate::ProcessorKind,
        layer: Option<String>,
        z_index: f64,
        activation_order: u32,
        start_tick: u32,
        end_tick: u32,
        payload: ResolvedValue,
        object_texture: Option<ResolvedValue>,
        /// Global object-helper scope key touched by this activation. A generic
        /// helper deletes this key before its parent/shouldCreate checks, even
        /// when it returns no display object.
        node_id: Option<String>,
        /// Root-container identity is separate from all JavaScript scope keys,
        /// including the literal string `"__root__"`.
        target_is_root: bool,
        touches_node: bool,
        /// The processor returned a display object through a fresh local scope
        /// instead of publishing it under the entity's global scope.
        temporary_node: bool,
        actions: Vec<ResolvedActionNode>,
    },
    Action {
        entity_id: String,
        object_type: String,
        definition_id: String,
        scope_id: String,
        target_id: Option<String>,
        activation_order: u32,
        start_tick: u32,
        end_tick: u32,
        actions: Vec<ResolvedActionNode>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedScene {
    pub activations: Vec<ResolvedActivation>,
    pub final_random_state: u32,
}

impl ResolvedScene {
    /// Resolve every constructor-time value in retained renderer order.
    ///
    /// Animation integration remains a separate lowering stage, but no later
    /// stage may reevaluate these expressions or consume the renderer PRNG.
    pub fn compile(
        artifact: &ReplayArtifact,
        plan: &RendererPlan,
        schedule: &SceneSchedule,
    ) -> Result<Self> {
        let replay = &artifact.replay;
        let tick_duration = crate::Rational::parse_rate(
            replay
                .timeline
                .tick_transition_seconds
                .0
                .as_deref()
                .ok_or_else(|| {
                    Error::Invalid("ReplayIR timeline lacks tickTransitionSeconds".to_owned())
                })?,
            "tickTransitionSeconds",
        )?;
        let entities = replay
            .entities
            .iter()
            .map(|entity| (entity.id.as_str(), entity))
            .collect::<BTreeMap<_, _>>();
        let mut random = RendererRandom::from_replay_state(replay.random_state_at_first_tick.0)?;
        let mut scopes = ResolutionScopes::default();
        let mut resolved = Vec::with_capacity(
            schedule.objects.len() + schedule.processors.len() + schedule.actions.len(),
        );

        for activation in schedule.activations() {
            let value = match activation {
                SceneActivation::Object(interval) => {
                    let resolved =
                        resolve_object(interval, &entities, plan, tick_duration, &mut random)?;
                    scopes.create_object(&interval.entity_id);
                    resolved
                }
                SceneActivation::Processor(interval) => resolve_processor(
                    interval,
                    artifact,
                    &entities,
                    plan,
                    tick_duration,
                    &mut random,
                    &mut scopes,
                )?,
                SceneActivation::Action(interval) => {
                    resolve_action(interval, &entities, plan, tick_duration, &mut random)?
                }
            };
            resolved.push(value);
        }
        let object_lifetimes = resolved
            .iter()
            .filter_map(|activation| match activation {
                ResolvedActivation::Object {
                    entity_id,
                    start_tick,
                    end_tick,
                    ..
                } => Some((entity_id.clone(), *start_tick, *end_tick)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for activation in &mut resolved {
            let ResolvedActivation::Processor {
                entity_id,
                start_tick,
                end_tick,
                temporary_node: true,
                ..
            } = activation
            else {
                continue;
            };
            *end_tick = object_lifetimes
                .iter()
                .find(|(object_entity, object_start, object_end)| {
                    object_entity == entity_id
                        && *object_start <= *start_tick
                        && *start_tick < *object_end
                })
                .map(|(_, _, object_end)| *object_end)
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "temporary processor result for {entity_id} lacks an owning object"
                    ))
                })?;
        }
        Ok(Self {
            activations: resolved,
            final_random_state: random.state(),
        })
    }
}

#[derive(Default)]
struct ResolutionScopes {
    addressable: BTreeSet<(String, Option<String>)>,
    site_progress: BTreeMap<String, ResolvedValue>,
}

impl ResolutionScopes {
    fn create_object(&mut self, entity_id: &str) {
        self.addressable.retain(|(entity, _)| entity != entity_id);
        self.site_progress.retain(|entity, _| entity != entity_id);
        self.addressable.insert((entity_id.to_owned(), None));
    }

    fn resolve_site_progress(
        &mut self,
        entity_id: &str,
        processor: &crate::ProcessorPlan,
        payload: &ResolvedValue,
    ) -> Result<(ResolvedValue, GenericResult)> {
        let payload_object = payload.as_object().ok_or_else(|| {
            Error::Invalid("siteProgress payload must resolve to an object".to_owned())
        })?;
        let progress = payload_object
            .get("progress")
            .cloned()
            .unwrap_or(ResolvedValue::Undefined);
        if !self.site_progress_changed(entity_id, progress) {
            return Ok((
                ResolvedValue::Undefined,
                GenericResult {
                    node_id: None,
                    target_is_root: false,
                    touches_node: false,
                    creates_node: false,
                    temporary_node: false,
                },
            ));
        }
        Ok((payload.clone(), generic_result(processor, payload, None)?))
    }

    fn site_progress_changed(&mut self, entity_id: &str, progress: ResolvedValue) -> bool {
        let key = entity_id.to_owned();
        let previous = self
            .site_progress
            .get(&key)
            .unwrap_or(&ResolvedValue::Undefined);
        if resolved_strict_equal(previous, &progress) {
            return false;
        }
        self.site_progress.insert(key, progress);
        true
    }

    fn contains(&self, entity_id: &str, node_id: Option<&str>) -> bool {
        self.addressable
            .contains(&(entity_id.to_owned(), node_id.map(str::to_owned)))
    }

    fn resolve_processor_target(
        &mut self,
        entity_id: &str,
        kind: crate::ProcessorKind,
        payload: &ResolvedValue,
        result: Option<&GenericResult>,
    ) -> Result<bool> {
        let Some(result) = result else {
            return Ok(true);
        };
        if kind == crate::ProcessorKind::RunAction {
            return Ok(self.contains(
                entity_id,
                if result.target_is_root {
                    None
                } else {
                    result.node_id.as_deref()
                },
            ));
        }
        if result.temporary_node {
            if !result.creates_node {
                return Ok(false);
            }
            let payload = payload.as_object().ok_or_else(|| {
                Error::Invalid(format!(
                    "{} payload must resolve to an object",
                    kind.as_str()
                ))
            })?;
            let parent_id = payload
                .get("parentId")
                .filter(|value| crate::value_plan::resolved_js_truthy(value))
                .map(crate::value_plan::js_property_key)
                .transpose()?;
            return Ok(self.contains(entity_id, parent_id.as_deref()));
        }
        let Some(node_id) = result.node_id.as_deref() else {
            return Ok(false);
        };
        if result.touches_node {
            self.addressable
                .remove(&(entity_id.to_owned(), Some(node_id.to_owned())));
        }
        if !result.creates_node {
            return Ok(false);
        }
        let payload = payload.as_object().ok_or_else(|| {
            Error::Invalid(format!(
                "{} payload must resolve to an object",
                kind.as_str()
            ))
        })?;
        let parent_id = payload
            .get("parentId")
            .filter(|value| crate::value_plan::resolved_js_truthy(value))
            .map(crate::value_plan::js_property_key)
            .transpose()?;
        if !self.contains(entity_id, parent_id.as_deref()) {
            return Ok(false);
        }
        self.addressable
            .insert((entity_id.to_owned(), Some(node_id.to_owned())));
        Ok(true)
    }
}

fn resolve_object(
    interval: &ObjectInterval,
    entities: &BTreeMap<&str, &crate::Entity>,
    plan: &RendererPlan,
    tick_duration: crate::Rational,
    random: &mut RendererRandom,
) -> Result<ResolvedActivation> {
    let roots = roots(
        interval.entity_id.as_str(),
        interval.start_tick,
        entities,
        tick_duration,
    )?;
    let object = &plan.objects[&interval.object_type];
    let data = object
        .data
        .evaluate(&roots.context(None), &mut || random.next_f64())?;
    Ok(ResolvedActivation::Object {
        entity_id: interval.entity_id.clone(),
        object_type: interval.object_type.clone(),
        layer: object.layer.clone(),
        z_index: object.z_index,
        activation_order: interval.activation_order,
        start_tick: interval.start_tick,
        end_tick: interval.end_tick,
        data,
    })
}

fn resolve_processor(
    interval: &ProcessorInterval,
    artifact: &ReplayArtifact,
    entities: &BTreeMap<&str, &crate::Entity>,
    plan: &RendererPlan,
    tick_duration: crate::Rational,
    random: &mut RendererRandom,
    scopes: &mut ResolutionScopes,
) -> Result<ResolvedActivation> {
    let roots = roots(
        interval.entity_id.as_str(),
        interval.start_tick,
        entities,
        tick_duration,
    )?;
    let processor = plan.objects[&interval.object_type]
        .processors
        .iter()
        .find(|processor| processor.definition_id == interval.definition_id)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "scene interval references unknown processor {}",
                interval.definition_id
            ))
        })?;
    let roots = EntityValueRoots {
        state: processor.path.as_deref().map_or_else(
            || roots.state.clone(),
            |path| processor_state(&roots.state, path),
        ),
        calculations: roots.calculations,
        processor_parameters: roots.processor_parameters,
    };
    let payload = processor
        .payload
        .evaluate(&roots.context(None), &mut || random.next_f64())?;
    let payload = strip_native_adapter_markers(payload);
    let object_texture = if processor.kind == crate::ProcessorKind::Sprite
        && processor.uses_object_texture_fallback
    {
        plan.objects[&interval.object_type]
            .texture
            .as_ref()
            .map(|texture| texture.evaluate(&roots.context(None), &mut || random.next_f64()))
            .transpose()?
    } else {
        None
    };
    let (resolved_payload, generic_result) = match processor.kind {
        crate::ProcessorKind::Circle
        | crate::ProcessorKind::Container
        | crate::ProcessorKind::Draw
        | crate::ProcessorKind::Sprite => (
            payload.clone(),
            Some(generic_result(
                processor,
                &payload,
                object_texture.as_ref(),
            )?),
        ),
        crate::ProcessorKind::CreepBuildBody => {
            let payload = creep_build_body_payload(&payload, &roots.state)?;
            (
                payload,
                Some(GenericResult {
                    node_id: None,
                    target_is_root: false,
                    touches_node: false,
                    creates_node: true,
                    // The official processor returns undefined and publishes
                    // only a private `scope.bodySprites` array. Keep the
                    // collapsed native mesh out of the addressable scope.
                    temporary_node: true,
                }),
            )
        }
        crate::ProcessorKind::CreepActions => {
            let entity = entities[interval.entity_id.as_str()];
            let previous_state = interval
                .start_tick
                .checked_sub(1)
                .filter(|tick| entity.alive_at(*tick))
                .map(|tick| EntityValueRoots::at(entity, tick, tick_duration))
                .transpose()?
                .map(|roots| {
                    processor
                        .path
                        .as_deref()
                        .map_or(roots.state.clone(), |path| {
                            processor_state(&roots.state, path)
                        })
                });
            match crate::creep_actions::lower_supported_payload(
                &payload,
                &roots.state,
                previous_state.as_ref(),
                tick_duration.as_f64(),
                &artifact.renderer_contract.world_options,
            )? {
                Some(payload) => (
                    payload,
                    Some(GenericResult {
                        node_id: None,
                        target_is_root: false,
                        touches_node: false,
                        creates_node: false,
                        temporary_node: false,
                    }),
                ),
                None => (payload.clone(), None),
            }
        }
        crate::ProcessorKind::CreepDecoration | crate::ProcessorKind::ObjectDecoration
            if decoration_kind_is_absent(artifact, processor.kind) =>
        {
            let payload = ResolvedValue::Object(BTreeMap::from([(
                "$nativeDecorationNoop".to_owned(),
                ResolvedValue::Bool(true),
            )]));
            (
                payload,
                Some(GenericResult {
                    node_id: None,
                    target_is_root: false,
                    touches_node: false,
                    creates_node: false,
                    temporary_node: false,
                }),
            )
        }
        crate::ProcessorKind::Text => {
            let stage_zoom = artifact
                .replay
                .render_config
                .0
                .as_ref()
                .map(|config| config.board_frame.zoom)
                .unwrap_or(f64::NAN);
            match crate::text_raster::lower_supported_text_payload(&payload, stage_zoom)? {
                Some(payload) => {
                    let result = generic_result(processor, &payload, None)?;
                    (payload, Some(result))
                }
                None => (payload.clone(), None),
            }
        }
        crate::ProcessorKind::ResourceCircle => {
            let entity = entities[interval.entity_id.as_str()];
            let previous_state = interval
                .start_tick
                .checked_sub(1)
                .filter(|tick| entity.alive_at(*tick))
                .map(|tick| EntityValueRoots::at(entity, tick, tick_duration))
                .transpose()?
                .map(|roots| {
                    processor
                        .path
                        .as_deref()
                        .map_or(roots.state.clone(), |path| {
                            processor_state(&roots.state, path)
                        })
                });
            resource_circle_result(
                &processor.scope_id,
                &payload,
                &roots.state,
                previous_state.as_ref(),
            )?
        }
        crate::ProcessorKind::UserBadge => {
            let users = global_value(artifact, "users", interval.start_tick)?;
            let (payload, image_branch) = user_badge_payload(&payload, &roots.state, &users)?;
            let mut result = generic_result(processor, &payload, None)?;
            if image_branch {
                result.node_id = None;
                result.touches_node = false;
                result.temporary_node = true;
            }
            (payload, Some(result))
        }
        crate::ProcessorKind::SiteProgress => {
            let (payload, result) =
                scopes.resolve_site_progress(&interval.entity_id, processor, &payload)?;
            (payload, Some(result))
        }
        crate::ProcessorKind::RunAction => (payload.clone(), Some(run_action_result(&payload)?)),
        _ => (payload.clone(), None),
    };
    let relative = processor_relative(&resolved_payload);
    let context = roots.context(Some(&relative));
    let target_available = scopes.resolve_processor_target(
        &interval.entity_id,
        processor.kind,
        &resolved_payload,
        generic_result.as_ref(),
    )?;
    let actions = if target_available {
        processor
            .actions
            .iter()
            .map(|action| action.evaluate(&context, &mut || random.next_f64()))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok(ResolvedActivation::Processor {
        entity_id: interval.entity_id.clone(),
        object_type: interval.object_type.clone(),
        definition_id: interval.definition_id.clone(),
        scope_id: processor.scope_id.clone(),
        kind: processor.kind,
        layer: processor.layer.clone(),
        z_index: processor.z_index,
        activation_order: interval.activation_order,
        start_tick: interval.start_tick,
        end_tick: interval.end_tick,
        payload: resolved_payload,
        object_texture,
        node_id: generic_result
            .as_ref()
            .and_then(|result| result.node_id.clone()),
        target_is_root: generic_result
            .as_ref()
            .is_some_and(|result| result.target_is_root),
        touches_node: generic_result
            .as_ref()
            .is_some_and(|result| result.touches_node),
        temporary_node: generic_result
            .as_ref()
            .is_some_and(|result| result.temporary_node),
        actions,
    })
}

fn decoration_kind_is_absent(artifact: &ReplayArtifact, kind: crate::ProcessorKind) -> bool {
    let expected = match kind {
        crate::ProcessorKind::CreepDecoration => "creep",
        crate::ProcessorKind::ObjectDecoration => "object",
        _ => return false,
    };
    !artifact.renderer_contract.decorations.iter().any(|item| {
        item.get("decoration")
            .and_then(|decoration| decoration.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some(expected)
    })
}

fn strip_native_adapter_markers(payload: ResolvedValue) -> ResolvedValue {
    let ResolvedValue::Object(mut payload) = payload else {
        return payload;
    };
    payload.retain(|key, _| !key.starts_with("$native"));
    ResolvedValue::Object(payload)
}

fn creep_build_body_payload(
    payload: &ResolvedValue,
    state: &ResolvedValue,
) -> Result<ResolvedValue> {
    const ANGLE_SHIFT: f64 = -std::f64::consts::FRAC_PI_2;
    const PART_ANGLE: f64 = std::f64::consts::PI / 50.0;
    const ARC_RADIUS: f64 = 41.0;
    const LINE_WIDTH: f64 = 18.0;

    #[derive(Clone)]
    struct BodyGroup {
        kind: String,
        hits: f64,
    }

    let mut output = payload
        .as_object()
        .ok_or_else(|| {
            Error::Invalid("creepBuildBody payload must resolve to an object".to_owned())
        })?
        .clone();
    let state = state.as_object().ok_or_else(|| {
        Error::Invalid("creepBuildBody state must resolve to an object".to_owned())
    })?;
    let body = state
        .get("body")
        .ok_or_else(|| Error::Invalid("creepBuildBody state lacks body".to_owned()))?;
    let parts = match body {
        ResolvedValue::Array(parts) => parts.iter().collect::<Vec<_>>(),
        ResolvedValue::Object(parts) => parts.values().collect::<Vec<_>>(),
        _ => {
            return Err(Error::Invalid(
                "creepBuildBody state body must resolve to an array or object".to_owned(),
            ));
        }
    };

    let mut groups = Vec::<BodyGroup>::new();
    let mut has_tough = false;
    for part in parts {
        let part = part.as_object().ok_or_else(|| {
            Error::Invalid("creepBuildBody body part must resolve to an object".to_owned())
        })?;
        let kind = part
            .get("type")
            .and_then(ResolvedValue::as_string)
            .ok_or_else(|| {
                Error::Invalid("creepBuildBody body part type must be a string".to_owned())
            })?;
        let hits = part
            .get("hits")
            .and_then(ResolvedValue::as_number)
            .ok_or_else(|| {
                Error::Invalid("creepBuildBody body part hits must be a number".to_owned())
            })?;
        if !hits.is_finite() {
            return Err(Error::Invalid(
                "creepBuildBody body part hits must be finite".to_owned(),
            ));
        }
        if hits <= 0.0 {
            continue;
        }
        if kind == "tough" {
            has_tough = true;
            continue;
        }
        if kind == "carry" {
            continue;
        }
        if let Some(group) = groups.iter_mut().find(|group| group.kind == kind) {
            group.hits += hits;
        } else {
            groups.push(BodyGroup {
                kind: kind.to_owned(),
                hits,
            });
        }
    }
    // V8's stable Array#sort preserves the first-body-occurrence order for
    // equal aggregate hit counts.
    groups.sort_by(|left, right| left.hits.total_cmp(&right.hits));

    let mut drawings = Vec::with_capacity(groups.len() * 4);
    let mut front_angle = 0.0;
    let mut back_angle = std::f64::consts::PI;
    for group in groups {
        let (color, back_side) = match group.kind.as_str() {
            "move" => (0xaa_b7_c5, true),
            "work" => (0xfd_e5_74, false),
            "attack" => (0xf7_2e_41, false),
            "ranged_attack" => (0x7f_a7_e5, false),
            "heal" => (0x56_cf_5e, false),
            "claim" => (0xb9_9c_fb, false),
            // The reference renderer warns and emits nothing for unknown body
            // types after filtering TOUGH and CARRY.
            _ => continue,
        };
        let start_angle = if back_side { back_angle } else { front_angle };
        let angle = PART_ANGLE * (group.hits / 100.0);
        let effective_color = multiply_srgb8(color, color);
        drawings.push(draw_command(
            "lineStyle",
            vec![
                ResolvedValue::Number(LINE_WIDTH),
                ResolvedValue::Number(f64::from(effective_color)),
                ResolvedValue::Number(1.0),
            ],
        ));
        drawings.push(draw_command(
            "arc",
            vec![
                ResolvedValue::Number(0.0),
                ResolvedValue::Number(0.0),
                ResolvedValue::Number(ARC_RADIUS),
                ResolvedValue::Number(ANGLE_SHIFT + start_angle),
                ResolvedValue::Number(ANGLE_SHIFT + start_angle + angle),
                ResolvedValue::Bool(false),
            ],
        ));
        drawings.push(draw_command(
            "lineStyle",
            vec![
                ResolvedValue::Number(LINE_WIDTH),
                ResolvedValue::Number(f64::from(effective_color)),
                ResolvedValue::Number(1.0),
            ],
        ));
        drawings.push(draw_command(
            "arc",
            vec![
                ResolvedValue::Number(0.0),
                ResolvedValue::Number(0.0),
                ResolvedValue::Number(ARC_RADIUS),
                ResolvedValue::Number(ANGLE_SHIFT - start_angle),
                ResolvedValue::Number(ANGLE_SHIFT - start_angle - angle),
                ResolvedValue::Bool(true),
            ],
        ));
        if back_side {
            back_angle += angle;
        } else {
            front_angle += angle;
        }
    }
    output.insert("drawings".to_owned(), ResolvedValue::Array(drawings));
    output.insert(
        "$nativeCreepBodyHasTough".to_owned(),
        ResolvedValue::Bool(has_tough),
    );
    Ok(ResolvedValue::Object(output))
}

fn draw_command(method: &str, params: Vec<ResolvedValue>) -> ResolvedValue {
    ResolvedValue::Object(BTreeMap::from([
        (
            "method".to_owned(),
            ResolvedValue::String(method.to_owned()),
        ),
        ("params".to_owned(), ResolvedValue::Array(params)),
    ]))
}

fn multiply_srgb8(left: u32, right: u32) -> u32 {
    let channel = |shift: u32| {
        let left = (left >> shift) & 0xff;
        let right = (right >> shift) & 0xff;
        (left * right + 127) / 255
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn processor_state(state: &ResolvedValue, path: &str) -> ResolvedValue {
    if path.is_empty() {
        return state.get("").cloned().unwrap_or(ResolvedValue::Undefined);
    }
    crate::value_plan::resolve_path(state, path)
}

fn global_value(artifact: &ReplayArtifact, name: &str, tick: u32) -> Result<ResolvedValue> {
    match artifact
        .replay
        .global_state
        .get(name)
        .and_then(|track| track.at(tick))
    {
        None | Some(TrackValue::Absent | TrackValue::Undefined) => Ok(ResolvedValue::Undefined),
        Some(TrackValue::Value(value)) => ResolvedValue::from_json(value),
    }
}

fn user_badge_payload(
    payload: &ResolvedValue,
    state: &ResolvedValue,
    users: &ResolvedValue,
) -> Result<(ResolvedValue, bool)> {
    let payload = match payload {
        ResolvedValue::Undefined => BTreeMap::new(),
        ResolvedValue::Object(payload) => payload.clone(),
        _ => {
            return Err(Error::Invalid(
                "userBadge payload must resolve to an object".to_owned(),
            ));
        }
    };
    let state = state
        .as_object()
        .ok_or_else(|| Error::Invalid("userBadge state must resolve to an object".to_owned()))?;
    let parent_id = payload
        .get("parentId")
        .cloned()
        .unwrap_or(ResolvedValue::Undefined);
    let radius = payload
        .get("radius")
        .filter(|value| !matches!(value, ResolvedValue::Undefined))
        .cloned()
        .unwrap_or(ResolvedValue::Number(37.0));
    let color = payload
        .get("color")
        .filter(|value| !matches!(value, ResolvedValue::Undefined))
        .cloned()
        .unwrap_or(ResolvedValue::Number(0x22_22_22 as f64));
    let user = state.get("user").unwrap_or(&ResolvedValue::Undefined);
    let badge_url = if crate::value_plan::resolved_js_truthy(user) {
        let users = match users {
            ResolvedValue::Undefined => None,
            ResolvedValue::Object(users) => Some(users),
            _ => {
                return Err(Error::Invalid(
                    "userBadge global users value must be an object".to_owned(),
                ));
            }
        };
        if let Some(users) = users {
            let key = crate::value_plan::js_property_key(user)?;
            users
                .get(&key)
                .filter(|entry| crate::value_plan::resolved_js_truthy(entry))
                .and_then(|entry| entry.get("badgeUrl"))
                .filter(|url| crate::value_plan::resolved_js_truthy(url))
                .cloned()
        } else {
            None
        }
    } else {
        None
    };

    if let Some(badge_url) = badge_url {
        let mut derived = BTreeMap::from([
            (
                "width".to_owned(),
                ResolvedValue::Number(2.0 * resolved_js_number(&radius)),
            ),
            ("texture".to_owned(), badge_url),
            ("parentId".to_owned(), parent_id),
        ]);
        derived.extend(
            payload
                .into_iter()
                .filter(|(key, _)| !matches!(key.as_str(), "parentId" | "radius" | "color")),
        );
        Ok((ResolvedValue::Object(derived), true))
    } else {
        Ok((
            ResolvedValue::Object(BTreeMap::from([
                ("radius".to_owned(), radius),
                ("color".to_owned(), color),
                ("parentId".to_owned(), parent_id),
            ])),
            false,
        ))
    }
}

fn resource_circle_result(
    scope_id: &str,
    payload: &ResolvedValue,
    state: &ResolvedValue,
    previous_state: Option<&ResolvedValue>,
) -> Result<(ResolvedValue, Option<GenericResult>)> {
    let payload = payload.as_object().ok_or_else(|| {
        Error::Invalid("resourceCircle payload must resolve to an object".to_owned())
    })?;
    let state = state.as_object().ok_or_else(|| {
        Error::Invalid("resourceCircle state must resolve to an object".to_owned())
    })?;
    let previous_state = previous_state.and_then(ResolvedValue::as_object);
    let resource_type = match state.get("resourceType") {
        None | Some(ResolvedValue::Undefined) => "energy".to_owned(),
        Some(value) => crate::value_plan::js_property_key(value)?,
    };
    let new_resource = state
        .get(&resource_type)
        .unwrap_or(&ResolvedValue::Undefined);
    let old_resource = previous_state
        .and_then(|state| state.get(&resource_type))
        .unwrap_or(&ResolvedValue::Undefined);
    if resolved_strict_equal(old_resource, new_resource) {
        return Ok((
            ResolvedValue::Undefined,
            Some(GenericResult {
                node_id: None,
                target_is_root: false,
                touches_node: false,
                creates_node: false,
                temporary_node: false,
            }),
        ));
    }

    let resource_meta_name = match state.get("resourceType") {
        None | Some(ResolvedValue::Undefined) => "creepEnergy".to_owned(),
        Some(value) => crate::value_plan::js_property_key(value)?,
    };
    let (color, metadata_radius) = match resource_meta_name.as_str() {
        "creepEnergy" => (0xff_e5_6d, 20.0),
        "energy" => (0xff_e5_6d, 30.0),
        "power" => (0xf4_1f_33, 45.0),
        _ => (0xff_ff_ff, 45.0),
    };
    let payload_radius = payload
        .get("radius")
        .filter(|value| crate::value_plan::resolved_js_truthy(value))
        .map(resolved_js_number)
        .unwrap_or(metadata_radius);
    let capacity_name = format!("{resource_type}Capacity");
    let capacity = match state.get(&capacity_name) {
        None | Some(ResolvedValue::Undefined) => 1_250.0,
        Some(value) => resolved_js_number(value),
    };
    let resource = resolved_js_number(new_resource);
    let fill = if capacity == 0.0 {
        0.0
    } else {
        (resource / capacity).min(1.0)
    };
    let radius = payload_radius * fill;
    if !radius.is_finite() || radius < 0.0 {
        return Err(Error::Invalid(
            "resourceCircle produced an invalid radius".to_owned(),
        ));
    }
    Ok((
        ResolvedValue::Object(BTreeMap::from([
            ("color".to_owned(), ResolvedValue::Number(f64::from(color))),
            ("radius".to_owned(), ResolvedValue::Number(radius)),
        ])),
        Some(GenericResult {
            node_id: Some(scope_id.to_owned()),
            target_is_root: false,
            touches_node: true,
            creates_node: true,
            temporary_node: false,
        }),
    ))
}

fn resolved_js_number(value: &ResolvedValue) -> f64 {
    match value {
        ResolvedValue::Null => 0.0,
        ResolvedValue::Bool(value) => f64::from(u8::from(*value)),
        ResolvedValue::Number(value) => *value,
        ResolvedValue::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                0.0
            } else if value == "Infinity" || value == "+Infinity" {
                f64::INFINITY
            } else if value == "-Infinity" {
                f64::NEG_INFINITY
            } else if let Some(value) = value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
            {
                u64::from_str_radix(value, 16).map_or(f64::NAN, |value| value as f64)
            } else if let Some(value) = value
                .strip_prefix("0b")
                .or_else(|| value.strip_prefix("0B"))
            {
                u64::from_str_radix(value, 2).map_or(f64::NAN, |value| value as f64)
            } else if let Some(value) = value
                .strip_prefix("0o")
                .or_else(|| value.strip_prefix("0O"))
            {
                u64::from_str_radix(value, 8).map_or(f64::NAN, |value| value as f64)
            } else {
                value.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        ResolvedValue::Array(values) if values.is_empty() => 0.0,
        ResolvedValue::Array(values) if values.len() == 1 => resolved_js_number(&values[0]),
        ResolvedValue::Undefined
        | ResolvedValue::Array(_)
        | ResolvedValue::Object(_)
        | ResolvedValue::BigInt(_) => f64::NAN,
    }
}

fn resolved_strict_equal(left: &ResolvedValue, right: &ResolvedValue) -> bool {
    match (left, right) {
        (ResolvedValue::Undefined, ResolvedValue::Undefined)
        | (ResolvedValue::Null, ResolvedValue::Null) => true,
        (ResolvedValue::Bool(left), ResolvedValue::Bool(right)) => left == right,
        (ResolvedValue::Number(left), ResolvedValue::Number(right)) => left == right,
        (ResolvedValue::String(left), ResolvedValue::String(right)) => left == right,
        // Renderer state is reconstructed into fresh objects and arrays, so
        // JavaScript reference equality is false for composite values.
        _ => false,
    }
}

fn run_action_result(payload: &ResolvedValue) -> Result<GenericResult> {
    let payload = payload
        .as_object()
        .ok_or_else(|| Error::Invalid("runAction payload must resolve to an object".to_owned()))?;
    let (node_id, target_is_root) = match payload.get("id") {
        None | Some(ResolvedValue::Undefined) | Some(ResolvedValue::Null) => (None, true),
        Some(value) if !crate::value_plan::resolved_js_truthy(value) => (None, true),
        Some(value) => (Some(crate::value_plan::js_property_key(value)?), false),
    };
    Ok(GenericResult {
        node_id,
        target_is_root,
        touches_node: false,
        // The processor returns an existing target instead of creating one,
        // but its metadata actions still run when that target is available.
        creates_node: true,
        temporary_node: false,
    })
}

struct GenericResult {
    node_id: Option<String>,
    target_is_root: bool,
    touches_node: bool,
    creates_node: bool,
    temporary_node: bool,
}

fn generic_result(
    processor: &crate::ProcessorPlan,
    payload: &ResolvedValue,
    object_texture: Option<&ResolvedValue>,
) -> Result<GenericResult> {
    let payload = payload.as_object().ok_or_else(|| {
        Error::Invalid(format!(
            "{} payload must resolve to an object",
            processor.kind.as_str()
        ))
    })?;
    if processor.kind == crate::ProcessorKind::Sprite
        || (processor.kind == crate::ProcessorKind::UserBadge && payload.contains_key("texture"))
    {
        let texture = if processor.uses_object_texture_fallback {
            object_texture
        } else {
            payload.get("texture")
        };
        match texture {
            Some(ResolvedValue::String(texture)) if !texture.is_empty() => {}
            Some(value) if !crate::value_plan::resolved_js_truthy(value) => {
                return Ok(GenericResult {
                    node_id: None,
                    target_is_root: false,
                    touches_node: false,
                    creates_node: false,
                    temporary_node: false,
                });
            }
            None => {
                return Ok(GenericResult {
                    node_id: None,
                    target_is_root: false,
                    touches_node: false,
                    creates_node: false,
                    temporary_node: false,
                });
            }
            Some(_) => {}
        }
    }
    let node_id = match payload.get("id") {
        None | Some(ResolvedValue::Undefined) => processor.scope_id.clone(),
        Some(value) => crate::value_plan::js_property_key(value)?,
    };
    if !payload
        .get("shouldCreate")
        .is_none_or(crate::value_plan::resolved_js_truthy)
    {
        return Ok(GenericResult {
            node_id: Some(node_id),
            target_is_root: false,
            touches_node: true,
            creates_node: false,
            temporary_node: false,
        });
    }
    Ok(GenericResult {
        node_id: Some(node_id),
        target_is_root: false,
        touches_node: true,
        creates_node: true,
        temporary_node: false,
    })
}

fn resolve_action(
    interval: &ActionInterval,
    entities: &BTreeMap<&str, &crate::Entity>,
    plan: &RendererPlan,
    tick_duration: crate::Rational,
    random: &mut RendererRandom,
) -> Result<ResolvedActivation> {
    let roots = roots(
        interval.entity_id.as_str(),
        interval.start_tick,
        entities,
        tick_duration,
    )?;
    let group = plan.objects[&interval.object_type]
        .actions
        .iter()
        .find(|group| group.definition_id == interval.definition_id)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "scene interval references unknown action group {}",
                interval.definition_id
            ))
        })?;
    let context = roots.context(None);
    let actions = group
        .actions
        .iter()
        .map(|action| action.evaluate(&context, &mut || random.next_f64()))
        .collect::<Result<Vec<_>>>()?;
    Ok(ResolvedActivation::Action {
        entity_id: interval.entity_id.clone(),
        object_type: interval.object_type.clone(),
        definition_id: interval.definition_id.clone(),
        scope_id: group.scope_id.clone(),
        target_id: group.target_id.clone(),
        activation_order: interval.activation_order,
        start_tick: interval.start_tick,
        end_tick: interval.end_tick,
        actions,
    })
}

fn roots(
    entity_id: &str,
    tick: u32,
    entities: &BTreeMap<&str, &crate::Entity>,
    tick_duration: crate::Rational,
) -> Result<EntityValueRoots> {
    let entity = entities.get(entity_id).ok_or_else(|| {
        Error::Invalid(format!(
            "scene activation references unknown entity {entity_id}"
        ))
    })?;
    EntityValueRoots::at(entity, tick, tick_duration)
}

fn processor_relative(payload: &ResolvedValue) -> ResolvedValue {
    let mut properties = match payload {
        ResolvedValue::Object(values) => values.clone(),
        _ => BTreeMap::new(),
    };
    let scale = match properties.remove("scale") {
        Some(ResolvedValue::Object(mut values)) => {
            values
                .entry("x".to_owned())
                .or_insert(ResolvedValue::Number(1.0));
            values
                .entry("y".to_owned())
                .or_insert(ResolvedValue::Number(1.0));
            ResolvedValue::Object(values)
        }
        _ => ResolvedValue::Object(BTreeMap::from([
            ("x".to_owned(), ResolvedValue::Number(1.0)),
            ("y".to_owned(), ResolvedValue::Number(1.0)),
        ])),
    };
    properties.insert("scale".to_owned(), scale);
    ResolvedValue::Object(properties)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use image::ImageEncoder;
    use serde_json::{Value, json};

    use super::{
        GenericResult, ResolutionScopes, creep_build_body_payload, multiply_srgb8, processor_state,
        resource_circle_result, strip_native_adapter_markers, user_badge_payload,
    };

    #[test]
    fn site_progress_strict_cache_retains_equal_values_and_resets_per_object() {
        let mut scopes = ResolutionScopes::default();
        scopes.create_object("one");
        assert!(!scopes.site_progress_changed("one", ResolvedValue::Undefined));
        assert!(scopes.site_progress_changed("one", ResolvedValue::Number(1.0)));
        assert!(!scopes.site_progress_changed("one", ResolvedValue::Number(1.0)));
        assert!(!scopes.site_progress_changed("one", ResolvedValue::Number(1.0)));
        assert!(scopes.site_progress_changed("one", ResolvedValue::String("1".to_owned())));

        scopes.create_object("one");
        assert!(!scopes.site_progress_changed("one", ResolvedValue::Undefined));
    }
    use crate::artifact::tests::{artifact_json, signed};
    use crate::{
        AtlasOptions, BoardTransform, GenericSceneRuntime, RendererPlan, ReplayArtifact,
        ResolvedActionParameter, ResolvedActivation, ResolvedScene, ResolvedValue,
        SceneNodeTemplates, SceneSchedule, TextureAtlas, procedural_graphics_assets,
    };

    #[test]
    fn creep_build_body_lowers_stable_aggregates_and_tough_sprite_marker() {
        let part = |kind: &str, hits: f64| {
            ResolvedValue::Object(BTreeMap::from([
                ("hits".to_owned(), ResolvedValue::Number(hits)),
                ("type".to_owned(), ResolvedValue::String(kind.to_owned())),
            ]))
        };
        let payload = ResolvedValue::Object(BTreeMap::from([(
            "parentId".to_owned(),
            ResolvedValue::String("mainContainer".to_owned()),
        )]));
        let state = ResolvedValue::Object(BTreeMap::from([(
            "body".to_owned(),
            ResolvedValue::Array(vec![
                part("move", 100.0),
                part("attack", 50.0),
                part("work", 50.0),
                part("attack", 25.0),
                part("tough", 100.0),
                part("carry", 100.0),
                part("heal", 0.0),
            ]),
        )]));

        let lowered = creep_build_body_payload(&payload, &state).unwrap();
        assert_eq!(
            lowered.get("parentId"),
            Some(&ResolvedValue::String("mainContainer".to_owned()))
        );
        let program = crate::VectorProgram::from_draw_payload(&lowered).unwrap();
        assert_eq!(program.commands.len(), 12);
        assert!(matches!(
            program.commands[0],
            crate::VectorCommand::LineStyle(crate::VectorLineStyle { color, .. })
                if color == multiply_srgb8(0xfd_e5_74, 0xfd_e5_74)
        ));
        assert!(matches!(
            program.commands[4],
            crate::VectorCommand::LineStyle(crate::VectorLineStyle { color, .. })
                if color == multiply_srgb8(0xf7_2e_41, 0xf7_2e_41)
        ));
        assert!(matches!(
            program.commands[8],
            crate::VectorCommand::LineStyle(crate::VectorLineStyle { color, .. })
                if color == multiply_srgb8(0xaa_b7_c5, 0xaa_b7_c5)
        ));
        assert_eq!(
            lowered.get("$nativeCreepBodyHasTough"),
            Some(&ResolvedValue::Bool(true))
        );
    }

    #[test]
    fn user_payload_cannot_forge_native_adapter_markers() {
        let payload = ResolvedValue::Object(BTreeMap::from([
            (
                "$nativeDecorationNoop".to_owned(),
                ResolvedValue::Bool(true),
            ),
            ("$nativeTextRaster".to_owned(), ResolvedValue::Bool(true)),
            (
                "$nativeFutureAdapter".to_owned(),
                ResolvedValue::String("forged".to_owned()),
            ),
            ("text".to_owned(), ResolvedValue::String("A".to_owned())),
        ]));

        assert_eq!(
            strip_native_adapter_markers(payload),
            ResolvedValue::Object(BTreeMap::from([(
                "text".to_owned(),
                ResolvedValue::String("A".to_owned()),
            )]))
        );
    }

    #[test]
    fn resource_circle_lowers_official_defaults_and_retains_early_return() {
        let payload = ResolvedValue::Object(BTreeMap::from([(
            "radius".to_owned(),
            ResolvedValue::Number(50.0),
        )]));
        let state = ResolvedValue::Object(BTreeMap::from([
            ("energy".to_owned(), ResolvedValue::Number(625.0)),
            ("energyCapacity".to_owned(), ResolvedValue::Number(1_250.0)),
        ]));
        let (lowered, result) = resource_circle_result("resource", &payload, &state, None).unwrap();
        assert_eq!(
            lowered,
            ResolvedValue::Object(BTreeMap::from([
                ("color".to_owned(), ResolvedValue::Number(0xff_e5_6d as f64)),
                ("radius".to_owned(), ResolvedValue::Number(25.0)),
            ]))
        );
        let result = result.unwrap();
        assert_eq!(result.node_id.as_deref(), Some("resource"));
        assert!(result.touches_node);
        assert!(result.creates_node);

        let (lowered, result) =
            resource_circle_result("resource", &payload, &state, Some(&state)).unwrap();
        assert_eq!(lowered, ResolvedValue::Undefined);
        let result = result.unwrap();
        assert_eq!(result.node_id, None);
        assert!(!result.touches_node);
        assert!(!result.creates_node);
    }

    #[test]
    fn resource_circle_uses_power_metadata_and_zero_capacity_rule() {
        let payload = ResolvedValue::Object(BTreeMap::new());
        let state = ResolvedValue::Object(BTreeMap::from([
            (
                "resourceType".to_owned(),
                ResolvedValue::String("power".to_owned()),
            ),
            ("power".to_owned(), ResolvedValue::Number(100.0)),
            ("powerCapacity".to_owned(), ResolvedValue::Number(200.0)),
        ]));
        let (lowered, _) = resource_circle_result("resource", &payload, &state, None).unwrap();
        assert_eq!(
            lowered,
            ResolvedValue::Object(BTreeMap::from([
                ("color".to_owned(), ResolvedValue::Number(0xf4_1f_33 as f64)),
                ("radius".to_owned(), ResolvedValue::Number(22.5)),
            ]))
        );

        let state = ResolvedValue::Object(BTreeMap::from([
            (
                "resourceType".to_owned(),
                ResolvedValue::String("power".to_owned()),
            ),
            ("power".to_owned(), ResolvedValue::Number(100.0)),
            ("powerCapacity".to_owned(), ResolvedValue::Number(0.0)),
        ]));
        let (lowered, _) = resource_circle_result("resource", &payload, &state, None).unwrap();
        assert_eq!(lowered.get("radius"), Some(&ResolvedValue::Number(0.0)));

        let state = ResolvedValue::Object(BTreeMap::from([
            (
                "resourceType".to_owned(),
                ResolvedValue::String(String::new()),
            ),
            ("".to_owned(), ResolvedValue::String("100".to_owned())),
            (
                "Capacity".to_owned(),
                ResolvedValue::String("200".to_owned()),
            ),
        ]));
        let (lowered, _) = resource_circle_result("resource", &payload, &state, None).unwrap();
        assert_eq!(
            lowered,
            ResolvedValue::Object(BTreeMap::from([
                ("color".to_owned(), ResolvedValue::Number(0xff_ff_ff as f64)),
                ("radius".to_owned(), ResolvedValue::Number(22.5)),
            ]))
        );

        let state = ResolvedValue::Object(BTreeMap::from([
            (
                "resourceType".to_owned(),
                ResolvedValue::String("power".to_owned()),
            ),
            (
                "power".to_owned(),
                ResolvedValue::String("Infinity".to_owned()),
            ),
            ("powerCapacity".to_owned(), ResolvedValue::Number(200.0)),
        ]));
        let (lowered, _) = resource_circle_result("resource", &payload, &state, None).unwrap();
        assert_eq!(lowered.get("radius"), Some(&ResolvedValue::Number(45.0)));
    }

    #[test]
    fn empty_processor_path_resolves_the_empty_property() {
        let state = ResolvedValue::Object(BTreeMap::from([
            (
                String::new(),
                ResolvedValue::Object(BTreeMap::from([(
                    "energy".to_owned(),
                    ResolvedValue::Number(625.0),
                )])),
            ),
            ("energy".to_owned(), ResolvedValue::Number(1_250.0)),
        ]));

        assert_eq!(
            processor_state(&state, ""),
            ResolvedValue::Object(BTreeMap::from([(
                "energy".to_owned(),
                ResolvedValue::Number(625.0),
            )]))
        );
    }

    #[test]
    fn user_badge_selects_materialized_sprite_or_circle_fallback() {
        let payload = ResolvedValue::Object(BTreeMap::from([
            (
                "parentId".to_owned(),
                ResolvedValue::String("main".to_owned()),
            ),
            ("radius".to_owned(), ResolvedValue::Number(26.0)),
            ("color".to_owned(), ResolvedValue::Number(0x12_34_56 as f64)),
            ("width".to_owned(), ResolvedValue::Number(80.0)),
            ("height".to_owned(), ResolvedValue::Number(70.0)),
        ]));
        let state = ResolvedValue::Object(BTreeMap::from([(
            "user".to_owned(),
            ResolvedValue::String("one".to_owned()),
        )]));
        let badge_url = "data:image/png;base64,badge";
        let users = ResolvedValue::Object(BTreeMap::from([(
            "one".to_owned(),
            ResolvedValue::Object(BTreeMap::from([(
                "badgeUrl".to_owned(),
                ResolvedValue::String(badge_url.to_owned()),
            )])),
        )]));

        let (badge, image_branch) = user_badge_payload(&payload, &state, &users).unwrap();
        assert!(image_branch);
        assert_eq!(
            badge.get("texture"),
            Some(&ResolvedValue::String(badge_url.to_owned()))
        );
        assert_eq!(badge.get("width"), Some(&ResolvedValue::Number(80.0)));
        assert_eq!(badge.get("height"), Some(&ResolvedValue::Number(70.0)));
        assert_eq!(badge.get("radius"), None);
        assert_eq!(badge.get("color"), None);

        let (fallback, image_branch) =
            user_badge_payload(&payload, &state, &ResolvedValue::Undefined).unwrap();
        assert!(!image_branch);
        assert_eq!(
            fallback,
            ResolvedValue::Object(BTreeMap::from([
                ("radius".to_owned(), ResolvedValue::Number(26.0)),
                ("color".to_owned(), ResolvedValue::Number(0x12_34_56 as f64),),
                (
                    "parentId".to_owned(),
                    ResolvedValue::String("main".to_owned()),
                ),
            ]))
        );

        let (defaults, image_branch) = user_badge_payload(
            &ResolvedValue::Undefined,
            &ResolvedValue::Object(BTreeMap::new()),
            &ResolvedValue::Null,
        )
        .unwrap();
        assert!(!image_branch);
        assert_eq!(defaults.get("radius"), Some(&ResolvedValue::Number(37.0)));
        assert_eq!(
            defaults.get("color"),
            Some(&ResolvedValue::Number(0x22_22_22 as f64))
        );
    }

    #[test]
    fn resource_circle_resolves_the_processor_state_path() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "path": "store",
                "props": ["store"],
                "type": "resourceCircle"
            }]
        });
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["resourceCircle"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], [], []]);
        root["replay"]["entities"][0]["properties"]["store"] = json!([
            [0, 2],
            [{"energy": 625, "energyCapacity": 1250}],
            [],
            [],
            []
        ]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0], [3, 7], [-1, 0], [-1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 2],
            "payloads": [],
            "semanticIds": ["auto:$.objects.unit.processors[0]"]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        assert_eq!(
            plan.objects["unit"].processors[0].path.as_deref(),
            Some("store")
        );
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let ResolvedActivation::Processor { payload, .. } = &scene.activations[1] else {
            panic!("expected resource processor")
        };
        assert_eq!(payload.get("radius"), Some(&ResolvedValue::Number(10.0)));
    }

    #[test]
    fn user_badge_resolves_the_activation_tick_global_user_asset() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[200, 100, 50, 128], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let badge_url = format!("data:image/png;base64,{}", STANDARD.encode(png));
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "props": ["user"],
                "type": "userBadge",
                "payload": {"radius": 26}
            }]
        });
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["userBadge"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["globalState"]["users"] = json!([
            [0, 2],
            [{"one": {"badgeUrl": badge_url.clone()}}],
            [],
            [],
            []
        ]);
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], [], []]);
        root["replay"]["entities"][0]["properties"]["user"] = json!([[0, 2], ["one"], [], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0, 0], [3, 7, 7], [-1, 0, 0], [-1, -1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 3],
            "payloads": [],
            "semanticIds": ["auto:$.objects.unit.processors[0]"]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let ResolvedActivation::Processor {
            payload,
            node_id,
            touches_node,
            temporary_node,
            end_tick,
            ..
        } = &scene.activations[1]
        else {
            panic!("expected user badge processor")
        };
        assert_eq!(
            payload.get("texture"),
            Some(&ResolvedValue::String(badge_url.clone()))
        );
        assert_eq!(payload.get("width"), Some(&ResolvedValue::Number(52.0)));
        assert_eq!(node_id, &None);
        assert!(!touches_node);
        assert!(temporary_node);
        assert_eq!(*end_tick, 2);
        let ResolvedActivation::Processor {
            temporary_node,
            end_tick,
            ..
        } = &scene.activations[2]
        else {
            panic!("expected repeated user badge processor")
        };
        assert!(temporary_node);
        assert_eq!(*end_tick, 2);

        let assets = procedural_graphics_assets(&scene, AtlasOptions::default()).unwrap();
        let atlas = TextureAtlas::build_with_raster_assets(
            &artifact.renderer_contract,
            AtlasOptions::default(),
            assets,
        )
        .unwrap();
        let nodes = SceneNodeTemplates::compile(&scene, &atlas).unwrap();
        assert_eq!(nodes.nodes.len(), 3);
        assert_ne!(nodes.nodes[1].node_id, nodes.nodes[2].node_id);
        assert!(nodes.nodes[1].node_id.starts_with("$temporary.processor."));
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &nodes).unwrap();
        let board = BoardTransform::from(
            &artifact
                .replay
                .render_config
                .0
                .as_ref()
                .unwrap()
                .board_frame,
        );
        runtime.apply_tick(0).unwrap();
        assert_eq!(runtime.prepare(0, board).unwrap().len(), 1);
        runtime.apply_tick(1).unwrap();
        assert_eq!(runtime.prepare(1, board).unwrap().len(), 2);
    }

    #[test]
    fn root_container_identity_does_not_collide_with_a_scope_key_named_root() {
        let mut scopes = ResolutionScopes::default();
        scopes.create_object("one");
        let payload = ResolvedValue::Object(BTreeMap::from([(
            "id".to_owned(),
            ResolvedValue::String("__root__".to_owned()),
        )]));
        let generic = GenericResult {
            node_id: Some("__root__".to_owned()),
            target_is_root: false,
            touches_node: true,
            creates_node: true,
            temporary_node: false,
        };
        assert!(
            scopes
                .resolve_processor_target(
                    "one",
                    crate::ProcessorKind::Sprite,
                    &payload,
                    Some(&generic),
                )
                .unwrap()
        );
        let root_action = GenericResult {
            node_id: None,
            target_is_root: true,
            touches_node: false,
            creates_node: true,
            temporary_node: false,
        };
        let scope_action = GenericResult {
            node_id: Some("__root__".to_owned()),
            target_is_root: false,
            touches_node: false,
            creates_node: true,
            temporary_node: false,
        };
        let temporary = GenericResult {
            node_id: None,
            target_is_root: false,
            touches_node: false,
            creates_node: true,
            temporary_node: true,
        };
        assert!(
            !scopes
                .resolve_processor_target(
                    "one",
                    crate::ProcessorKind::UserBadge,
                    &ResolvedValue::Object(BTreeMap::from([(
                        "parentId".to_owned(),
                        ResolvedValue::String("missing".to_owned()),
                    )])),
                    Some(&temporary),
                )
                .unwrap()
        );
        assert!(
            scopes
                .resolve_processor_target(
                    "one",
                    crate::ProcessorKind::UserBadge,
                    &ResolvedValue::Object(BTreeMap::new()),
                    Some(&temporary),
                )
                .unwrap()
        );
        assert!(
            scopes
                .resolve_processor_target(
                    "one",
                    crate::ProcessorKind::RunAction,
                    &ResolvedValue::Object(BTreeMap::new()),
                    Some(&root_action),
                )
                .unwrap()
        );
        assert!(
            scopes
                .resolve_processor_target(
                    "one",
                    crate::ProcessorKind::RunAction,
                    &ResolvedValue::Object(BTreeMap::new()),
                    Some(&scope_action),
                )
                .unwrap()
        );
    }

    #[test]
    fn resolves_object_processor_and_nested_action_values_in_global_order() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {"phase": 3},
            "processors": [{
                "actions": [{
                    "action": "Sequence",
                    "params": [[
                        {"action": "DelayTime", "params": [{"$random": 4}]},
                        {"action": "ScaleTo", "params": [
                            {"$rel": "scale.x"},
                            {"$rel": "scale.y"},
                            1
                        ]}
                    ]]
                }],
                "payload": {"scale": {"x": 2}, "texture": "unit"},
                "type": "sprite"
            }]
        });
        root["rendererContract"]["inventory"]["actionTypes"] =
            json!(["DelayTime", "ScaleTo", "Sequence"]);
        root["rendererContract"]["inventory"]["expressionOperators"] = json!(["$random", "$rel"]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["randomStateAtFirstTick"] = json!(123);
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0], [3, 7], [-1, 0], [-1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 2],
            "payloads": [],
            "semanticIds": ["auto:$.objects.unit.processors[0]"]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let resolved = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        assert_eq!(resolved.activations.len(), 2);
        let ResolvedActivation::Object { data, .. } = &resolved.activations[0] else {
            panic!("expected object activation")
        };
        assert_eq!(data.get("phase"), Some(&ResolvedValue::Number(3.0)));
        let ResolvedActivation::Processor { actions, .. } = &resolved.activations[1] else {
            panic!("expected processor activation")
        };
        let ResolvedActionParameter::Array(nested) = &actions[0].params[0] else {
            panic!("expected nested action array")
        };
        let ResolvedActionParameter::Action(delay) = &nested[0] else {
            panic!("expected delay")
        };
        assert_eq!(
            delay.params[0],
            ResolvedActionParameter::Value(ResolvedValue::Number(3.1490064933896065))
        );
        let ResolvedActionParameter::Action(scale) = &nested[1] else {
            panic!("expected scale")
        };
        assert_eq!(
            scale.params[..2],
            [
                ResolvedActionParameter::Value(ResolvedValue::Number(2.0)),
                ResolvedActionParameter::Value(ResolvedValue::Number(1.0))
            ]
        );
        assert_eq!(resolved.final_random_state, 1_831_565_936);
    }

    #[test]
    fn skips_randomized_actions_when_a_generic_processor_creates_no_result() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "actions": [{
                    "action": "DelayTime",
                    "params": [{"$random": 4}]
                }],
                "payload": {
                    "parentId": "missing",
                    "shouldCreate": false,
                    "texture": "unit"
                },
                "type": "sprite"
            }]
        });
        root["rendererContract"]["inventory"]["actionTypes"] = json!(["DelayTime"]);
        root["rendererContract"]["inventory"]["expressionOperators"] = json!(["$random"]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["randomStateAtFirstTick"] = json!(123);
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0], [3, 7], [-1, 0], [-1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 2],
            "payloads": [],
            "semanticIds": ["auto:$.objects.unit.processors[0]"]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let resolved = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();

        let ResolvedActivation::Processor {
            actions,
            node_id,
            touches_node,
            ..
        } = &resolved.activations[1]
        else {
            panic!("expected processor activation")
        };
        assert!(actions.is_empty());
        assert_eq!(
            node_id.as_deref(),
            Some("auto:$.objects.unit.processors[0]")
        );
        assert!(*touches_node);
        assert_eq!(resolved.final_random_state, 123);
    }

    #[test]
    fn missing_run_action_target_does_not_consume_renderer_randomness() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "actions": [{
                    "action": "DelayTime",
                    "params": [{"$random": 4}]
                }],
                "id": "missing-action",
                "payload": {"id": "__root__"},
                "type": "runAction"
            }, {
                "actions": [{
                    "action": "DelayTime",
                    "params": [{"$random": 4}]
                }],
                "id": "later-sprite",
                "payload": {
                    "id": "later",
                    "texture": "unit"
                },
                "type": "sprite"
            }]
        });
        root["rendererContract"]["inventory"]["actionTypes"] = json!(["DelayTime"]);
        root["rendererContract"]["inventory"]["expressionOperators"] = json!(["$random"]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["runAction", "sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["randomStateAtFirstTick"] = json!(123);
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0, 0], [3, 7, 7], [-1, 0, 1], [-1, -1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 3, 3],
            "payloads": [],
            "semanticIds": [
                "auto:$.objects.unit.processors[0]",
                "auto:$.objects.unit.processors[1]"
            ]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let resolved = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let ResolvedActivation::Processor {
            actions: missing_actions,
            ..
        } = &resolved.activations[1]
        else {
            panic!("expected runAction activation")
        };
        assert!(missing_actions.is_empty());
        let ResolvedActivation::Processor { actions, .. } = &resolved.activations[2] else {
            panic!("expected sprite activation")
        };
        assert_eq!(
            actions[0].params[0],
            ResolvedActionParameter::Value(ResolvedValue::Number(3.1490064933896065))
        );
        assert_eq!(resolved.final_random_state, 1_831_565_936);
    }
}
