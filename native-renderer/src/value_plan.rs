use std::collections::BTreeMap;

use serde_json::Value;

use crate::{Error, Result};

pub const RETAINED_EXPRESSION_OPERATORS: [&str; 5] =
    ["$calc", "$processorParam", "$random", "$rel", "$state"];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ExpressionOperator {
    Calculation,
    ProcessorParameter,
    Random,
    Relative,
    State,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpressionPlan {
    pub operator: ExpressionOperator,
    pub parameter: Box<CompiledValue>,
    /// The complete expression object excluding the selected operator.
    /// This can itself be an expression when multiple retained operators are
    /// present; the official evaluator resolves it before invoking the first
    /// operator in namespace order.
    pub options: Box<CompiledValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompiledValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<CompiledValue>),
    Object(BTreeMap<String, CompiledValue>),
    Expression(ExpressionPlan),
    Undefined,
    BigInt(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedValue {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<ResolvedValue>),
    Object(BTreeMap<String, ResolvedValue>),
    BigInt(String),
}

fn js_number_string(value: f64) -> Result<String> {
    if !value.is_finite() {
        return Err(Error::Invalid(
            "JavaScript property-key number must be finite".to_owned(),
        ));
    }
    if value == 0.0 {
        return Ok("0".to_owned());
    }
    let absolute = value.abs();
    if !(1e-6..1e21).contains(&absolute) {
        let raw = format!("{value:e}");
        let (coefficient, exponent) = raw
            .split_once('e')
            .expect("Rust scientific formatting includes an exponent");
        let exponent = exponent.parse::<i32>().map_err(|_| {
            Error::Invalid("failed to format JavaScript numeric property key".to_owned())
        })?;
        return Ok(format!(
            "{coefficient}e{}{exponent}",
            if exponent >= 0 { "+" } else { "" }
        ));
    }
    Ok(value.to_string())
}

pub(crate) fn js_property_key(value: &ResolvedValue) -> Result<String> {
    fn string(value: &ResolvedValue) -> Result<String> {
        match value {
            ResolvedValue::Undefined => Ok("undefined".to_owned()),
            ResolvedValue::Null => Ok("null".to_owned()),
            ResolvedValue::Bool(value) => Ok(value.to_string()),
            ResolvedValue::Number(value) => js_number_string(*value),
            ResolvedValue::String(value) | ResolvedValue::BigInt(value) => Ok(value.clone()),
            ResolvedValue::Array(values) => values
                .iter()
                .map(|value| match value {
                    ResolvedValue::Undefined | ResolvedValue::Null => Ok(String::new()),
                    value => string(value),
                })
                .collect::<Result<Vec<_>>>()
                .map(|values| values.join(",")),
            ResolvedValue::Object(_) => Ok("[object Object]".to_owned()),
        }
    }

    string(value)
}

pub(crate) fn json_property_key(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("null".to_owned()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => value.as_f64().map(js_number_string).unwrap_or_else(|| {
            Err(Error::Invalid(
                "JavaScript property-key number is outside f64 range".to_owned(),
            ))
        }),
        Value::String(value) => Ok(value.clone()),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::Null => Ok(String::new()),
                Value::Object(object)
                    if object.len() == 1
                        && object.get("$undefined") == Some(&Value::Bool(true)) =>
                {
                    Ok(String::new())
                }
                value => json_property_key(value),
            })
            .collect::<Result<Vec<_>>>()
            .map(|values| values.join(",")),
        Value::Object(object)
            if object.len() == 1 && object.get("$undefined") == Some(&Value::Bool(true)) =>
        {
            Ok("undefined".to_owned())
        }
        Value::Object(object) if object.len() == 1 && object.contains_key("$bigint") => object
            .get("$bigint")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Invalid("encoded BigInt property key is invalid".to_owned())),
        Value::Object(_) => Ok("[object Object]".to_owned()),
    }
}

pub struct ValueContext<'a> {
    pub state: &'a ResolvedValue,
    pub calculations: &'a ResolvedValue,
    pub processor_parameters: &'a ResolvedValue,
    pub relative: Option<&'a ResolvedValue>,
}

impl ExpressionOperator {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Calculation => "$calc",
            Self::ProcessorParameter => "$processorParam",
            Self::Random => "$random",
            Self::Relative => "$rel",
            Self::State => "$state",
        }
    }
}

impl TryFrom<&str> for ExpressionOperator {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "$calc" => Ok(Self::Calculation),
            "$processorParam" => Ok(Self::ProcessorParameter),
            "$random" => Ok(Self::Random),
            "$rel" => Ok(Self::Relative),
            "$state" => Ok(Self::State),
            other => Err(Error::Invalid(format!(
                "native value plan does not implement renderer expression {other}"
            ))),
        }
    }
}

impl CompiledValue {
    pub fn compile(value: &Value, label: &str) -> Result<Self> {
        match value {
            Value::Null => Ok(Self::Null),
            Value::Bool(value) => Ok(Self::Bool(*value)),
            Value::Number(value) => value
                .as_f64()
                .filter(|value| value.is_finite())
                .map(Self::Number)
                .ok_or_else(|| Error::Invalid(format!("{label} contains a non-finite number"))),
            Value::String(value) => Ok(Self::String(value.clone())),
            Value::Array(values) => values
                .iter()
                .enumerate()
                .map(|(index, value)| Self::compile(value, &format!("{label}[{index}]")))
                .collect::<Result<Vec<_>>>()
                .map(Self::Array),
            Value::Object(object) => {
                if object.len() == 1 {
                    if object.get("$undefined") == Some(&Value::Bool(true)) {
                        return Ok(Self::Undefined);
                    }
                    if let Some(Value::String(value)) = object.get("$bigint") {
                        return Ok(Self::BigInt(value.clone()));
                    }
                    if object.contains_key("$function") {
                        return Err(Error::Invalid(format!(
                            "{label} contains an executable function where a renderer value is required"
                        )));
                    }
                }
                let operator = RETAINED_EXPRESSION_OPERATORS
                    .iter()
                    .find(|operator| object.contains_key(**operator))
                    .copied()
                    .map(ExpressionOperator::try_from)
                    .transpose()?;
                if let Some(operator) = operator {
                    let parameter = Box::new(Self::compile(
                        object
                            .get(operator.as_str())
                            .expect("collected expression operator"),
                        &format!("{label}.{}", operator.as_str()),
                    )?);
                    let options = object
                        .iter()
                        .filter(|(key, _)| key.as_str() != operator.as_str())
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    let options = Box::new(Self::compile(
                        &Value::Object(options),
                        &format!("{label} options"),
                    )?);
                    return Ok(Self::Expression(ExpressionPlan {
                        operator,
                        parameter,
                        options,
                    }));
                }
                if let Some(operator) = object.keys().find(|key| key.starts_with('$')) {
                    return Err(Error::Invalid(format!(
                        "{label} contains unsupported renderer expression {operator}"
                    )));
                }
                object
                    .iter()
                    .map(|(key, value)| {
                        Self::compile(value, &format!("{label}.{key}"))
                            .map(|value| (key.clone(), value))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()
                    .map(Self::Object)
            }
        }
    }

    pub fn evaluate(
        &self,
        context: &ValueContext<'_>,
        random: &mut impl FnMut() -> f64,
    ) -> Result<ResolvedValue> {
        match self {
            Self::Null => Ok(ResolvedValue::Null),
            Self::Bool(value) => Ok(ResolvedValue::Bool(*value)),
            Self::Number(value) => Ok(ResolvedValue::Number(*value)),
            Self::String(value) => Ok(ResolvedValue::String(value.clone())),
            Self::Undefined => Ok(ResolvedValue::Undefined),
            Self::BigInt(value) => Ok(ResolvedValue::BigInt(value.clone())),
            Self::Array(values) => values
                .iter()
                .map(|value| value.evaluate(context, random))
                .collect::<Result<Vec<_>>>()
                .map(ResolvedValue::Array),
            Self::Object(values) => values
                .iter()
                .map(|(key, value)| {
                    value
                        .evaluate(context, random)
                        .map(|value| (key.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>>>()
                .map(ResolvedValue::Object),
            Self::Expression(expression) => expression.evaluate(context, random),
        }
    }

    pub fn contains_operator(&self, operator: ExpressionOperator) -> bool {
        match self {
            Self::Array(values) => values.iter().any(|value| value.contains_operator(operator)),
            Self::Object(values) => values
                .values()
                .any(|value| value.contains_operator(operator)),
            Self::Expression(expression) => {
                expression.operator == operator
                    || expression.parameter.contains_operator(operator)
                    || expression.options.contains_operator(operator)
            }
            Self::Null
            | Self::Bool(_)
            | Self::Number(_)
            | Self::String(_)
            | Self::Undefined
            | Self::BigInt(_) => false,
        }
    }

    pub fn object_field(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(values) => values.get(key),
            _ => None,
        }
    }
}

impl ExpressionPlan {
    fn evaluate(
        &self,
        context: &ValueContext<'_>,
        random: &mut impl FnMut() -> f64,
    ) -> Result<ResolvedValue> {
        let parameter = self.parameter.evaluate(context, random)?;
        // The reference evaluator resolves the entire `rest` object before
        // invoking an operator, including options the operator does not use.
        // Preserve that order so nested `$random` calls stay byte-identical.
        let evaluated_options = self.options.evaluate(context, random)?;
        if self.operator == ExpressionOperator::Random {
            let range = parameter.as_number().ok_or_else(|| {
                Error::Invalid("renderer $random range must resolve to a number".to_owned())
            })?;
            let value = random();
            if !value.is_finite() || !(0.0..1.0).contains(&value) {
                return Err(Error::Invalid(
                    "renderer random source must return a finite value in [0, 1)".to_owned(),
                ));
            }
            return Ok(ResolvedValue::Number(value * range));
        }

        let path = parameter.as_string().ok_or_else(|| {
            Error::Invalid(format!(
                "renderer {} path must resolve to a string",
                self.operator.as_str()
            ))
        })?;
        let root = match self.operator {
            ExpressionOperator::Calculation => context.calculations,
            ExpressionOperator::ProcessorParameter => context.processor_parameters,
            ExpressionOperator::Relative => context.relative.unwrap_or(&ResolvedValue::Undefined),
            ExpressionOperator::State => context.state,
            ExpressionOperator::Random => unreachable!("handled above"),
        };
        let mut resolved = resolve_path(root, path);
        if matches!(resolved, ResolvedValue::Undefined)
            && let Some(default) = evaluated_options.get("default")
        {
            resolved = default.clone();
        }
        if let ResolvedValue::Number(value) = resolved {
            let coefficient = match evaluated_options.get("koef") {
                Some(coefficient) => coefficient.as_number().ok_or_else(|| {
                    Error::Invalid(
                        "renderer expression coefficient must resolve to a number".to_owned(),
                    )
                })?,
                None => 1.0,
            };
            Ok(ResolvedValue::Number(coefficient * value))
        } else {
            Ok(resolved)
        }
    }
}

impl ResolvedValue {
    pub fn from_json(value: &Value) -> Result<Self> {
        match value {
            Value::Null => Ok(Self::Null),
            Value::Bool(value) => Ok(Self::Bool(*value)),
            Value::Number(value) => value
                .as_f64()
                .filter(|value| value.is_finite())
                .map(Self::Number)
                .ok_or_else(|| {
                    Error::Invalid("resolved value contains non-finite number".to_owned())
                }),
            Value::String(value) => Ok(Self::String(value.clone())),
            Value::Array(values) => values
                .iter()
                .map(Self::from_json)
                .collect::<Result<Vec<_>>>()
                .map(Self::Array),
            Value::Object(object) => object
                .iter()
                .map(|(key, value)| Self::from_json(value).map(|value| (key.clone(), value)))
                .collect::<Result<BTreeMap<_, _>>>()
                .map(Self::Object),
        }
    }

    pub const fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(values) => values.get(key),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Object(values) => Some(values),
            _ => None,
        }
    }
}

pub(crate) fn resolve_path(root: &ResolvedValue, path: &str) -> ResolvedValue {
    if path == "^" {
        return root.clone();
    }
    let normalized = normalize_path(path);
    let mut value = root;
    for key in normalized.split('.').filter(|key| !key.is_empty()) {
        if !resolved_js_truthy(value) {
            return value.clone();
        }
        let next = match value {
            ResolvedValue::Object(object) => object.get(key),
            ResolvedValue::Array(values) => key
                .parse::<usize>()
                .ok()
                .and_then(|index| values.get(index)),
            _ => None,
        };
        let Some(next) = next else {
            return ResolvedValue::Undefined;
        };
        value = next;
    }
    value.clone()
}

fn normalize_path(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    let mut characters = path.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '[' {
            output.push(character);
            continue;
        }
        let mut key = String::new();
        while let Some(&character) = characters.peek() {
            characters.next();
            if character == ']' {
                break;
            }
            key.push(character);
        }
        output.push('.');
        output.push_str(&key);
    }
    output.strip_prefix('.').unwrap_or(&output).to_owned()
}

pub(crate) fn resolved_js_truthy(value: &ResolvedValue) -> bool {
    match value {
        ResolvedValue::Undefined | ResolvedValue::Null => false,
        ResolvedValue::Bool(value) => *value,
        ResolvedValue::Number(value) => *value != 0.0 && !value.is_nan(),
        ResolvedValue::String(value) => !value.is_empty(),
        ResolvedValue::BigInt(value) => value
            .trim_start_matches(['+', '-'])
            .chars()
            .any(|digit| digit != '0'),
        ResolvedValue::Array(_) | ResolvedValue::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::collections::BTreeMap;

    use super::{js_property_key, resolved_js_truthy};
    use crate::{
        CompiledValue, ExpressionOperator, RETAINED_EXPRESSION_OPERATORS, ResolvedValue,
        ValueContext,
    };

    #[test]
    fn coerces_plain_json_values_to_javascript_property_keys() {
        assert_eq!(js_property_key(&ResolvedValue::Number(7.0)).unwrap(), "7");
        assert_eq!(js_property_key(&ResolvedValue::Number(-0.0)).unwrap(), "0");
        assert_eq!(
            js_property_key(&ResolvedValue::Number(1e21)).unwrap(),
            "1e+21"
        );
        assert_eq!(
            js_property_key(&ResolvedValue::Number(1e-7)).unwrap(),
            "1e-7"
        );
        assert_eq!(
            js_property_key(&ResolvedValue::Number(1e20)).unwrap(),
            "100000000000000000000"
        );
        assert_eq!(js_property_key(&ResolvedValue::Bool(true)).unwrap(), "true");
        assert_eq!(
            js_property_key(&ResolvedValue::Array(vec![
                ResolvedValue::Number(1.0),
                ResolvedValue::Null,
                ResolvedValue::String("x".to_owned()),
            ]))
            .unwrap(),
            "1,,x"
        );
        assert_eq!(
            js_property_key(&ResolvedValue::Object(BTreeMap::new())).unwrap(),
            "[object Object]"
        );
        assert_eq!(
            js_property_key(&ResolvedValue::BigInt("0".to_owned())).unwrap(),
            "0"
        );
        assert!(!resolved_js_truthy(&ResolvedValue::BigInt("0".to_owned())));
        assert!(resolved_js_truthy(&ResolvedValue::BigInt("1".to_owned())));
    }

    #[test]
    fn recognizes_every_retained_expression_operator() {
        let operators = RETAINED_EXPRESSION_OPERATORS
            .iter()
            .map(|operator| ExpressionOperator::try_from(*operator).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(operators.len(), 5);
    }

    #[test]
    fn compiles_nested_values_with_fixed_operator_precedence() {
        let value = CompiledValue::compile(
            &json!({
                "position": [
                    {"$state": "x", "koef": 100},
                    {"$calc": "height", "default": 0}
                ],
                "visible": true
            }),
            "payload",
        )
        .unwrap();
        let CompiledValue::Object(object) = value else {
            panic!("expected object")
        };
        let CompiledValue::Array(position) = &object["position"] else {
            panic!("expected position array")
        };
        let CompiledValue::Expression(expression) = &position[0] else {
            panic!("expected expression")
        };
        assert_eq!(expression.operator, ExpressionOperator::State);

        let precedence =
            CompiledValue::compile(&json!({"$state": "x", "$calc": "x"}), "precedence payload")
                .unwrap();
        let CompiledValue::Expression(precedence) = precedence else {
            panic!("expected expression")
        };
        assert_eq!(precedence.operator, ExpressionOperator::Calculation);
        let literal_undefined =
            CompiledValue::compile(&json!({"texture": {"$undefined": true}}), "literal").unwrap();
        assert!(matches!(
            literal_undefined.object_field("texture"),
            Some(CompiledValue::Undefined)
        ));
        let expression_undefined =
            CompiledValue::compile(&json!({"texture": {"$state": "missing"}}), "expression")
                .unwrap();
        assert!(matches!(
            expression_undefined.object_field("texture"),
            Some(CompiledValue::Expression(_))
        ));
        assert!(
            CompiledValue::compile(&json!({"$function": "() => 1"}), "executable payload").is_err()
        );
    }

    #[test]
    fn evaluates_retained_expression_semantics_and_path_edge_cases() {
        let state = ResolvedValue::Object(BTreeMap::from([
            ("x".to_owned(), ResolvedValue::Number(4.0)),
            (
                "nested".to_owned(),
                ResolvedValue::Object(BTreeMap::from([(
                    "value".to_owned(),
                    ResolvedValue::Number(3.0),
                )])),
            ),
            ("falsy".to_owned(), ResolvedValue::Number(0.0)),
        ]));
        let calculations = ResolvedValue::Object(BTreeMap::from([(
            "height".to_owned(),
            ResolvedValue::Number(7.0),
        )]));
        let processor_parameters = ResolvedValue::Object(BTreeMap::from([(
            "tickDuration".to_owned(),
            ResolvedValue::Number(0.25),
        )]));
        let context = ValueContext {
            state: &state,
            calculations: &calculations,
            processor_parameters: &processor_parameters,
            relative: None,
        };
        let plan = CompiledValue::compile(
            &json!([
                {"$state": "x", "koef": 100},
                {"$calc": "missing", "default": 9, "koef": 2},
                {"$processorParam": "tickDuration"},
                {"$state": "nested[value]"},
                {"$state": "falsy.missing"},
                {"$random": 8}
            ]),
            "expressions",
        )
        .unwrap();
        let mut random = || 0.25;
        let ResolvedValue::Array(values) = plan.evaluate(&context, &mut random).unwrap() else {
            panic!("expected array")
        };
        assert_eq!(
            values,
            vec![
                ResolvedValue::Number(400.0),
                ResolvedValue::Number(18.0),
                ResolvedValue::Number(0.25),
                ResolvedValue::Number(3.0),
                ResolvedValue::Number(0.0),
                ResolvedValue::Number(2.0),
            ]
        );

        let multiple =
            CompiledValue::compile(&json!({"$calc": "height", "$random": 8}), "multiple").unwrap();
        let mut calls = 0;
        assert_eq!(
            multiple
                .evaluate(&context, &mut || {
                    calls += 1;
                    0.5
                })
                .unwrap(),
            ResolvedValue::Number(7.0)
        );
        assert_eq!(calls, 1);
    }
}
