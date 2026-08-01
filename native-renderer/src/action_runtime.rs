use std::collections::BTreeMap;

use crate::{
    ActionKind, Error, ResolvedActionNode, ResolvedActionParameter, ResolvedValue, Result,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ActionTarget {
    pub x: f64,
    pub y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub rotation: f64,
    pub alpha: f64,
    pub tint: u32,
    pub filters: Vec<BTreeMap<String, f64>>,
}

impl Default for ActionTarget {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            rotation: 0.0,
            alpha: 1.0,
            tint: 0x00ff_ffff,
            filters: Vec::new(),
        }
    }
}

/// Mutable equivalent of one official renderer action instance.
///
/// The compatibility timeline supplies the exact update boundaries. This type
/// deliberately preserves renderer quirks such as Sequence discarding a
/// child's remainder, FadeIn/Out not resetting themselves in `finish`, and
/// TintTo flooring every component after each integration step.
#[derive(Clone, Debug)]
pub enum ActionRuntime {
    AlphaTo {
        target: f64,
        clock: TimeableClock,
    },
    DelayTime {
        clock: TimeableClock,
    },
    Ease {
        action: Box<ActionRuntime>,
        time_ms: f64,
        ease: ActionEasing,
        original_time_passed_ms: f64,
        time_passed_ms: f64,
    },
    FadeIn {
        target: f64,
        clock: TimeableClock,
    },
    FilterTo {
        filter_index: usize,
        property: String,
        target: f64,
        clock: TimeableClock,
    },
    MoveTo {
        x: f64,
        y: f64,
        clock: TimeableClock,
    },
    Repeat {
        action: Box<ActionRuntime>,
        count: Option<f64>,
        remaining: f64,
    },
    RotateBy {
        rotation: f64,
        target_rotation: Option<f64>,
        clock: TimeableClock,
    },
    RotateTo {
        rotation: f64,
        clock: TimeableClock,
    },
    ScaleTo {
        x: f64,
        y: f64,
        clock: TimeableClock,
    },
    Sequence {
        actions: Vec<ActionRuntime>,
        index: usize,
    },
    Spawn {
        actions: Vec<ActionRuntime>,
        active: Vec<bool>,
    },
    TintTo {
        tint: u32,
        clock: TimeableClock,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct TimeableClock {
    time_ms: f64,
    rest_ms: f64,
}

#[derive(Clone, Copy, Debug)]
pub enum ActionEasing {
    Linear,
    InQuad,
    OutQuad,
    InOutQuad,
    InCubic,
    OutCubic,
    InOutCubic,
    InQuart,
    OutQuart,
    InOutQuart,
    InQuint,
    OutQuint,
    InOutQuint,
}

impl ActionRuntime {
    pub fn from_resolved(node: &ResolvedActionNode) -> Result<Self> {
        let params = &node.params;
        Ok(match node.kind {
            ActionKind::AlphaTo => Self::AlphaTo {
                target: number(value_param(params, 0, node.kind)?, "AlphaTo alpha")?,
                clock: timeable(params, 1, node.kind)?,
            },
            ActionKind::DelayTime => Self::DelayTime {
                clock: timeable(params, 0, node.kind)?,
            },
            ActionKind::Ease => {
                let action = nested_action(params, 0, node.kind)?;
                let action = Box::new(Self::from_resolved(action)?);
                let time_ms = action.time_ms().ok_or_else(|| {
                    Error::Invalid("Ease requires a timeable nested action".to_owned())
                })?;
                let ease = match params.get(1) {
                    None => ActionEasing::OutQuad,
                    Some(parameter) => {
                        ActionEasing::try_from(string(parameter_value(parameter)?, "Ease type")?)?
                    }
                };
                Self::Ease {
                    action,
                    time_ms,
                    ease,
                    original_time_passed_ms: 0.0,
                    time_passed_ms: 0.0,
                }
            }
            ActionKind::FadeIn | ActionKind::FadeOut => Self::FadeIn {
                target: if node.kind == ActionKind::FadeIn {
                    1.0
                } else {
                    0.0
                },
                clock: timeable(params, 0, node.kind)?,
            },
            ActionKind::FilterTo => {
                let filter_index =
                    number(value_param(params, 0, node.kind)?, "FilterTo filter index")?;
                if filter_index < 0.0
                    || filter_index.fract() != 0.0
                    || filter_index > usize::MAX as f64
                {
                    return Err(Error::Invalid(
                        "FilterTo filter index must be a nonnegative integer".to_owned(),
                    ));
                }
                Self::FilterTo {
                    filter_index: filter_index as usize,
                    property: string(value_param(params, 1, node.kind)?, "FilterTo property name")?
                        .to_owned(),
                    target: number(
                        value_param(params, 2, node.kind)?,
                        "FilterTo property value",
                    )?,
                    clock: timeable(params, 3, node.kind)?,
                }
            }
            ActionKind::MoveTo => Self::MoveTo {
                x: number(value_param(params, 0, node.kind)?, "MoveTo x")?,
                y: number(value_param(params, 1, node.kind)?, "MoveTo y")?,
                clock: timeable(params, 2, node.kind)?,
            },
            ActionKind::Repeat => {
                let action = Box::new(Self::from_resolved(nested_action(params, 0, node.kind)?)?);
                let count = params
                    .get(1)
                    .map(parameter_value)
                    .transpose()?
                    .map(|value| number(value, "Repeat count"))
                    .transpose()?;
                let remaining = repeat_remaining(count);
                Self::Repeat {
                    action,
                    count,
                    remaining,
                }
            }
            ActionKind::RotateBy => Self::RotateBy {
                rotation: number(value_param(params, 0, node.kind)?, "RotateBy rotation")?,
                target_rotation: None,
                clock: timeable(params, 1, node.kind)?,
            },
            ActionKind::RotateTo => Self::RotateTo {
                rotation: number(value_param(params, 0, node.kind)?, "RotateTo rotation")?,
                clock: timeable(params, 1, node.kind)?,
            },
            ActionKind::ScaleTo => Self::ScaleTo {
                x: number(value_param(params, 0, node.kind)?, "ScaleTo x")?,
                y: number(value_param(params, 1, node.kind)?, "ScaleTo y")?,
                clock: timeable(params, 2, node.kind)?,
            },
            ActionKind::Sequence | ActionKind::Spawn => {
                let actions = action_array(params, 0, node.kind)?
                    .into_iter()
                    .map(Self::from_resolved)
                    .collect::<Result<Vec<_>>>()?;
                if node.kind == ActionKind::Sequence {
                    Self::Sequence { actions, index: 0 }
                } else {
                    let active = vec![true; actions.len()];
                    Self::Spawn { actions, active }
                }
            }
            ActionKind::TintTo => Self::TintTo {
                tint: color(value_param(params, 0, node.kind)?, "TintTo tint")?,
                clock: timeable(params, 1, node.kind)?,
            },
        })
    }

    pub fn update(&mut self, target: &mut ActionTarget, delta_ms: f64) -> Result<bool> {
        if !delta_ms.is_finite() || delta_ms < 0.0 {
            return Err(Error::Invalid(
                "action delta must be a nonnegative finite number".to_owned(),
            ));
        }
        match self {
            Self::AlphaTo {
                target: destination,
                clock,
            } => {
                target.alpha = interpolate(target.alpha, *destination, *clock, delta_ms);
                if clock.advance(delta_ms) {
                    target.alpha = *destination;
                    clock.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::DelayTime { clock } => {
                if clock.advance(delta_ms) {
                    clock.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::Ease {
                action,
                time_ms,
                ease,
                original_time_passed_ms,
                time_passed_ms,
            } => {
                *original_time_passed_ms += delta_ms;
                let ease_delta = if *original_time_passed_ms <= *time_ms {
                    (*time_ms * ease.apply(*original_time_passed_ms / *time_ms) - *time_passed_ms)
                        .max(0.0)
                } else {
                    delta_ms
                };
                *time_passed_ms += ease_delta;
                let ended = action.update(target, ease_delta)?;
                if ended {
                    action.finish(target)?;
                    *original_time_passed_ms = 0.0;
                    *time_passed_ms = 0.0;
                    action.reset();
                }
                Ok(ended)
            }
            Self::FadeIn {
                target: destination,
                clock,
            } => {
                target.alpha = interpolate(target.alpha, *destination, *clock, delta_ms);
                if clock.advance(delta_ms) {
                    // The official FadeIn.finish deliberately does not reset.
                    target.alpha = *destination;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::FilterTo {
                filter_index,
                property,
                target: destination,
                clock,
            } => {
                let filter = target.filters.get_mut(*filter_index).ok_or_else(|| {
                    Error::Invalid(format!("FilterTo references missing filter {filter_index}"))
                })?;
                let current = filter.get(property).copied().ok_or_else(|| {
                    Error::Invalid(format!("FilterTo references missing property {property}"))
                })?;
                filter.insert(
                    property.clone(),
                    interpolate(current, *destination, *clock, delta_ms),
                );
                if clock.advance(delta_ms) {
                    filter.insert(property.clone(), *destination);
                    clock.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::MoveTo { x, y, clock } => {
                target.x = interpolate(target.x, *x, *clock, delta_ms);
                target.y = interpolate(target.y, *y, *clock, delta_ms);
                if clock.advance(delta_ms) {
                    target.x = *x;
                    target.y = *y;
                    clock.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::Repeat {
                action, remaining, ..
            } => {
                if action.update(target, delta_ms)? {
                    action.reset();
                    *remaining -= 1.0;
                }
                if *remaining <= 0.0 {
                    self.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::RotateBy {
                rotation,
                target_rotation,
                clock,
            } => {
                let destination = *target_rotation.get_or_insert(target.rotation + *rotation);
                target.rotation = interpolate(target.rotation, destination, *clock, delta_ms);
                if clock.advance(delta_ms) {
                    target.rotation = destination;
                    clock.reset();
                    *target_rotation = None;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::RotateTo { rotation, clock } => {
                while *rotation - target.rotation > std::f64::consts::PI {
                    *rotation -= std::f64::consts::TAU;
                }
                while *rotation - target.rotation < -std::f64::consts::PI {
                    *rotation += std::f64::consts::TAU;
                }
                target.rotation = interpolate(target.rotation, *rotation, *clock, delta_ms);
                if clock.advance(delta_ms) {
                    target.rotation = *rotation;
                    clock.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::ScaleTo { x, y, clock } => {
                target.scale_x = interpolate(target.scale_x, *x, *clock, delta_ms);
                target.scale_y = interpolate(target.scale_y, *y, *clock, delta_ms);
                if clock.advance(delta_ms) {
                    target.scale_x = *x;
                    target.scale_y = *y;
                    clock.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::Sequence { actions, index } => {
                if *index >= actions.len() {
                    return Ok(true);
                }
                if actions[*index].update(target, delta_ms)? {
                    actions[*index].reset();
                    *index += 1;
                }
                Ok(false)
            }
            Self::Spawn { actions, active } => {
                for (action, is_active) in actions.iter_mut().zip(active.iter_mut()) {
                    if *is_active && action.update(target, delta_ms)? {
                        action.reset();
                        *is_active = false;
                    }
                }
                if active.iter().all(|active| !active) {
                    self.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Self::TintTo { tint, clock } => {
                let red = interpolate(
                    f64::from(color_component(target.tint, 16)),
                    f64::from(color_component(*tint, 16)),
                    *clock,
                    delta_ms,
                );
                let green = interpolate(
                    f64::from(color_component(target.tint, 8)),
                    f64::from(color_component(*tint, 8)),
                    *clock,
                    delta_ms,
                );
                let blue = interpolate(
                    f64::from(color_component(target.tint, 0)),
                    f64::from(color_component(*tint, 0)),
                    *clock,
                    delta_ms,
                );
                target.tint = pack_color(red, green, blue);
                if clock.advance(delta_ms) {
                    target.tint = *tint;
                    clock.reset();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    pub fn finish(&mut self, target: &mut ActionTarget) -> Result<()> {
        match self {
            Self::AlphaTo {
                target: destination,
                ..
            }
            | Self::FadeIn {
                target: destination,
                ..
            } => target.alpha = *destination,
            Self::DelayTime { .. } => {}
            Self::Ease { action, .. } => action.finish(target)?,
            Self::FilterTo {
                filter_index,
                property,
                target: destination,
                ..
            } => {
                let filter = target.filters.get_mut(*filter_index).ok_or_else(|| {
                    Error::Invalid(format!("FilterTo references missing filter {filter_index}"))
                })?;
                filter.insert(property.clone(), *destination);
            }
            Self::MoveTo { x, y, .. } => {
                target.x = *x;
                target.y = *y;
            }
            Self::Repeat { .. } | Self::Sequence { .. } | Self::Spawn { .. } => {}
            Self::RotateBy {
                target_rotation, ..
            } => target.rotation = target_rotation.unwrap_or(0.0),
            Self::RotateTo { rotation, .. } => target.rotation = *rotation,
            Self::ScaleTo { x, y, .. } => {
                target.scale_x = *x;
                target.scale_y = *y;
            }
            Self::TintTo { tint, .. } => target.tint = *tint,
        }
        if matches!(self, Self::FadeIn { .. }) {
            // FadeIn/FadeOut override finish without calling the base reset.
            return Ok(());
        }
        self.reset();
        Ok(())
    }

    pub fn reset(&mut self) {
        match self {
            Self::AlphaTo { clock, .. }
            | Self::DelayTime { clock }
            | Self::FilterTo { clock, .. }
            | Self::MoveTo { clock, .. }
            | Self::RotateTo { clock, .. }
            | Self::ScaleTo { clock, .. }
            | Self::TintTo { clock, .. } => clock.reset(),
            Self::FadeIn { clock, .. } => clock.reset(),
            Self::Ease {
                action,
                original_time_passed_ms,
                time_passed_ms,
                ..
            } => {
                *original_time_passed_ms = 0.0;
                *time_passed_ms = 0.0;
                action.reset();
            }
            Self::Repeat {
                count, remaining, ..
            } => *remaining = repeat_remaining(*count),
            Self::RotateBy {
                target_rotation,
                clock,
                ..
            } => {
                clock.reset();
                *target_rotation = None;
            }
            Self::Sequence { index, .. } => *index = 0,
            Self::Spawn { active, .. } => active.fill(true),
        }
    }

    fn time_ms(&self) -> Option<f64> {
        match self {
            Self::AlphaTo { clock, .. }
            | Self::DelayTime { clock }
            | Self::FadeIn { clock, .. }
            | Self::FilterTo { clock, .. }
            | Self::MoveTo { clock, .. }
            | Self::RotateBy { clock, .. }
            | Self::RotateTo { clock, .. }
            | Self::ScaleTo { clock, .. }
            | Self::TintTo { clock, .. } => Some(clock.time_ms),
            Self::Ease { time_ms, .. } => Some(*time_ms),
            Self::Repeat { .. } | Self::Sequence { .. } | Self::Spawn { .. } => None,
        }
    }
}

impl TimeableClock {
    fn from_seconds(seconds: f64, label: &str) -> Result<Self> {
        let time_ms = seconds * 1_000.0;
        if !time_ms.is_finite() || time_ms < 0.0 {
            return Err(Error::Invalid(format!(
                "{label} must be a nonnegative finite duration"
            )));
        }
        Ok(Self {
            time_ms,
            rest_ms: time_ms,
        })
    }

    fn advance(&mut self, delta_ms: f64) -> bool {
        self.rest_ms -= delta_ms;
        self.rest_ms <= 0.0
    }

    fn reset(&mut self) {
        self.rest_ms = self.time_ms;
    }
}

impl ActionEasing {
    fn apply(self, time: f64) -> f64 {
        match self {
            Self::Linear => time,
            Self::InQuad => time.powi(2),
            Self::OutQuad => 1.0 - (time - 1.0).powi(2).abs(),
            Self::InOutQuad => in_out(time, 2),
            Self::InCubic => time.powi(3),
            Self::OutCubic => 1.0 - (time - 1.0).powi(3).abs(),
            Self::InOutCubic => in_out(time, 3),
            Self::InQuart => time.powi(4),
            Self::OutQuart => 1.0 - (time - 1.0).powi(4).abs(),
            Self::InOutQuart => in_out(time, 4),
            Self::InQuint => time.powi(5),
            Self::OutQuint => 1.0 - (time - 1.0).powi(5).abs(),
            Self::InOutQuint => in_out(time, 5),
        }
    }
}

impl TryFrom<&str> for ActionEasing {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "LINEAR" => Ok(Self::Linear),
            "EASE_IN_QUAD" => Ok(Self::InQuad),
            "EASE_OUT_QUAD" => Ok(Self::OutQuad),
            "EASE_IN_OUT_QUAD" => Ok(Self::InOutQuad),
            "EASE_IN_CUBIC" => Ok(Self::InCubic),
            "EASE_OUT_CUBIC" => Ok(Self::OutCubic),
            "EASE_IN_OUT_CUBIC" => Ok(Self::InOutCubic),
            "EASE_IN_QUART" => Ok(Self::InQuart),
            "EASE_OUT_QUART" => Ok(Self::OutQuart),
            "EASE_IN_OUT_QUART" => Ok(Self::InOutQuart),
            "EASE_IN_QUINT" => Ok(Self::InQuint),
            "EASE_OUT_QUINT" => Ok(Self::OutQuint),
            "EASE_IN_OUT_QUINT" => Ok(Self::InOutQuint),
            _ => Err(Error::Invalid(format!("wrong Ease type {value}"))),
        }
    }
}

fn value_param(
    params: &[ResolvedActionParameter],
    index: usize,
    kind: ActionKind,
) -> Result<&ResolvedValue> {
    parameter_value(
        params
            .get(index)
            .ok_or_else(|| Error::Invalid(format!("{} lacks parameter {index}", kind.as_str())))?,
    )
}

fn parameter_value(parameter: &ResolvedActionParameter) -> Result<&ResolvedValue> {
    match parameter {
        ResolvedActionParameter::Value(value) => Ok(value),
        _ => Err(Error::Invalid(
            "action parameter has the wrong structural type".to_owned(),
        )),
    }
}

fn nested_action(
    params: &[ResolvedActionParameter],
    index: usize,
    kind: ActionKind,
) -> Result<&ResolvedActionNode> {
    match params.get(index) {
        Some(ResolvedActionParameter::Action(action)) => Ok(action),
        _ => Err(Error::Invalid(format!(
            "{} parameter {index} must be an action",
            kind.as_str()
        ))),
    }
}

fn action_array(
    params: &[ResolvedActionParameter],
    index: usize,
    kind: ActionKind,
) -> Result<Vec<&ResolvedActionNode>> {
    let Some(ResolvedActionParameter::Array(values)) = params.get(index) else {
        return Err(Error::Invalid(format!(
            "{} parameter {index} must be an action array",
            kind.as_str()
        )));
    };
    values
        .iter()
        .map(|value| match value {
            ResolvedActionParameter::Action(action) => Ok(action.as_ref()),
            _ => Err(Error::Invalid(format!(
                "{} action array contains a non-action",
                kind.as_str()
            ))),
        })
        .collect()
}

fn timeable(
    params: &[ResolvedActionParameter],
    index: usize,
    kind: ActionKind,
) -> Result<TimeableClock> {
    TimeableClock::from_seconds(
        number(value_param(params, index, kind)?, "action duration")?,
        kind.as_str(),
    )
}

fn number(value: &ResolvedValue, label: &str) -> Result<f64> {
    match value {
        ResolvedValue::Number(value) if value.is_finite() => Ok(*value),
        _ => Err(Error::Invalid(format!(
            "{label} must resolve to a finite number"
        ))),
    }
}

fn string<'a>(value: &'a ResolvedValue, label: &str) -> Result<&'a str> {
    match value {
        ResolvedValue::String(value) => Ok(value),
        _ => Err(Error::Invalid(format!("{label} must resolve to a string"))),
    }
}

fn color(value: &ResolvedValue, label: &str) -> Result<u32> {
    Ok(number(value, label)?.floor().clamp(0.0, 16_777_215.0) as u32)
}

fn interpolate(current: f64, target: f64, clock: TimeableClock, delta_ms: f64) -> f64 {
    if clock.rest_ms == 0.0 {
        target
    } else {
        current + (target - current) / clock.rest_ms * delta_ms
    }
}

fn repeat_remaining(count: Option<f64>) -> f64 {
    match count {
        Some(count) if count != 0.0 => count,
        _ => f64::INFINITY,
    }
}

fn color_component(color: u32, shift: u32) -> u32 {
    (color >> shift) & 0xff
}

fn pack_color(red: f64, green: f64, blue: f64) -> u32 {
    fn component(value: f64) -> u32 {
        value.floor().clamp(0.0, 255.0) as u32
    }
    (component(red) << 16) | (component(green) << 8) | component(blue)
}

fn in_out(time: f64, power: i32) -> f64 {
    if time < 0.5 {
        0.5 * (time * 2.0).powi(power)
    } else {
        1.0 - 0.5 * (time * 2.0 - 2.0).powi(power).abs()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ActionKind, ResolvedActionNode, ResolvedActionParameter as Parameter, ResolvedValue,
    };

    use super::{ActionRuntime, ActionTarget};

    fn value(value: f64) -> Parameter {
        Parameter::Value(ResolvedValue::Number(value))
    }

    fn text(value: &str) -> Parameter {
        Parameter::Value(ResolvedValue::String(value.to_owned()))
    }

    fn action(kind: ActionKind, params: Vec<Parameter>) -> ResolvedActionNode {
        ResolvedActionNode { kind, params }
    }

    fn nested(node: ResolvedActionNode) -> Parameter {
        Parameter::Action(Box::new(node))
    }

    #[test]
    fn timeable_scalar_vector_rotation_and_tint_match_fixed_step_mutation() {
        let mut target = ActionTarget::default();
        let mut alpha = ActionRuntime::from_resolved(&action(
            ActionKind::AlphaTo,
            vec![value(0.0), value(1.0)],
        ))
        .unwrap();
        assert!(!alpha.update(&mut target, 250.0).unwrap());
        assert_eq!(target.alpha, 0.75);
        assert!(alpha.update(&mut target, 750.0).unwrap());
        assert_eq!(target.alpha, 0.0);

        let mut movement = ActionRuntime::from_resolved(&action(
            ActionKind::MoveTo,
            vec![value(8.0), value(-4.0), value(1.0)],
        ))
        .unwrap();
        assert!(!movement.update(&mut target, 250.0).unwrap());
        assert_eq!([target.x, target.y], [2.0, -1.0]);

        target.rotation = 0.0;
        let mut rotation = ActionRuntime::from_resolved(&action(
            ActionKind::RotateTo,
            vec![value(std::f64::consts::PI * 1.5), value(1.0)],
        ))
        .unwrap();
        assert!(!rotation.update(&mut target, 500.0).unwrap());
        assert_eq!(target.rotation, -std::f64::consts::FRAC_PI_4);

        target.tint = 0;
        let mut tint = ActionRuntime::from_resolved(&action(
            ActionKind::TintTo,
            vec![value(16_777_215.0), value(1.0)],
        ))
        .unwrap();
        assert!(!tint.update(&mut target, 500.0).unwrap());
        assert_eq!(target.tint, 0x007f_7f7f);
    }

    #[test]
    fn sequence_discards_remainder_and_completes_one_update_after_last_child() {
        let sequence = action(
            ActionKind::Sequence,
            vec![Parameter::Array(vec![
                nested(action(ActionKind::DelayTime, vec![value(0.1)])),
                nested(action(ActionKind::AlphaTo, vec![value(0.0), value(0.1)])),
            ])],
        );
        let mut runtime = ActionRuntime::from_resolved(&sequence).unwrap();
        let mut target = ActionTarget::default();

        assert!(!runtime.update(&mut target, 250.0).unwrap());
        assert_eq!(target.alpha, 1.0);
        assert!(!runtime.update(&mut target, 250.0).unwrap());
        assert_eq!(target.alpha, 0.0);
        assert!(runtime.update(&mut target, 1.0).unwrap());
    }

    #[test]
    fn spawn_repeat_and_ease_preserve_official_reset_and_ordering() {
        let spawn = action(
            ActionKind::Spawn,
            vec![Parameter::Array(vec![
                nested(action(ActionKind::AlphaTo, vec![value(0.0), value(1.0)])),
                nested(action(
                    ActionKind::ScaleTo,
                    vec![value(2.0), value(3.0), value(0.5)],
                )),
            ])],
        );
        let mut runtime = ActionRuntime::from_resolved(&spawn).unwrap();
        let mut target = ActionTarget::default();
        assert!(!runtime.update(&mut target, 250.0).unwrap());
        assert_eq!(
            [target.alpha, target.scale_x, target.scale_y],
            [0.75, 1.5, 2.0]
        );
        assert!(!runtime.update(&mut target, 250.0).unwrap());
        assert_eq!(
            [target.alpha, target.scale_x, target.scale_y],
            [0.5, 2.0, 3.0]
        );
        assert!(runtime.update(&mut target, 500.0).unwrap());
        assert_eq!(target.alpha, 0.0);

        let repeat = action(
            ActionKind::Repeat,
            vec![
                nested(action(ActionKind::RotateBy, vec![value(1.0), value(0.1)])),
                value(2.0),
            ],
        );
        let mut runtime = ActionRuntime::from_resolved(&repeat).unwrap();
        target.rotation = 0.0;
        assert!(!runtime.update(&mut target, 100.0).unwrap());
        assert!(runtime.update(&mut target, 100.0).unwrap());
        assert_eq!(target.rotation, 2.0);

        let ease = action(
            ActionKind::Ease,
            vec![
                nested(action(ActionKind::AlphaTo, vec![value(0.0), value(1.0)])),
                text("EASE_OUT_QUAD"),
            ],
        );
        let mut runtime = ActionRuntime::from_resolved(&ease).unwrap();
        target.alpha = 1.0;
        assert!(!runtime.update(&mut target, 500.0).unwrap());
        assert_eq!(target.alpha, 0.25);
        assert!(runtime.update(&mut target, 500.0).unwrap());
        assert_eq!(target.alpha, 0.0);
    }

    #[test]
    fn filter_to_updates_and_finishes_the_named_filter_property() {
        let mut target = ActionTarget {
            filters: vec![std::collections::BTreeMap::from([(
                "strength".to_owned(),
                0.0,
            )])],
            ..ActionTarget::default()
        };
        let filter = action(
            ActionKind::FilterTo,
            vec![value(0.0), text("strength"), value(8.0), value(1.0)],
        );
        let mut runtime = ActionRuntime::from_resolved(&filter).unwrap();
        assert!(!runtime.update(&mut target, 250.0).unwrap());
        assert_eq!(target.filters[0]["strength"], 2.0);
        runtime.finish(&mut target).unwrap();
        assert_eq!(target.filters[0]["strength"], 8.0);
    }

    #[test]
    fn zero_duration_and_preupdate_finish_match_effective_renderer_values() {
        let mut target = ActionTarget::default();
        let mut alpha = ActionRuntime::from_resolved(&action(
            ActionKind::AlphaTo,
            vec![value(0.2), value(0.0)],
        ))
        .unwrap();
        assert!(alpha.update(&mut target, 16.0).unwrap());
        assert_eq!(target.alpha, 0.2);

        target.rotation = 4.0;
        let mut rotate_by = ActionRuntime::from_resolved(&action(
            ActionKind::RotateBy,
            vec![value(1.0), value(1.0)],
        ))
        .unwrap();
        rotate_by.finish(&mut target).unwrap();
        // The official action assigns null before its first update. Pixi's
        // transform math coerces that effective rotation to zero.
        assert_eq!(target.rotation, 0.0);

        let repeat_forever = action(
            ActionKind::Repeat,
            vec![
                nested(action(ActionKind::DelayTime, vec![value(0.0)])),
                value(0.0),
            ],
        );
        let mut repeat_forever = ActionRuntime::from_resolved(&repeat_forever).unwrap();
        assert!(!repeat_forever.update(&mut target, 16.0).unwrap());
    }
}
