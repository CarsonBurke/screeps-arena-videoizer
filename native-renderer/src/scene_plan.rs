use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::action_plan::{compile_action_group, compile_action_nodes};
use crate::{ActionGroupPlan, ActionNode, CompiledValue, Error, RendererContract, Result};

pub const RETAINED_PROCESSOR_TYPES: [&str; 17] = [
    "circle",
    "container",
    "creepActions",
    "creepBuildBody",
    "creepDecoration",
    "disappear",
    "draw",
    "objectDecoration",
    "powerInfluence",
    "resourceCircle",
    "road",
    "runAction",
    "say",
    "siteProgress",
    "sprite",
    "text",
    "userBadge",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProcessorKind {
    Circle,
    Container,
    CreepActions,
    CreepBuildBody,
    CreepDecoration,
    Disappear,
    Draw,
    ObjectDecoration,
    PowerInfluence,
    ResourceCircle,
    Road,
    RunAction,
    Say,
    SiteProgress,
    Sprite,
    Text,
    UserBadge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotBudget {
    None,
    Fixed(u32),
    Dynamic,
}

#[derive(Clone, Debug)]
pub struct ProcessorPlan {
    /// Stable renderer-event key for this exact metadata definition.
    pub definition_id: String,
    /// Official processor/node scope key. Unlike `definition_id`, this can be
    /// shared deliberately by multiple definitions.
    pub scope_id: String,
    pub kind: ProcessorKind,
    /// Optional renderer state subtree passed as `state`/`prevState`.
    pub path: Option<String>,
    pub layer: Option<String>,
    pub z_index: f64,
    pub once: bool,
    pub payload: CompiledValue,
    /// JavaScript destructuring uses the object texture only when the raw
    /// payload field is absent or literal undefined, before expression parsing.
    pub uses_object_texture_fallback: bool,
    pub actions: Vec<ActionNode>,
    /// Draw-node outputs from one processor activation. Repeating transient
    /// activations are interval-allocated later and are not implied to share a
    /// persistent slot.
    pub output_budget: SlotBudget,
}

#[derive(Clone, Debug)]
pub struct ObjectPlan {
    pub object_type: String,
    /// Properties assigned to the root container on the first state update.
    pub data: CompiledValue,
    pub texture: Option<CompiledValue>,
    pub layer: Option<String>,
    pub z_index: f64,
    pub processors: Vec<ProcessorPlan>,
    pub actions: Vec<ActionGroupPlan>,
    pub disappear_processor: Option<ProcessorKind>,
    pub fixed_template_slots: u32,
    pub has_dynamic_outputs: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RendererLayerPlan {
    pub id: String,
    pub order: u32,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct RendererPlan {
    /// Pixi stage layer order exactly as declared by renderer metadata.
    pub layers: Vec<RendererLayerPlan>,
    pub layer_orders: BTreeMap<String, u32>,
    pub default_layer_id: Option<String>,
    pub objects: BTreeMap<String, ObjectPlan>,
    pub processor_definitions: usize,
    pub action_definitions: usize,
    pub max_fixed_template_slots: u32,
    pub has_dynamic_outputs: bool,
}

#[derive(Default)]
struct RecomputedInventory {
    object_types: BTreeSet<String>,
    processor_types: BTreeSet<String>,
    action_types: BTreeSet<String>,
    preprocessors: BTreeSet<String>,
    calculation_ids: BTreeSet<String>,
    drawing_methods: BTreeSet<String>,
    expression_operators: BTreeSet<String>,
    function_semantics: BTreeSet<String>,
    layer_ids: BTreeSet<String>,
}

impl RendererPlan {
    pub fn compile(contract: &RendererContract) -> Result<Self> {
        validate_metadata_inventory(contract)?;
        let metadata = object(&contract.metadata, "renderer metadata")?;
        let objects = object(
            metadata
                .get("objects")
                .ok_or_else(|| Error::Invalid("renderer metadata lacks objects".to_owned()))?,
            "renderer metadata objects",
        )?;
        let mut layers = Vec::new();
        let mut layer_orders = BTreeMap::new();
        let mut default_layer_id = None;
        for (index, value) in optional_array(metadata.get("layers"), "renderer layers")?
            .iter()
            .enumerate()
        {
            let layer = object(value, &format!("renderer layer {index}"))?;
            let id = string(
                layer
                    .get("id")
                    .ok_or_else(|| Error::Invalid(format!("renderer layer {index} lacks an ID")))?,
                &format!("renderer layer {index} ID"),
            )?
            .to_owned();
            let order = u32::try_from(index).map_err(|_| Error::ArithmeticOverflow)?;
            if layer_orders.insert(id.clone(), order).is_some() {
                return Err(Error::Invalid(format!(
                    "renderer metadata repeats layer ID {id}"
                )));
            }
            let is_default = layer.get("isDefault").is_some_and(js_truthy);
            if is_default && default_layer_id.replace(id.clone()).is_some() {
                return Err(Error::Invalid(
                    "renderer metadata declares multiple default layers".to_owned(),
                ));
            }
            layers.push(RendererLayerPlan {
                id,
                order,
                is_default,
            });
        }

        let mut plans = BTreeMap::new();
        let mut processor_definitions = 0usize;
        let mut action_definitions = 0usize;
        let mut max_fixed_template_slots = 0u32;
        let mut has_dynamic_outputs = false;
        for (object_type, value) in objects {
            let definition = object(value, &format!("renderer object {object_type}"))?;
            let mut definition_ids = BTreeSet::new();
            let mut object_fixed_slots = 0u32;
            let mut object_has_dynamic_slots = false;
            let processors = optional_array(definition.get("processors"), "object processors")?
                .iter()
                .enumerate()
                .map(|(index, processor)| {
                    let processor = object(
                        processor,
                        &format!("renderer object {object_type} processor {index}"),
                    )?;
                    let kind = processor_kind(processor)?;
                    let definition_id = definition_id(object_type, index);
                    if !definition_ids.insert(definition_id.clone()) {
                        return Err(Error::Invalid(format!(
                            "renderer object {object_type} repeats processor definition ID {definition_id}"
                        )));
                    }
                    let scope_id = semantic_id(processor, object_type, index);
                    let path = optional_string(
                        processor.get("path"),
                        &format!("renderer object {object_type} processor {index} path"),
                    )?;
                    let layer = optional_string(processor.get("layer"), "processor layer")?;
                    validate_layer_reference(
                        layer.as_deref(),
                        &layer_orders,
                        &format!("renderer object {object_type} processor {index}"),
                    )?;
                    let z_index = optional_number(processor.get("zIndex"), 0.0, "processor zIndex")?;
                    let once = processor.get("once").is_some_and(js_truthy);
                    let empty_payload = Value::Object(Map::new());
                    let payload = CompiledValue::compile(
                        processor.get("payload").unwrap_or(&empty_payload),
                        &format!("renderer object {object_type} processor {index} payload"),
                    )?;
                    reject_eager_random(
                        &payload,
                        &format!("renderer object {object_type} processor {index} payload"),
                    )?;
                    let uses_object_texture_fallback = matches!(
                        payload.object_field("texture"),
                        None | Some(CompiledValue::Undefined)
                    );
                    let actions = compile_action_nodes(
                        optional_array(processor.get("actions"), "processor actions")?,
                        &format!("renderer object {object_type} processor {index} actions"),
                    )?;
                    if actions.iter().any(|action| {
                        action.contains_operator(crate::ExpressionOperator::Random)
                    }) && !matches!(
                        kind,
                        ProcessorKind::Container
                            | ProcessorKind::Sprite
                            | ProcessorKind::RunAction
                    )
                    {
                        return Err(Error::Invalid(format!(
                            "renderer object {object_type} processor {index} has randomized actions and requires result-aware native lowering"
                        )));
                    }
                    action_definitions = action_definitions
                        .checked_add(count_action_nodes(&actions))
                        .ok_or(Error::ArithmeticOverflow)?;
                    let output_budget = kind.slot_budget();
                    match output_budget {
                        SlotBudget::Fixed(count) => {
                            object_fixed_slots = object_fixed_slots
                                .checked_add(count)
                                .ok_or(Error::ArithmeticOverflow)?;
                        }
                        SlotBudget::Dynamic => object_has_dynamic_slots = true,
                        SlotBudget::None => {}
                    }
                    processor_definitions += 1;
                    Ok(ProcessorPlan {
                        definition_id,
                        scope_id,
                        kind,
                        path,
                        layer,
                        z_index,
                        once,
                        payload,
                        uses_object_texture_fallback,
                        actions,
                        output_budget,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let disappear_processor = definition
                .get("disappearProcessor")
                .map(|value| processor_kind(object(value, "disappear processor")?))
                .transpose()?;
            let empty_data = Value::Object(Map::new());
            let data = CompiledValue::compile(
                definition.get("data").unwrap_or(&empty_data),
                &format!("renderer object {object_type} data"),
            )?;
            reject_eager_random(&data, &format!("renderer object {object_type} data"))?;
            let texture = definition
                .get("texture")
                .map(|value| {
                    CompiledValue::compile(value, &format!("renderer object {object_type} texture"))
                })
                .transpose()?;
            if let Some(texture) = &texture {
                reject_eager_random(texture, &format!("renderer object {object_type} texture"))?;
            }
            let z_index = optional_number(definition.get("zIndex"), 0.0, "object zIndex")?;
            let layer = optional_string(definition.get("layer"), "object layer")?;
            validate_layer_reference(
                layer.as_deref(),
                &layer_orders,
                &format!("renderer object {object_type}"),
            )?;
            if !layer_orders.is_empty() && layer.is_none() && default_layer_id.is_none() {
                return Err(Error::Invalid(format!(
                    "renderer object {object_type} has no layer and renderer metadata has no default layer"
                )));
            }
            let actions = optional_array(definition.get("actions"), "object actions")?
                .iter()
                .enumerate()
                .map(|(index, value)| compile_action_group(value, object_type, index))
                .collect::<Result<Vec<_>>>()?;
            action_definitions = action_definitions
                .checked_add(
                    actions
                        .iter()
                        .map(|group| count_action_nodes(&group.actions))
                        .sum::<usize>(),
                )
                .ok_or(Error::ArithmeticOverflow)?;
            max_fixed_template_slots = max_fixed_template_slots.max(object_fixed_slots);
            has_dynamic_outputs |= object_has_dynamic_slots;
            plans.insert(
                object_type.clone(),
                ObjectPlan {
                    object_type: object_type.clone(),
                    data,
                    texture,
                    layer,
                    z_index,
                    processors,
                    actions,
                    disappear_processor,
                    fixed_template_slots: object_fixed_slots,
                    has_dynamic_outputs: object_has_dynamic_slots,
                },
            );
        }
        Ok(Self {
            layers,
            layer_orders,
            default_layer_id,
            objects: plans,
            processor_definitions,
            action_definitions,
            max_fixed_template_slots,
            has_dynamic_outputs,
        })
    }
}

fn reject_eager_random(value: &CompiledValue, label: &str) -> Result<()> {
    if value.contains_operator(crate::ExpressionOperator::Random) {
        return Err(Error::Invalid(format!(
            "{label} contains $random and requires processor-specific lazy evaluation"
        )));
    }
    Ok(())
}

fn validate_layer_reference(
    layer: Option<&str>,
    layer_orders: &BTreeMap<String, u32>,
    label: &str,
) -> Result<()> {
    if let Some(layer) = layer
        && !layer_orders.contains_key(layer)
    {
        return Err(Error::Invalid(format!(
            "{label} references unknown renderer layer {layer}"
        )));
    }
    Ok(())
}

fn count_action_nodes(actions: &[ActionNode]) -> usize {
    actions
        .iter()
        .map(|action| 1 + action.nested_action_count())
        .sum()
}

impl ProcessorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Circle => "circle",
            Self::Container => "container",
            Self::CreepActions => "creepActions",
            Self::CreepBuildBody => "creepBuildBody",
            Self::CreepDecoration => "creepDecoration",
            Self::Disappear => "disappear",
            Self::Draw => "draw",
            Self::ObjectDecoration => "objectDecoration",
            Self::PowerInfluence => "powerInfluence",
            Self::ResourceCircle => "resourceCircle",
            Self::Road => "road",
            Self::RunAction => "runAction",
            Self::Say => "say",
            Self::SiteProgress => "siteProgress",
            Self::Sprite => "sprite",
            Self::Text => "text",
            Self::UserBadge => "userBadge",
        }
    }

    pub const fn slot_budget(self) -> SlotBudget {
        match self {
            Self::Container | Self::Disappear | Self::RunAction => SlotBudget::None,
            Self::Circle
            | Self::Draw
            | Self::PowerInfluence
            | Self::ResourceCircle
            | Self::Road
            | Self::SiteProgress
            | Self::Sprite
            | Self::Text
            | Self::UserBadge => SlotBudget::Fixed(1),
            // One bubble mesh and one glyph-run texture.
            Self::Say => SlotBudget::Fixed(2),
            // Cover, flare, lighting cover, child cover, and bounded combat
            // line/effect primitives. Exact activation is event-driven.
            Self::CreepActions => SlotBudget::Fixed(36),
            // Two arcs for each of the six non-tough visible body types, plus
            // one tough ring.
            Self::CreepBuildBody => SlotBudget::Fixed(13),
            Self::CreepDecoration | Self::ObjectDecoration => SlotBudget::Dynamic,
        }
    }
}

impl TryFrom<&str> for ProcessorKind {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "circle" => Ok(Self::Circle),
            "container" => Ok(Self::Container),
            "creepActions" => Ok(Self::CreepActions),
            "creepBuildBody" => Ok(Self::CreepBuildBody),
            "creepDecoration" => Ok(Self::CreepDecoration),
            "disappear" => Ok(Self::Disappear),
            "draw" => Ok(Self::Draw),
            "objectDecoration" => Ok(Self::ObjectDecoration),
            "powerInfluence" => Ok(Self::PowerInfluence),
            "resourceCircle" => Ok(Self::ResourceCircle),
            "road" => Ok(Self::Road),
            "runAction" => Ok(Self::RunAction),
            "say" => Ok(Self::Say),
            "siteProgress" => Ok(Self::SiteProgress),
            "sprite" => Ok(Self::Sprite),
            "text" => Ok(Self::Text),
            "userBadge" => Ok(Self::UserBadge),
            other => Err(Error::Invalid(format!(
                "native scene plan does not implement renderer processor {other}"
            ))),
        }
    }
}

pub(crate) fn validate_metadata_inventory(contract: &RendererContract) -> Result<()> {
    let actual = recompute_inventory(&contract.metadata)?;
    compare_inventory(
        "objectTypes",
        &actual.object_types,
        &contract.inventory.object_types,
    )?;
    compare_inventory(
        "processorTypes",
        &actual.processor_types,
        &contract.inventory.processor_types,
    )?;
    compare_inventory(
        "actionTypes",
        &actual.action_types,
        &contract.inventory.action_types,
    )?;
    compare_inventory(
        "preprocessors",
        &actual.preprocessors,
        &contract.inventory.preprocessors,
    )?;
    compare_inventory(
        "calculationIds",
        &actual.calculation_ids,
        &contract.inventory.calculation_ids,
    )?;
    compare_inventory(
        "drawingMethods",
        &actual.drawing_methods,
        &contract.inventory.drawing_methods,
    )?;
    compare_inventory(
        "expressionOperators",
        &actual.expression_operators,
        &contract.inventory.expression_operators,
    )?;
    compare_inventory("layerIds", &actual.layer_ids, &contract.inventory.layer_ids)?;
    let declared_functions = contract
        .inventory
        .function_semantics
        .iter()
        .filter(|value| !value.starts_with("objectFilter:"))
        .cloned()
        .collect::<Vec<_>>();
    compare_inventory(
        "functionSemantics",
        &actual.function_semantics,
        &declared_functions,
    )
}

fn recompute_inventory(metadata: &Value) -> Result<RecomputedInventory> {
    let root = object(metadata, "renderer metadata")?;
    let objects = object(
        root.get("objects")
            .ok_or_else(|| Error::Invalid("renderer metadata lacks objects".to_owned()))?,
        "renderer metadata objects",
    )?;
    let mut inventory = RecomputedInventory::default();
    inventory.object_types.extend(objects.keys().cloned());
    for preprocessor in optional_array(root.get("preprocessors"), "renderer preprocessors")? {
        inventory
            .preprocessors
            .insert(string(preprocessor, "renderer preprocessor")?.to_owned());
    }
    for layer in optional_array(root.get("layers"), "renderer layers")? {
        let layer = object(layer, "renderer layer")?;
        if let Some(id) = layer.get("id") {
            inventory
                .layer_ids
                .insert(string(id, "renderer layer ID")?.to_owned());
        }
    }
    inventory_semantics(metadata, &mut inventory, "")?;

    for (object_type, value) in objects {
        let definition = object(value, &format!("renderer object {object_type}"))?;
        for calculation in optional_array(definition.get("calculations"), "object calculations")? {
            let calculation = object(calculation, "renderer calculation")?;
            if let Some(id) = calculation.get("id") {
                inventory
                    .calculation_ids
                    .insert(string(id, "calculation ID")?.to_owned());
            }
        }
        for processor in optional_array(definition.get("processors"), "object processors")? {
            let processor = object(processor, "renderer processor")?;
            inventory
                .processor_types
                .insert(processor_type(processor)?.to_owned());
            for action in optional_array(processor.get("actions"), "processor actions")? {
                inventory_action(action, &mut inventory.action_types)?;
            }
        }
        if let Some(disappear) = definition.get("disappearProcessor") {
            inventory
                .processor_types
                .insert(processor_type(object(disappear, "disappear processor")?)?.to_owned());
        }
        for group in optional_array(definition.get("actions"), "object action groups")? {
            let group = object(group, "renderer action group")?;
            for action in optional_array(group.get("actions"), "renderer action group actions")? {
                inventory_action(action, &mut inventory.action_types)?;
            }
        }
    }
    Ok(inventory)
}

fn inventory_action(value: &Value, actions: &mut BTreeSet<String>) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                inventory_action(value, actions)?;
            }
        }
        Value::Object(object) => {
            if let Some(action) = object.get("action") {
                actions.insert(string(action, "renderer action type")?.to_owned());
            }
            for value in object.values() {
                inventory_action(value, actions)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn inventory_semantics(
    value: &Value,
    inventory: &mut RecomputedInventory,
    parent_key: &str,
) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                inventory_semantics(value, inventory, parent_key)?;
            }
        }
        Value::Object(object) => {
            if let Some(source) = canonical_function_source(object)? {
                let serialized = serde_json::to_vec(source)?;
                inventory
                    .function_semantics
                    .insert(format!("{parent_key}:{}", hex(&Sha256::digest(serialized))));
                return Ok(());
            }
            for (key, value) in object {
                if key.starts_with('$') && !matches!(key.as_str(), "$bigint" | "$undefined") {
                    inventory.expression_operators.insert(key.clone());
                }
                if key == "method" && parent_key == "drawings" {
                    inventory
                        .drawing_methods
                        .insert(string(value, "renderer drawing method")?.to_owned());
                }
                inventory_semantics(
                    value,
                    inventory,
                    if key == "method" { parent_key } else { key },
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn canonical_function_source(object: &Map<String, Value>) -> Result<Option<&str>> {
    if object.len() == 1 && object.contains_key("$function") {
        return Ok(Some(string(
            object.get("$function").expect("checked key"),
            "canonical renderer function",
        )?));
    }
    Ok(None)
}

fn processor_kind(processor: &Map<String, Value>) -> Result<ProcessorKind> {
    ProcessorKind::try_from(processor_type(processor)?)
}

fn processor_type(processor: &Map<String, Value>) -> Result<&str> {
    processor
        .get("type")
        .or_else(|| processor.get("name"))
        .ok_or_else(|| Error::Invalid("renderer processor lacks type/name".to_owned()))
        .and_then(|value| string(value, "renderer processor type"))
}

fn semantic_id(processor: &Map<String, Value>, object_type: &str, index: usize) -> String {
    processor
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !is_runtime_id(id))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("auto:$.objects.{object_type}.processors[{index}]"))
}

fn definition_id(object_type: &str, index: usize) -> String {
    format!("auto:$.objects.{object_type}.processors[{index}]")
}

fn is_runtime_id(value: &str) -> bool {
    value.strip_prefix("id#").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn compare_inventory(name: &str, actual: &BTreeSet<String>, declared: &[String]) -> Result<()> {
    if actual.iter().ne(declared.iter()) {
        return Err(Error::Invalid(format!(
            "renderer contract inventory {name} does not match metadata"
        )));
    }
    Ok(())
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("{label} must be an object")))
}

fn optional_array<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a [Value]> {
    match value {
        None => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        Some(_) => Err(Error::Invalid(format!("{label} must be an array"))),
    }
}

fn string<'a>(value: &'a Value, label: &str) -> Result<&'a str> {
    value
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("{label} must be a string")))
}

fn optional_string(value: Option<&Value>, label: &str) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => string(value, label).map(|value| Some(value.to_owned())),
    }
}

fn optional_number(value: Option<&Value>, default: f64, label: &str) -> Result<f64> {
    match value {
        None => Ok(default),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| Error::Invalid(format!("{label} must be finite"))),
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::artifact::tests::{artifact_json, signed};
    use crate::{ProcessorKind, RendererPlan, ReplayArtifact, SlotBudget};

    #[test]
    fn recognizes_every_retained_processor_and_assigns_explicit_slot_budgets() {
        let kinds = super::RETAINED_PROCESSOR_TYPES
            .iter()
            .map(|name| ProcessorKind::try_from(*name).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(kinds.len(), 17);
        assert_eq!(ProcessorKind::Container.slot_budget(), SlotBudget::None);
        assert_eq!(
            ProcessorKind::CreepBuildBody.slot_budget(),
            SlotBudget::Fixed(13)
        );
        assert_eq!(
            ProcessorKind::ObjectDecoration.slot_budget(),
            SlotBudget::Dynamic
        );
    }

    #[test]
    fn compiles_typed_object_and_processor_plans_with_javascript_truthiness() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"] = json!({
            "layers": [
                {"id": "terrain"},
                {"id": "objects", "isDefault": true}
            ],
            "objects": {
                "unit": {
                    "actions": [],
                    "calculations": [],
                    "data": {"x": {"$state": "x"}},
                    "processors": [{
                        "actions": [],
                        "id": "body",
                        "layer": "objects",
                        "once": "true",
                        "payload": {"texture": "unit"},
                        "type": "sprite",
                        "zIndex": 2
                    }],
                    "texture": "unit",
                    "zIndex": 4
                }
            },
            "preprocessors": []
        });
        root["rendererContract"]["inventory"] = json!({
            "objectTypes": ["unit"],
            "processorTypes": ["sprite"],
            "actionTypes": [],
            "preprocessors": [],
            "calculationIds": [],
            "drawingMethods": [],
            "expressionOperators": ["$state"],
            "functionSemantics": [],
            "layerIds": ["objects", "terrain"],
            "rendererImplementationFingerprints": []
        });
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let fingerprint = root["rendererContract"]["fingerprint"].clone();
        root["replay"]["rendererContractFingerprint"] = fingerprint;
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        assert_eq!(
            plan.layers
                .iter()
                .map(|layer| (layer.id.as_str(), layer.order, layer.is_default))
                .collect::<Vec<_>>(),
            [("terrain", 0, false), ("objects", 1, true)]
        );
        assert_eq!(plan.default_layer_id.as_deref(), Some("objects"));
        assert_eq!(plan.layer_orders["objects"], 1);
        let unit = &plan.objects["unit"];
        assert_eq!(unit.z_index, 4.0);
        assert!(matches!(unit.data, crate::CompiledValue::Object(_)));
        assert_eq!(
            unit.processors[0].definition_id,
            "auto:$.objects.unit.processors[0]"
        );
        assert_eq!(unit.processors[0].scope_id, "body");
        assert!(unit.processors[0].once);
        assert!(!unit.processors[0].uses_object_texture_fallback);
        assert_eq!(plan.max_fixed_template_slots, 1);
        assert!(!plan.has_dynamic_outputs);
    }

    #[test]
    fn rejects_inventory_that_hides_metadata_semantics() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"]["processors"] =
            json!([{"type": "secretProcessor"}]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let fingerprint = root["rendererContract"]["fingerprint"].clone();
        root["replay"]["rendererContractFingerprint"] = fingerprint;
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let result = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn preserves_duplicate_scope_ids_with_unique_definition_ids() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"]["processors"] = json!([
            {"id": "flare", "type": "sprite"},
            {"id": "flare", "type": "sprite"}
        ]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let fingerprint = root["rendererContract"]["fingerprint"].clone();
        root["replay"]["rendererContractFingerprint"] = fingerprint;
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let processors = &plan.objects["unit"].processors;
        assert_eq!(processors[0].scope_id, "flare");
        assert_eq!(processors[1].scope_id, "flare");
        assert_ne!(processors[0].definition_id, processors[1].definition_id);
    }

    #[test]
    fn fails_closed_on_random_processor_payload_until_lazy_adapter_lowering() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"]["processors"] =
            json!([{"payload": {"x": {"$random": 10}}, "type": "sprite"}]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]["inventory"]["expressionOperators"] = json!(["$random"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        assert!(RendererPlan::compile(&artifact.renderer_contract).is_err());
    }

    #[test]
    fn sprite_texture_fallback_matches_raw_destructuring_default_semantics() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"]["texture"] = json!("unit");
        root["rendererContract"]["metadata"]["objects"]["unit"]["processors"] = json!([
            {"type": "sprite"},
            {"payload": {"texture": {"$undefined": true}}, "type": "sprite"},
            {"payload": {"texture": null}, "type": "sprite"},
            {"payload": {"texture": false}, "type": "sprite"},
            {"payload": {"texture": ""}, "type": "sprite"},
            {"payload": {"texture": {"$state": "texture"}}, "type": "sprite"}
        ]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]["inventory"]["expressionOperators"] = json!(["$state"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        assert_eq!(
            plan.objects["unit"]
                .processors
                .iter()
                .map(|processor| processor.uses_object_texture_fallback)
                .collect::<Vec<_>>(),
            [true, true, false, false, false, false]
        );
    }
}
