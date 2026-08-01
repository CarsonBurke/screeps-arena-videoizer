use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::{
    Error, ProcessorKind, RendererEventOpcode, RendererPlan, ReplayArtifact, Result, SlotBudget,
    TrackValue,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectInterval {
    pub entity_id: String,
    pub object_type: String,
    /// Global renderer-event index that created this activation.
    pub activation_order: u32,
    /// Inclusive replay tick.
    pub start_tick: u32,
    /// Exclusive replay tick.
    pub end_tick: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessorInterval {
    pub entity_id: String,
    pub object_type: String,
    pub definition_id: String,
    pub output_budget: SlotBudget,
    /// Global renderer-event index that created this activation.
    pub activation_order: u32,
    /// Inclusive replay tick.
    pub start_tick: u32,
    /// Exclusive replay tick.
    pub end_tick: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionInterval {
    pub entity_id: String,
    pub object_type: String,
    pub definition_id: String,
    /// Global renderer-event index that created this activation.
    pub activation_order: u32,
    /// Inclusive replay tick.
    pub start_tick: u32,
    /// Exclusive replay tick.
    pub end_tick: u32,
}

#[derive(Clone, Debug)]
pub struct SceneSchedule {
    pub objects: Vec<ObjectInterval>,
    pub processors: Vec<ProcessorInterval>,
    pub actions: Vec<ActionInterval>,
    /// Peak sum of fixed per-activation output budgets at one replay tick.
    /// Processors may emit fewer outputs; dynamic decoration output is counted
    /// separately and lowered later.
    pub max_concurrent_fixed_output_budget: u32,
    pub max_concurrent_dynamic_processor_budget: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum SceneActivation<'a> {
    Object(&'a ObjectInterval),
    Processor(&'a ProcessorInterval),
    Action(&'a ActionInterval),
}

struct ActiveObject {
    object_type: String,
    interval_index: usize,
}

impl SceneSchedule {
    pub fn compile(artifact: &ReplayArtifact, plan: &RendererPlan) -> Result<Self> {
        let replay = &artifact.replay;
        if !replay.renderer_graph.enabled {
            return Err(Error::Invalid(
                "native scene scheduling requires a compiled renderer graph".to_owned(),
            ));
        }

        let entities = replay
            .entities
            .iter()
            .map(|entity| (entity.id.as_str(), entity))
            .collect::<BTreeMap<_, _>>();
        let mut objects = Vec::new();
        let mut processors = Vec::new();
        let mut actions = Vec::new();
        let mut active_objects = BTreeMap::<String, ActiveObject>::new();
        let mut active_processors = BTreeMap::<(String, String), usize>::new();
        let mut active_actions = BTreeMap::<(String, String), usize>::new();
        let mut live_fixed_outputs = 0u32;
        let mut detached_user_badge_outputs = 0u32;
        let mut detached_user_badge_outputs_by_entity = BTreeMap::<String, u32>::new();
        let mut max_concurrent_fixed_output_budget = 0u32;
        let mut live_dynamic_processors = 0u32;
        let mut max_concurrent_dynamic_processor_budget = 0u32;

        for tick in 0..=replay.total_ticks {
            let start = replay.renderer_graph.offsets[tick as usize] as usize;
            let end = replay.renderer_graph.offsets[tick as usize + 1] as usize;
            for event_index in start..end {
                let activation_order =
                    u32::try_from(event_index).map_err(|_| Error::ArithmeticOverflow)?;
                if replay.renderer_graph.columns[3][event_index] >= 0 {
                    return Err(Error::Invalid(format!(
                        "native scene scheduling does not support renderer event payload at tick {tick}"
                    )));
                }
                let entity_id = optional_index(
                    &replay.renderer_graph.entity_ids,
                    replay.renderer_graph.columns[0][event_index],
                );
                let opcode =
                    RendererEventOpcode::try_from(replay.renderer_graph.columns[1][event_index])
                        .expect("artifact validation checked renderer opcode");
                let definition_id = optional_index(
                    &replay.renderer_graph.semantic_ids,
                    replay.renderer_graph.columns[2][event_index],
                );

                match opcode {
                    RendererEventOpcode::ObjectCreate => {
                        let entity_id = required(entity_id, "object:create entity")?;
                        if active_objects.contains_key(entity_id) {
                            return Err(Error::Invalid(format!(
                                "renderer graph creates active object {entity_id} at tick {tick}"
                            )));
                        }
                        let entity = entities.get(entity_id).ok_or_else(|| {
                            Error::Invalid(format!(
                                "renderer graph creates unknown object {entity_id}"
                            ))
                        })?;
                        if !entity.alive_at(tick) {
                            return Err(Error::Invalid(format!(
                                "renderer graph creates inactive object {entity_id} at tick {tick}"
                            )));
                        }
                        let object_type = entity_type(entity, tick)?;
                        if !plan.objects.contains_key(object_type) {
                            return Err(Error::Invalid(format!(
                                "renderer graph creates unsupported object type {object_type}"
                            )));
                        }
                        let interval_index = objects.len();
                        objects.push(ObjectInterval {
                            entity_id: entity_id.to_owned(),
                            object_type: object_type.to_owned(),
                            activation_order,
                            start_tick: tick,
                            end_tick: replay.total_ticks + 1,
                        });
                        active_objects.insert(
                            entity_id.to_owned(),
                            ActiveObject {
                                object_type: object_type.to_owned(),
                                interval_index,
                            },
                        );
                    }
                    RendererEventOpcode::ObjectRemove => {
                        let entity_id = required(entity_id, "object:remove entity")?;
                        close_object(
                            entity_id,
                            tick,
                            &mut active_objects,
                            &mut objects,
                            &mut active_processors,
                            &mut processors,
                            &mut active_actions,
                            &mut actions,
                            &mut live_fixed_outputs,
                            &mut live_dynamic_processors,
                        )?;
                        clear_detached_user_badge_outputs(
                            entity_id,
                            &mut detached_user_badge_outputs,
                            &mut detached_user_badge_outputs_by_entity,
                        )?;
                    }
                    RendererEventOpcode::ProcessorRun => {
                        let entity_id = required(entity_id, "processor:run entity")?;
                        let definition_id = required(definition_id, "processor:run definition")?;
                        let active = active_objects.get(entity_id).ok_or_else(|| {
                            Error::Invalid(format!(
                                "renderer graph runs processor for inactive object {entity_id} at tick {tick}"
                            ))
                        })?;
                        let object_plan = &plan.objects[&active.object_type];
                        let processor = object_plan
                            .processors
                            .iter()
                            .find(|processor| processor.definition_id == definition_id)
                            .ok_or_else(|| {
                                Error::Invalid(format!(
                                    "renderer graph references unknown processor {definition_id} for {}",
                                    active.object_type
                                ))
                            })?;
                        let output_budget = processor.output_budget;
                        let key = (entity_id.to_owned(), processor.scope_id.clone());
                        if let Some(previous_index) = active_processors.get(&key).copied() {
                            let previous = &processors[previous_index];
                            let previous_kind = object_plan
                                .processors
                                .iter()
                                .find(|candidate| candidate.definition_id == previous.definition_id)
                                .map(|candidate| candidate.kind)
                                .ok_or_else(|| {
                                    Error::Invalid(format!(
                                        "active processor {} is absent from the {} plan",
                                        previous.definition_id, active.object_type
                                    ))
                                })?;
                            if previous_kind == ProcessorKind::UserBadge {
                                detached_user_badge_outputs = detached_user_badge_outputs
                                    .checked_add(1)
                                    .ok_or(Error::ArithmeticOverflow)?;
                                let entity_detached = detached_user_badge_outputs_by_entity
                                    .entry(entity_id.to_owned())
                                    .or_default();
                                *entity_detached = entity_detached
                                    .checked_add(1)
                                    .ok_or(Error::ArithmeticOverflow)?;
                            }
                            close_processor(
                                &key,
                                tick,
                                &mut active_processors,
                                &mut processors,
                                &mut live_fixed_outputs,
                                &mut live_dynamic_processors,
                            )?;
                        }
                        match output_budget {
                            SlotBudget::Fixed(count) => {
                                live_fixed_outputs = live_fixed_outputs
                                    .checked_add(count)
                                    .ok_or(Error::ArithmeticOverflow)?;
                                let total_fixed_outputs = live_fixed_outputs
                                    .checked_add(detached_user_badge_outputs)
                                    .ok_or(Error::ArithmeticOverflow)?;
                                max_concurrent_fixed_output_budget =
                                    max_concurrent_fixed_output_budget.max(total_fixed_outputs);
                            }
                            SlotBudget::Dynamic => {
                                live_dynamic_processors = live_dynamic_processors
                                    .checked_add(1)
                                    .ok_or(Error::ArithmeticOverflow)?;
                                max_concurrent_dynamic_processor_budget =
                                    max_concurrent_dynamic_processor_budget
                                        .max(live_dynamic_processors);
                            }
                            SlotBudget::None => {}
                        }
                        let interval_index = processors.len();
                        processors.push(ProcessorInterval {
                            entity_id: entity_id.to_owned(),
                            object_type: active.object_type.clone(),
                            definition_id: definition_id.to_owned(),
                            output_budget,
                            activation_order,
                            start_tick: tick,
                            end_tick: replay.total_ticks + 1,
                        });
                        active_processors.insert(key, interval_index);
                    }
                    RendererEventOpcode::ProcessorDestruct => {
                        let entity_id = required(entity_id, "processor:destruct entity")?;
                        let definition_id =
                            required(definition_id, "processor:destruct definition")?;
                        let active = active_objects.get(entity_id).ok_or_else(|| {
                            Error::Invalid(format!(
                                "renderer graph destructs processor for inactive object {entity_id} at tick {tick}"
                            ))
                        })?;
                        let processor = plan.objects[&active.object_type]
                            .processors
                            .iter()
                            .find(|processor| processor.definition_id == definition_id)
                            .ok_or_else(|| {
                                Error::Invalid(format!(
                                    "renderer graph references unknown processor {definition_id} for {}",
                                    active.object_type
                                ))
                            })?;
                        close_processor(
                            &(entity_id.to_owned(), processor.scope_id.clone()),
                            tick,
                            &mut active_processors,
                            &mut processors,
                            &mut live_fixed_outputs,
                            &mut live_dynamic_processors,
                        )?;
                    }
                    RendererEventOpcode::ActionRun => {
                        let entity_id = required(entity_id, "action:run entity")?;
                        let definition_id = required(definition_id, "action:run definition")?;
                        let active = active_objects.get(entity_id).ok_or_else(|| {
                            Error::Invalid(format!(
                                "renderer graph runs action for inactive object {entity_id} at tick {tick}"
                            ))
                        })?;
                        validate_action_definition(
                            &plan.objects[&active.object_type],
                            definition_id,
                        )?;
                        let key = (entity_id.to_owned(), definition_id.to_owned());
                        if active_actions.contains_key(&key) {
                            return Err(Error::Invalid(format!(
                                "renderer graph reruns active action {definition_id} for {entity_id} without finishing it at tick {tick}"
                            )));
                        }
                        let interval_index = actions.len();
                        actions.push(ActionInterval {
                            entity_id: entity_id.to_owned(),
                            object_type: active.object_type.clone(),
                            definition_id: definition_id.to_owned(),
                            activation_order,
                            start_tick: tick,
                            end_tick: replay.total_ticks + 1,
                        });
                        active_actions.insert(key, interval_index);
                    }
                    RendererEventOpcode::ActionFinish => {
                        let entity_id = required(entity_id, "action:finish entity")?;
                        let definition_id = required(definition_id, "action:finish definition")?;
                        let active = active_objects.get(entity_id).ok_or_else(|| {
                            Error::Invalid(format!(
                                "renderer graph finishes action for inactive object {entity_id} at tick {tick}"
                            ))
                        })?;
                        validate_action_definition(
                            &plan.objects[&active.object_type],
                            definition_id,
                        )?;
                        let key = (entity_id.to_owned(), definition_id.to_owned());
                        let interval_index = active_actions.remove(&key).ok_or_else(|| {
                            Error::Invalid(format!(
                                "renderer graph finishes inactive action {definition_id} for {entity_id} at tick {tick}"
                            ))
                        })?;
                        actions[interval_index].end_tick = tick;
                    }
                    RendererEventOpcode::ObjectAlpha => {
                        let entity_id = required(entity_id, "object:alpha entity")?;
                        if !active_objects.contains_key(entity_id) {
                            return Err(Error::Invalid(format!(
                                "renderer graph changes alpha for inactive object {entity_id} at tick {tick}"
                            )));
                        }
                    }
                    RendererEventOpcode::PreprocessorRun => {
                        let preprocessor = required(definition_id, "preprocessor:run definition")?;
                        if !artifact
                            .renderer_contract
                            .inventory
                            .preprocessors
                            .iter()
                            .any(|value| value == preprocessor)
                        {
                            return Err(Error::Invalid(format!(
                                "renderer graph references unknown preprocessor {preprocessor}"
                            )));
                        }
                    }
                }
            }
        }

        let replay_end = replay.total_ticks + 1;
        for entity_id in active_objects.keys().cloned().collect::<Vec<_>>() {
            close_object(
                &entity_id,
                replay_end,
                &mut active_objects,
                &mut objects,
                &mut active_processors,
                &mut processors,
                &mut active_actions,
                &mut actions,
                &mut live_fixed_outputs,
                &mut live_dynamic_processors,
            )?;
            clear_detached_user_badge_outputs(
                &entity_id,
                &mut detached_user_badge_outputs,
                &mut detached_user_badge_outputs_by_entity,
            )?;
        }
        if live_fixed_outputs != 0
            || detached_user_badge_outputs != 0
            || !detached_user_badge_outputs_by_entity.is_empty()
            || live_dynamic_processors != 0
            || !active_processors.is_empty()
            || !active_actions.is_empty()
        {
            return Err(Error::Invalid(
                "native scene scheduler did not close every activation".to_owned(),
            ));
        }

        validate_object_lifetimes(&objects, &entities)?;
        Ok(Self {
            objects,
            processors,
            actions,
            max_concurrent_fixed_output_budget,
            max_concurrent_dynamic_processor_budget,
        })
    }

    /// Creation events in the exact order observed in the retained renderer.
    /// Destruction events affect interval endpoints but do not consume
    /// constructor expressions or renderer RNG.
    pub fn activations(&self) -> Vec<SceneActivation<'_>> {
        let mut activations = self
            .objects
            .iter()
            .map(SceneActivation::Object)
            .chain(self.processors.iter().map(SceneActivation::Processor))
            .chain(self.actions.iter().map(SceneActivation::Action))
            .collect::<Vec<_>>();
        activations.sort_unstable_by_key(SceneActivation::activation_order);
        activations
    }
}

impl SceneActivation<'_> {
    pub const fn activation_order(&self) -> u32 {
        match *self {
            Self::Object(interval) => interval.activation_order,
            Self::Processor(interval) => interval.activation_order,
            Self::Action(interval) => interval.activation_order,
        }
    }

    pub const fn start_tick(&self) -> u32 {
        match *self {
            Self::Object(interval) => interval.start_tick,
            Self::Processor(interval) => interval.start_tick,
            Self::Action(interval) => interval.start_tick,
        }
    }
}

fn clear_detached_user_badge_outputs(
    entity_id: &str,
    detached_outputs: &mut u32,
    detached_outputs_by_entity: &mut BTreeMap<String, u32>,
) -> Result<()> {
    let count = detached_outputs_by_entity.remove(entity_id).unwrap_or(0);
    *detached_outputs = detached_outputs
        .checked_sub(count)
        .ok_or(Error::ArithmeticOverflow)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn close_object(
    entity_id: &str,
    tick: u32,
    active_objects: &mut BTreeMap<String, ActiveObject>,
    objects: &mut [ObjectInterval],
    active_processors: &mut BTreeMap<(String, String), usize>,
    processors: &mut [ProcessorInterval],
    active_actions: &mut BTreeMap<(String, String), usize>,
    actions: &mut [ActionInterval],
    live_fixed_outputs: &mut u32,
    live_dynamic_processors: &mut u32,
) -> Result<()> {
    let active = active_objects.remove(entity_id).ok_or_else(|| {
        Error::Invalid(format!(
            "renderer graph removes inactive object {entity_id} at tick {tick}"
        ))
    })?;
    objects[active.interval_index].end_tick = tick;

    let processor_keys = active_processors
        .keys()
        .filter(|(active_entity, _)| active_entity == entity_id)
        .cloned()
        .collect::<Vec<_>>();
    for key in processor_keys {
        close_processor(
            &key,
            tick,
            active_processors,
            processors,
            live_fixed_outputs,
            live_dynamic_processors,
        )?;
    }
    let action_keys = active_actions
        .keys()
        .filter(|(active_entity, _)| active_entity == entity_id)
        .cloned()
        .collect::<Vec<_>>();
    for key in action_keys {
        let index = active_actions.remove(&key).expect("collected active key");
        actions[index].end_tick = tick;
    }
    Ok(())
}

fn close_processor(
    key: &(String, String),
    tick: u32,
    active_processors: &mut BTreeMap<(String, String), usize>,
    processors: &mut [ProcessorInterval],
    live_fixed_outputs: &mut u32,
    live_dynamic_processors: &mut u32,
) -> Result<()> {
    // The retained renderer can emit destruct for a processor that never
    // produced a live result. Match that official no-op.
    let Some(index) = active_processors.remove(key) else {
        return Ok(());
    };
    let interval = &mut processors[index];
    interval.end_tick = tick;
    match interval.output_budget {
        SlotBudget::Fixed(count) => {
            *live_fixed_outputs = live_fixed_outputs
                .checked_sub(count)
                .ok_or(Error::ArithmeticOverflow)?;
        }
        SlotBudget::Dynamic => {
            *live_dynamic_processors = live_dynamic_processors
                .checked_sub(1)
                .ok_or(Error::ArithmeticOverflow)?;
        }
        SlotBudget::None => {}
    }
    Ok(())
}

fn entity_type(entity: &crate::Entity, tick: u32) -> Result<&str> {
    match entity
        .properties
        .get("type")
        .and_then(|track| track.at(tick))
    {
        Some(TrackValue::Value(Value::String(value))) => Ok(value),
        _ => Err(Error::Invalid(format!(
            "active renderer object {} lacks a string type at tick {tick}",
            entity.id
        ))),
    }
}

fn validate_action_definition(object: &crate::ObjectPlan, definition_id: &str) -> Result<()> {
    let Some(index) = definition_id
        .strip_prefix(&format!("auto:$.objects.{}.actions[", object.object_type))
        .and_then(|suffix| suffix.strip_suffix(']'))
        .and_then(|index| index.parse::<usize>().ok())
    else {
        return Err(Error::Invalid(format!(
            "renderer graph references malformed action {definition_id} for {}",
            object.object_type
        )));
    };
    if index >= object.actions.len() {
        return Err(Error::Invalid(format!(
            "renderer graph references unknown action {definition_id} for {}",
            object.object_type
        )));
    }
    Ok(())
}

fn validate_object_lifetimes(
    objects: &[ObjectInterval],
    entities: &BTreeMap<&str, &crate::Entity>,
) -> Result<()> {
    let mut actual = BTreeMap::<&str, Vec<[u32; 2]>>::new();
    for interval in objects {
        actual
            .entry(&interval.entity_id)
            .or_default()
            .push([interval.start_tick, interval.end_tick]);
    }
    let known = entities.keys().copied().collect::<BTreeSet<_>>();
    if actual.keys().copied().collect::<BTreeSet<_>>() != known {
        return Err(Error::Invalid(
            "renderer object lifecycle does not cover every ReplayIR entity".to_owned(),
        ));
    }
    for (entity_id, entity) in entities {
        if actual.get(entity_id).map(Vec::as_slice) != Some(entity.lifetimes.as_slice()) {
            return Err(Error::Invalid(format!(
                "renderer object lifecycle does not match ReplayIR lifetimes for {entity_id}"
            )));
        }
    }
    Ok(())
}

fn optional_index(values: &[String], index: i32) -> Option<&str> {
    usize::try_from(index)
        .ok()
        .and_then(|index| values.get(index))
        .map(String::as_str)
}

fn required<'a>(value: Option<&'a str>, label: &str) -> Result<&'a str> {
    value.ok_or_else(|| Error::Invalid(format!("renderer graph lacks {label}")))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::artifact::tests::{artifact_json, signed};
    use crate::{RendererPlan, ReplayArtifact, SceneSchedule};

    #[test]
    fn compiles_exact_object_processor_and_action_intervals() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [{"actions": [], "id": "pulse"}],
            "calculations": [],
            "processors": [{"id": "body", "type": "sprite"}]
        });
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let contract_fingerprint = root["rendererContract"]["fingerprint"].clone();

        root["replay"]["rendererContractFingerprint"] = contract_fingerprint;
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [
                [0, 0, 0, 0, 0],
                [3, 7, 1, 0, 7],
                [-1, 1, 0, 0, 1],
                [-1, -1, -1, -1, -1]
            ],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 3, 5],
            "payloads": [],
            "semanticIds": [
                "auto:$.objects.unit.actions[0]",
                "auto:$.objects.unit.processors[0]"
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
        assert_eq!(schedule.objects[0].start_tick, 0);
        assert_eq!(schedule.objects[0].end_tick, 2);
        assert_eq!(schedule.objects[0].activation_order, 0);
        assert_eq!(schedule.processors[0].start_tick, 0);
        assert_eq!(schedule.processors[0].end_tick, 1);
        assert_eq!(schedule.processors[0].activation_order, 1);
        assert_eq!(schedule.processors[1].start_tick, 1);
        assert_eq!(schedule.processors[1].end_tick, 2);
        assert_eq!(schedule.processors[1].activation_order, 4);
        assert_eq!(schedule.actions[0].start_tick, 0);
        assert_eq!(schedule.actions[0].end_tick, 1);
        assert_eq!(schedule.actions[0].activation_order, 2);
        assert_eq!(
            schedule
                .activations()
                .iter()
                .map(|activation| activation.activation_order())
                .collect::<Vec<_>>(),
            [0, 1, 2, 4]
        );
        assert_eq!(schedule.max_concurrent_fixed_output_budget, 1);
        assert_eq!(schedule.max_concurrent_dynamic_processor_budget, 0);
    }

    #[test]
    fn rejects_lifecycle_that_disagrees_with_entity_lifetimes() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0], [3, 4], [-1, -1], [-1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 1, 2],
            "payloads": [],
            "semanticIds": []
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        assert!(SceneSchedule::compile(&artifact, &plan).is_err());
    }

    #[test]
    fn shared_scope_replaces_the_previous_definition_interval() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"]["processors"] = json!([
            {"id": "flare", "once": true, "type": "sprite"},
            {"id": "flare", "once": true, "type": "sprite"}
        ]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let contract_fingerprint = root["rendererContract"]["fingerprint"].clone();

        root["replay"]["rendererContractFingerprint"] = contract_fingerprint;
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0, 0], [3, 7, 7], [-1, 1, 0], [-1, -1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 3],
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
        assert_eq!(schedule.processors.len(), 2);
        assert_eq!(
            schedule.processors[0].definition_id,
            plan.objects["unit"].processors[1].definition_id
        );
        assert_eq!(schedule.processors[0].start_tick, 0);
        assert_eq!(schedule.processors[0].end_tick, 1);
        assert_eq!(
            schedule.processors[1].definition_id,
            plan.objects["unit"].processors[0].definition_id
        );
        assert_eq!(schedule.processors[1].start_tick, 1);
        assert_eq!(schedule.processors[1].end_tick, 2);
        assert_eq!(schedule.max_concurrent_fixed_output_budget, 1);
    }

    #[test]
    fn repeated_user_badges_reserve_detached_temporary_output_capacity() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"]["processors"] = json!([
            {"id": "badge", "type": "userBadge"},
            {"id": "badge", "type": "userBadge"}
        ]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["userBadge"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let contract_fingerprint = root["rendererContract"]["fingerprint"].clone();

        root["replay"]["rendererContractFingerprint"] = contract_fingerprint;
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0, 0], [3, 7, 7], [-1, 1, 0], [-1, -1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 3],
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

        assert_eq!(schedule.processors.len(), 2);
        assert_eq!(schedule.max_concurrent_fixed_output_budget, 2);
    }
}
