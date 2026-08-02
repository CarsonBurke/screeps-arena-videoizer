use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::{
    CompiledValue, Error, RETAINED_EXPRESSION_OPERATORS, ResolvedValue, Result, ValueContext,
};

pub const RETAINED_ACTION_TYPES: [&str; 14] = [
    "AlphaTo",
    "DelayTime",
    "Ease",
    "FadeIn",
    "FadeOut",
    "FilterTo",
    "MoveTo",
    "Repeat",
    "RotateBy",
    "RotateTo",
    "ScaleTo",
    "Sequence",
    "Spawn",
    "TintTo",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ActionKind {
    AlphaTo,
    DelayTime,
    Ease,
    FadeIn,
    FadeOut,
    FilterTo,
    MoveBy,
    MoveTo,
    Repeat,
    RotateBy,
    RotateTo,
    ScaleTo,
    Sequence,
    Spawn,
    TintTo,
}

#[derive(Clone, Debug)]
pub struct ActionNode {
    pub kind: ActionKind,
    /// Canonical unresolved parameters. Nested action specifications remain
    /// typed so evaluating a parent does not consume their expressions twice.
    pub params: Vec<ActionParameter>,
}

#[derive(Clone, Debug)]
pub enum ActionParameter {
    Value(CompiledValue),
    Action(Box<ActionNode>),
    Array(Vec<ActionParameter>),
    Object(BTreeMap<String, ActionParameter>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedActionNode {
    pub kind: ActionKind,
    pub params: Vec<ResolvedActionParameter>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedActionParameter {
    Value(ResolvedValue),
    Action(Box<ResolvedActionNode>),
    Array(Vec<ResolvedActionParameter>),
    Object(BTreeMap<String, ResolvedActionParameter>),
}

#[derive(Clone, Debug)]
pub struct ActionGroupPlan {
    pub definition_id: String,
    pub scope_id: String,
    pub target_id: Option<String>,
    pub once: bool,
    pub actions: Vec<ActionNode>,
    pub payload: Value,
}

impl ActionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlphaTo => "AlphaTo",
            Self::DelayTime => "DelayTime",
            Self::Ease => "Ease",
            Self::FadeIn => "FadeIn",
            Self::FadeOut => "FadeOut",
            Self::FilterTo => "FilterTo",
            Self::MoveBy => "MoveBy",
            Self::MoveTo => "MoveTo",
            Self::Repeat => "Repeat",
            Self::RotateBy => "RotateBy",
            Self::RotateTo => "RotateTo",
            Self::ScaleTo => "ScaleTo",
            Self::Sequence => "Sequence",
            Self::Spawn => "Spawn",
            Self::TintTo => "TintTo",
        }
    }

    const fn parameter_range(self) -> (usize, usize) {
        match self {
            Self::AlphaTo | Self::RotateBy | Self::RotateTo | Self::TintTo => (2, 2),
            Self::DelayTime | Self::FadeIn | Self::FadeOut | Self::Sequence | Self::Spawn => (1, 1),
            Self::Ease | Self::Repeat => (1, 2),
            Self::FilterTo => (4, 4),
            Self::MoveBy | Self::MoveTo | Self::ScaleTo => (3, 3),
        }
    }
}

impl ActionNode {
    pub fn evaluate(
        &self,
        context: &ValueContext<'_>,
        random: &mut impl FnMut() -> f64,
    ) -> Result<ResolvedActionNode> {
        Ok(ResolvedActionNode {
            kind: self.kind,
            params: self
                .params
                .iter()
                .map(|parameter| parameter.evaluate(context, random))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    pub fn nested_action_count(&self) -> usize {
        self.params.iter().map(ActionParameter::action_count).sum()
    }

    pub fn contains_operator(&self, operator: crate::ExpressionOperator) -> bool {
        self.params
            .iter()
            .any(|parameter| parameter.contains_operator(operator))
    }
}

impl ActionParameter {
    fn evaluate(
        &self,
        context: &ValueContext<'_>,
        random: &mut impl FnMut() -> f64,
    ) -> Result<ResolvedActionParameter> {
        match self {
            Self::Value(value) => value
                .evaluate(context, random)
                .map(ResolvedActionParameter::Value),
            Self::Action(action) => action
                .evaluate(context, random)
                .map(Box::new)
                .map(ResolvedActionParameter::Action),
            Self::Array(values) => values
                .iter()
                .map(|value| value.evaluate(context, random))
                .collect::<Result<Vec<_>>>()
                .map(ResolvedActionParameter::Array),
            Self::Object(values) => values
                .iter()
                .map(|(key, value)| {
                    value
                        .evaluate(context, random)
                        .map(|value| (key.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>>>()
                .map(ResolvedActionParameter::Object),
        }
    }

    fn action_count(&self) -> usize {
        match self {
            Self::Value(_) => 0,
            Self::Action(action) => 1 + action.nested_action_count(),
            Self::Array(values) => values.iter().map(Self::action_count).sum(),
            Self::Object(values) => values.values().map(Self::action_count).sum(),
        }
    }

    fn contains_operator(&self, operator: crate::ExpressionOperator) -> bool {
        match self {
            Self::Value(value) => value.contains_operator(operator),
            Self::Action(action) => action.contains_operator(operator),
            Self::Array(values) => values.iter().any(|value| value.contains_operator(operator)),
            Self::Object(values) => values
                .values()
                .any(|value| value.contains_operator(operator)),
        }
    }
}

impl TryFrom<&str> for ActionKind {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "AlphaTo" => Ok(Self::AlphaTo),
            "DelayTime" => Ok(Self::DelayTime),
            "Ease" => Ok(Self::Ease),
            "FadeIn" => Ok(Self::FadeIn),
            "FadeOut" => Ok(Self::FadeOut),
            "FilterTo" => Ok(Self::FilterTo),
            "MoveBy" => Ok(Self::MoveBy),
            "MoveTo" => Ok(Self::MoveTo),
            "Repeat" => Ok(Self::Repeat),
            "RotateBy" => Ok(Self::RotateBy),
            "RotateTo" => Ok(Self::RotateTo),
            "ScaleTo" => Ok(Self::ScaleTo),
            "Sequence" => Ok(Self::Sequence),
            "Spawn" => Ok(Self::Spawn),
            "TintTo" => Ok(Self::TintTo),
            other => Err(Error::Invalid(format!(
                "native action plan does not implement renderer action {other}"
            ))),
        }
    }
}

pub(crate) fn compile_action_nodes(values: &[Value], label: &str) -> Result<Vec<ActionNode>> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| compile_action_node(value, &format!("{label}[{index}]")))
        .collect()
}

pub(crate) fn compile_action_group(
    value: &Value,
    object_type: &str,
    index: usize,
) -> Result<ActionGroupPlan> {
    let label = format!("renderer object {object_type} action group {index}");
    let group = object(value, &label)?;
    let actions = array(
        group
            .get("actions")
            .ok_or_else(|| Error::Invalid(format!("{label} lacks actions")))?,
        &format!("{label} actions"),
    )?;
    let definition_id = format!("auto:$.objects.{object_type}.actions[{index}]");
    let scope_id = group
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !is_runtime_id(id))
        .unwrap_or(&definition_id)
        .to_owned();
    let target_id = group
        .get("targetId")
        .map(action_target_id)
        .transpose()?
        .flatten();
    Ok(ActionGroupPlan {
        definition_id,
        scope_id,
        target_id,
        once: group.get("once").is_some_and(js_truthy),
        actions: compile_action_nodes(actions, &format!("{label} actions"))?,
        payload: value.clone(),
    })
}

fn action_target_id(value: &Value) -> Result<Option<String>> {
    if let Value::Object(object) = value
        && object.len() == 1
    {
        if object.get("$undefined") == Some(&Value::Bool(true)) {
            return Ok(None);
        }
        if let Some(Value::String(value)) = object.get("$bigint") {
            let truthy = value
                .trim_start_matches(['+', '-'])
                .chars()
                .any(|digit| digit != '0');
            return Ok(truthy.then(|| value.clone()));
        }
    }
    js_truthy(value)
        .then(|| crate::value_plan::json_property_key(value))
        .transpose()
}

fn compile_action_node(value: &Value, label: &str) -> Result<ActionNode> {
    let specification = object(value, label)?;
    let name = specification
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid(format!("{label} lacks an action type")))?;
    let kind = ActionKind::try_from(name)?;
    let params = array(
        specification
            .get("params")
            .ok_or_else(|| Error::Invalid(format!("{label} lacks params")))?,
        &format!("{label} params"),
    )?;
    let (minimum, maximum) = kind.parameter_range();
    if !(minimum..=maximum).contains(&params.len()) {
        return Err(Error::Invalid(format!(
            "renderer action {} expects {minimum}..={maximum} parameters, got {}",
            kind.as_str(),
            params.len()
        )));
    }

    match kind {
        ActionKind::Sequence | ActionKind::Spawn => {
            if !matches!(params.first(), Some(Value::Array(_))) {
                return Err(Error::Invalid(format!(
                    "renderer action {} requires an action array",
                    kind.as_str()
                )));
            }
        }
        ActionKind::Ease | ActionKind::Repeat => {
            if !matches!(
                params.first(),
                Some(Value::Object(object)) if object.get("action").and_then(Value::as_str).is_some()
            ) {
                return Err(Error::Invalid(format!(
                    "renderer action {} requires a nested action",
                    kind.as_str()
                )));
            }
        }
        _ => {}
    }

    Ok(ActionNode {
        kind,
        params: params
            .iter()
            .enumerate()
            .map(|(index, value)| {
                compile_action_parameter(value, &format!("{label}.params[{index}]"))
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn compile_action_parameter(value: &Value, label: &str) -> Result<ActionParameter> {
    match value {
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| compile_action_parameter(value, &format!("{label}[{index}]")))
            .collect::<Result<Vec<_>>>()
            .map(ActionParameter::Array),
        Value::Object(object)
            if !object
                .keys()
                .any(|key| RETAINED_EXPRESSION_OPERATORS.contains(&key.as_str()))
                && object.get("action").and_then(Value::as_str).is_some() =>
        {
            compile_action_node(value, label)
                .map(Box::new)
                .map(ActionParameter::Action)
        }
        Value::Object(object)
            if !is_encoded_scalar(object)
                && !object
                    .keys()
                    .any(|key| RETAINED_EXPRESSION_OPERATORS.contains(&key.as_str()))
                && !object.keys().any(|key| key.starts_with('$')) =>
        {
            object
                .iter()
                .map(|(key, value)| {
                    compile_action_parameter(value, &format!("{label}.{key}"))
                        .map(|value| (key.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>>>()
                .map(ActionParameter::Object)
        }
        _ => CompiledValue::compile(value, label).map(ActionParameter::Value),
    }
}

fn is_encoded_scalar(object: &Map<String, Value>) -> bool {
    object.len() == 1
        && (object.get("$undefined") == Some(&Value::Bool(true))
            || matches!(object.get("$bigint"), Some(Value::String(_))))
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("{label} must be an object")))
}

fn array<'a>(value: &'a Value, label: &str) -> Result<&'a [Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| Error::Invalid(format!("{label} must be an array")))
}

fn is_runtime_id(value: &str) -> bool {
    value.strip_prefix("id#").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::{
        ActionKind, RETAINED_ACTION_TYPES, ResolvedActionParameter, ResolvedValue, ValueContext,
    };

    #[test]
    fn recognizes_every_retained_action_type() {
        let kinds = RETAINED_ACTION_TYPES
            .iter()
            .map(|name| ActionKind::try_from(*name).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(kinds.len(), 14);
    }

    #[test]
    fn action_group_targets_follow_javascript_truthiness_and_property_keys() {
        let root = super::compile_action_group(&json!({"targetId": "", "actions": []}), "unit", 0)
            .unwrap();
        assert_eq!(root.target_id, None);
        let numeric =
            super::compile_action_group(&json!({"targetId": 7, "actions": []}), "unit", 1).unwrap();
        assert_eq!(numeric.target_id.as_deref(), Some("7"));
        let boolean =
            super::compile_action_group(&json!({"targetId": true, "actions": []}), "unit", 2)
                .unwrap();
        assert_eq!(boolean.target_id.as_deref(), Some("true"));
        let undefined = super::compile_action_group(
            &json!({"targetId": {"$undefined": true}, "actions": []}),
            "unit",
            3,
        )
        .unwrap();
        assert_eq!(undefined.target_id, None);
        let zero_bigint = super::compile_action_group(
            &json!({"targetId": {"$bigint": "0"}, "actions": []}),
            "unit",
            4,
        )
        .unwrap();
        assert_eq!(zero_bigint.target_id, None);
        let one_bigint = super::compile_action_group(
            &json!({"targetId": {"$bigint": "1"}, "actions": []}),
            "unit",
            5,
        )
        .unwrap();
        assert_eq!(one_bigint.target_id.as_deref(), Some("1"));
    }

    #[test]
    fn compiles_and_validates_nested_action_trees() {
        let node = super::compile_action_node(
            &json!({
                "action": "Sequence",
                "params": [[
                    {"action": "AlphaTo", "params": [0.5, 0.2]},
                    {"action": "Repeat", "params": [
                        {"action": "RotateBy", "params": [3, 1]}
                    ]}
                ]]
            }),
            "test action",
        )
        .unwrap();
        assert_eq!(node.kind, ActionKind::Sequence);
        assert_eq!(node.nested_action_count(), 3);

        assert!(
            super::compile_action_node(
                &json!({"action": "ScaleTo", "params": [1, 2]}),
                "bad action"
            )
            .is_err()
        );
        assert!(
            super::compile_action_node(
                &json!({"action": "Spawn", "params": [{"action": "FadeOut", "params": [1]}]}),
                "bad action"
            )
            .is_err()
        );
    }

    #[test]
    fn resolves_nested_actions_once_in_javascript_parameter_order() {
        let node = super::compile_action_node(
            &json!({
                "action": "Sequence",
                "params": [[
                    {"action": "DelayTime", "params": [{"$random": 10}]},
                    {"action": "AlphaTo", "params": [0.5, 0.2]}
                ]]
            }),
            "test action",
        )
        .unwrap();
        let empty = ResolvedValue::Object(BTreeMap::new());
        let context = ValueContext {
            state: &empty,
            calculations: &empty,
            processor_parameters: &empty,
            relative: None,
        };
        let mut calls = 0;
        let resolved = node
            .evaluate(&context, &mut || {
                calls += 1;
                0.25
            })
            .unwrap();
        assert_eq!(calls, 1);
        let ResolvedActionParameter::Array(actions) = &resolved.params[0] else {
            panic!("expected resolved action array")
        };
        let ResolvedActionParameter::Action(delay) = &actions[0] else {
            panic!("expected nested delay")
        };
        assert_eq!(
            delay.params[0],
            ResolvedActionParameter::Value(ResolvedValue::Number(2.5))
        );
    }
}
