use std::collections::BTreeMap;

use crate::{Error, ProcessorKind, ResolvedActivation, ResolvedScene, ResolvedValue, Result};

pub const RETAINED_DRAW_METHODS: [&str; 9] = [
    "arc",
    "beginFill",
    "drawCircle",
    "drawEllipse",
    "drawPolygon",
    "drawRect",
    "drawRoundedRect",
    "endFill",
    "lineStyle",
];

const MAX_COMMANDS_PER_PROGRAM: usize = 4_096;
const MAX_POLYGON_POINTS: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorFillStyle {
    pub color: u32,
    pub alpha: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorLineStyle {
    pub width: f64,
    pub color: u32,
    pub alpha: f64,
    pub alignment: f64,
    pub native: bool,
}

impl Default for VectorLineStyle {
    fn default() -> Self {
        Self {
            width: 0.0,
            color: 0,
            alpha: 1.0,
            alignment: 0.5,
            native: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum VectorCommand {
    BeginFill(VectorFillStyle),
    EndFill,
    LineStyle(VectorLineStyle),
    Arc {
        center: [f64; 2],
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        anticlockwise: bool,
    },
    Circle {
        center: [f64; 2],
        radius: f64,
    },
    Ellipse {
        center: [f64; 2],
        half_size: [f64; 2],
    },
    Polygon {
        points: Vec<[f64; 2]>,
    },
    Rect {
        origin: [f64; 2],
        size: [f64; 2],
    },
    RoundedRect {
        origin: [f64; 2],
        size: [f64; 2],
        radius: f64,
    },
    // Dedicated processors such as siteProgress construct paths directly.
    MoveTo([f64; 2]),
    LineTo([f64; 2]),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VectorProgram {
    pub commands: Vec<VectorCommand>,
}

impl VectorProgram {
    pub fn from_draw_payload(payload: &ResolvedValue) -> Result<Self> {
        let payload = match payload {
            ResolvedValue::Undefined => return Ok(Self::default()),
            ResolvedValue::Object(payload) => payload,
            _ => {
                return Err(Error::Invalid(
                    "draw payload must resolve to an object".to_owned(),
                ));
            }
        };
        let drawings = match payload.get("drawings") {
            None | Some(ResolvedValue::Undefined) => return Ok(Self::default()),
            Some(ResolvedValue::Array(drawings)) => drawings,
            Some(_) => {
                return Err(Error::Invalid(
                    "draw payload drawings must resolve to an array".to_owned(),
                ));
            }
        };
        if drawings.len() > MAX_COMMANDS_PER_PROGRAM {
            return Err(Error::Invalid(format!(
                "draw payload exceeds the {MAX_COMMANDS_PER_PROGRAM}-command limit"
            )));
        }
        let commands = drawings
            .iter()
            .enumerate()
            .map(|(index, drawing)| parse_drawing(drawing, index))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { commands })
    }
}

pub fn vector_graphics_programs(scene: &ResolvedScene) -> Result<BTreeMap<u32, VectorProgram>> {
    scene
        .activations
        .iter()
        .filter_map(|activation| {
            let ResolvedActivation::Processor {
                kind,
                activation_order,
                payload,
                ..
            } = activation
            else {
                return None;
            };
            match kind {
                ProcessorKind::Draw => Some(
                    VectorProgram::from_draw_payload(payload)
                        .map(|program| (*activation_order, program)),
                ),
                ProcessorKind::SiteProgress if !matches!(payload, ResolvedValue::Undefined) => {
                    Some(site_progress_program(payload).map(|program| (*activation_order, program)))
                }
                _ => None,
            }
        })
        .collect()
}

pub fn site_progress_program(payload: &ResolvedValue) -> Result<VectorProgram> {
    let payload = payload.as_object().ok_or_else(|| {
        Error::Invalid("siteProgress payload must resolve to an object".to_owned())
    })?;
    let required = |name: &str| {
        payload
            .get(name)
            .ok_or_else(|| Error::Invalid(format!("siteProgress payload lacks {name}")))
    };
    let progress = number(required("progress")?, "siteProgress progress")?;
    let progress_total = number(required("progressTotal")?, "siteProgress progressTotal")?;
    let color = optional_color(Some(required("color")?), 0, "siteProgress color")?;
    let radius = number(required("radius")?, "siteProgress radius")?;
    let line_width = number(required("lineWidth")?, "siteProgress lineWidth")?;
    nonnegative(radius, "siteProgress radius")?;
    nonnegative(line_width, "siteProgress lineWidth")?;

    let mut commands = vec![
        VectorCommand::BeginFill(VectorFillStyle {
            color: 0,
            alpha: 0.0,
        }),
        VectorCommand::LineStyle(VectorLineStyle {
            width: line_width,
            color,
            ..VectorLineStyle::default()
        }),
        VectorCommand::Circle {
            center: [0.0, 0.0],
            radius: radius + line_width / 2.0,
        },
    ];
    if progress > 0.0 && progress_total > 0.0 {
        let angle = std::f64::consts::TAU * progress.min(progress_total) / progress_total;
        commands.extend([
            VectorCommand::BeginFill(VectorFillStyle { color, alpha: 1.0 }),
            VectorCommand::MoveTo([0.0, 0.0]),
            VectorCommand::LineStyle(VectorLineStyle {
                width: 1.0,
                color,
                ..VectorLineStyle::default()
            }),
            VectorCommand::LineTo([radius, 0.0]),
            VectorCommand::Arc {
                center: [0.0, 0.0],
                radius,
                start_angle: 0.0,
                end_angle: angle,
                anticlockwise: false,
            },
            VectorCommand::LineTo([0.0, 0.0]),
            VectorCommand::EndFill,
        ]);
    }
    Ok(VectorProgram { commands })
}

fn parse_drawing(value: &ResolvedValue, index: usize) -> Result<VectorCommand> {
    let drawing = value
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("draw command {index} must resolve to an object")))?;
    let method = drawing
        .get("method")
        .and_then(ResolvedValue::as_string)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "draw command {index} method must resolve to a string"
            ))
        })?;
    if RETAINED_DRAW_METHODS.binary_search(&method).is_err() {
        return Err(Error::Invalid(format!(
            "draw command {index} uses unsupported Graphics method {method}"
        )));
    }
    let empty = Vec::new();
    let params = match drawing.get("params") {
        None | Some(ResolvedValue::Undefined) => &empty,
        Some(ResolvedValue::Array(params)) => params,
        Some(_) => {
            return Err(Error::Invalid(format!(
                "draw command {index} params must resolve to an array"
            )));
        }
    };
    let label = |parameter: &str| format!("draw command {index} {method} {parameter}");
    match method {
        "beginFill" => {
            at_most(params, 2, index, method)?;
            Ok(VectorCommand::BeginFill(VectorFillStyle {
                color: optional_color(params.first(), 0, &label("color"))?,
                alpha: optional_number(params.get(1), 1.0, &label("alpha"))?,
            }))
        }
        "endFill" => {
            exactly(params, 0, index, method)?;
            Ok(VectorCommand::EndFill)
        }
        "lineStyle" => {
            at_most(params, 5, index, method)?;
            let width = optional_number(params.first(), 0.0, &label("width"))?;
            let alignment = optional_number(params.get(3), 0.5, &label("alignment"))?;
            if width < 0.0 {
                return Err(Error::Invalid(format!(
                    "{} cannot be negative",
                    label("width")
                )));
            }
            if !(0.0..=1.0).contains(&alignment) {
                return Err(Error::Invalid(format!(
                    "{} must be between zero and one",
                    label("alignment")
                )));
            }
            Ok(VectorCommand::LineStyle(VectorLineStyle {
                width,
                color: optional_color(params.get(1), 0, &label("color"))?,
                alpha: optional_number(params.get(2), 1.0, &label("alpha"))?,
                alignment,
                native: params
                    .get(4)
                    .is_some_and(crate::value_plan::resolved_js_truthy),
            }))
        }
        "arc" => {
            between(params, 5, 6, index, method)?;
            let radius = number(&params[2], &label("radius"))?;
            nonnegative(radius, &label("radius"))?;
            Ok(VectorCommand::Arc {
                center: [
                    number(&params[0], &label("x"))?,
                    number(&params[1], &label("y"))?,
                ],
                radius,
                start_angle: number(&params[3], &label("startAngle"))?,
                end_angle: number(&params[4], &label("endAngle"))?,
                anticlockwise: params
                    .get(5)
                    .is_some_and(crate::value_plan::resolved_js_truthy),
            })
        }
        "drawCircle" => {
            exactly(params, 3, index, method)?;
            let radius = number(&params[2], &label("radius"))?;
            nonnegative(radius, &label("radius"))?;
            Ok(VectorCommand::Circle {
                center: [
                    number(&params[0], &label("x"))?,
                    number(&params[1], &label("y"))?,
                ],
                radius,
            })
        }
        "drawEllipse" => {
            exactly(params, 4, index, method)?;
            let half_size = [
                number(&params[2], &label("halfWidth"))?,
                number(&params[3], &label("halfHeight"))?,
            ];
            nonnegative(half_size[0], &label("halfWidth"))?;
            nonnegative(half_size[1], &label("halfHeight"))?;
            Ok(VectorCommand::Ellipse {
                center: [
                    number(&params[0], &label("x"))?,
                    number(&params[1], &label("y"))?,
                ],
                half_size,
            })
        }
        "drawPolygon" => {
            exactly(params, 1, index, method)?;
            Ok(VectorCommand::Polygon {
                points: polygon_points(&params[0], &label("points"))?,
            })
        }
        "drawRect" => {
            exactly(params, 4, index, method)?;
            Ok(VectorCommand::Rect {
                origin: [
                    number(&params[0], &label("x"))?,
                    number(&params[1], &label("y"))?,
                ],
                size: [
                    number(&params[2], &label("width"))?,
                    number(&params[3], &label("height"))?,
                ],
            })
        }
        "drawRoundedRect" => {
            exactly(params, 5, index, method)?;
            let radius = number(&params[4], &label("radius"))?;
            nonnegative(radius, &label("radius"))?;
            Ok(VectorCommand::RoundedRect {
                origin: [
                    number(&params[0], &label("x"))?,
                    number(&params[1], &label("y"))?,
                ],
                size: [
                    number(&params[2], &label("width"))?,
                    number(&params[3], &label("height"))?,
                ],
                radius,
            })
        }
        _ => unreachable!("retained draw method was validated above"),
    }
}

fn polygon_points(value: &ResolvedValue, label: &str) -> Result<Vec<[f64; 2]>> {
    let values = match value {
        ResolvedValue::Array(values) => values,
        _ => return Err(Error::Invalid(format!("{label} must resolve to an array"))),
    };
    if values.len() > MAX_POLYGON_POINTS.saturating_mul(2) {
        return Err(Error::Invalid(format!(
            "{label} exceeds the {MAX_POLYGON_POINTS}-point limit"
        )));
    }
    if values
        .iter()
        .all(|value| matches!(value, ResolvedValue::Number(_)))
    {
        if values.len() % 2 != 0 {
            return Err(Error::Invalid(format!(
                "{label} must contain x/y coordinate pairs"
            )));
        }
        return values
            .chunks_exact(2)
            .enumerate()
            .map(|(index, pair)| {
                Ok([
                    number(&pair[0], &format!("{label}[{}]", index * 2))?,
                    number(&pair[1], &format!("{label}[{}]", index * 2 + 1))?,
                ])
            })
            .collect();
    }
    if values.len() > MAX_POLYGON_POINTS {
        return Err(Error::Invalid(format!(
            "{label} exceeds the {MAX_POLYGON_POINTS}-point limit"
        )));
    }
    values
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let point = point.as_object().ok_or_else(|| {
                Error::Invalid(format!("{label}[{index}] must resolve to a point object"))
            })?;
            let x = point
                .get("x")
                .ok_or_else(|| Error::Invalid(format!("{label}[{index}] lacks x")))?;
            let y = point
                .get("y")
                .ok_or_else(|| Error::Invalid(format!("{label}[{index}] lacks y")))?;
            Ok([
                number(x, &format!("{label}[{index}].x"))?,
                number(y, &format!("{label}[{index}].y"))?,
            ])
        })
        .collect()
}

fn number(value: &ResolvedValue, label: &str) -> Result<f64> {
    match value {
        ResolvedValue::Number(value) if value.is_finite() => Ok(*value),
        _ => Err(Error::Invalid(format!("{label} must resolve to a number"))),
    }
}

fn optional_number(value: Option<&ResolvedValue>, default: f64, label: &str) -> Result<f64> {
    match value {
        None | Some(ResolvedValue::Undefined) => Ok(default),
        Some(value) => number(value, label),
    }
}

fn optional_color(value: Option<&ResolvedValue>, default: u32, label: &str) -> Result<u32> {
    match value {
        None | Some(ResolvedValue::Undefined) => Ok(default),
        Some(ResolvedValue::Number(value))
            if value.is_finite() && (0.0..=16_777_215.0).contains(value) =>
        {
            Ok(*value as u32)
        }
        Some(ResolvedValue::Number(_)) => Err(Error::Invalid(format!(
            "{label} must be between 0 and 16777215"
        ))),
        Some(_) => Err(Error::Invalid(format!("{label} must resolve to a number"))),
    }
}

fn nonnegative(value: f64, label: &str) -> Result<()> {
    if value < 0.0 {
        return Err(Error::Invalid(format!("{label} cannot be negative")));
    }
    Ok(())
}

fn exactly(params: &[ResolvedValue], expected: usize, index: usize, method: &str) -> Result<()> {
    between(params, expected, expected, index, method)
}

fn at_most(params: &[ResolvedValue], maximum: usize, index: usize, method: &str) -> Result<()> {
    between(params, 0, maximum, index, method)
}

fn between(
    params: &[ResolvedValue],
    minimum: usize,
    maximum: usize,
    index: usize,
    method: &str,
) -> Result<()> {
    if (minimum..=maximum).contains(&params.len()) {
        return Ok(());
    }
    let expected = if minimum == maximum {
        minimum.to_string()
    } else {
        format!("{minimum} to {maximum}")
    };
    Err(Error::Invalid(format!(
        "draw command {index} {method} requires {expected} parameters, got {}",
        params.len()
    )))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        RETAINED_DRAW_METHODS, VectorCommand, VectorFillStyle, VectorLineStyle, VectorProgram,
        site_progress_program,
    };
    use crate::ResolvedValue;

    fn number(value: f64) -> ResolvedValue {
        ResolvedValue::Number(value)
    }

    fn drawing(method: &str, params: Vec<ResolvedValue>) -> ResolvedValue {
        ResolvedValue::Object(BTreeMap::from([
            (
                "method".to_owned(),
                ResolvedValue::String(method.to_owned()),
            ),
            ("params".to_owned(), ResolvedValue::Array(params)),
        ]))
    }

    fn payload(drawings: Vec<ResolvedValue>) -> ResolvedValue {
        ResolvedValue::Object(BTreeMap::from([(
            "drawings".to_owned(),
            ResolvedValue::Array(drawings),
        )]))
    }

    #[test]
    fn compiles_every_retained_draw_method_into_typed_commands() {
        let program = VectorProgram::from_draw_payload(&payload(vec![
            drawing("lineStyle", vec![number(7.0), number(0xcc_cc_cc as f64)]),
            drawing("beginFill", vec![number(0x22_22_22 as f64), number(0.5)]),
            drawing("drawCircle", vec![number(1.0), number(2.0), number(75.0)]),
            drawing(
                "drawEllipse",
                vec![number(3.0), number(4.0), number(45.0), number(40.0)],
            ),
            drawing(
                "drawPolygon",
                vec![ResolvedValue::Array(vec![
                    number(-1.0),
                    number(0.0),
                    number(0.0),
                    number(-2.0),
                    number(1.0),
                    number(0.0),
                ])],
            ),
            drawing(
                "drawRect",
                vec![number(-50.0), number(-50.0), number(100.0), number(100.0)],
            ),
            drawing(
                "drawRoundedRect",
                vec![
                    number(-20.0),
                    number(-20.0),
                    number(40.0),
                    number(40.0),
                    number(15.0),
                ],
            ),
            drawing(
                "arc",
                vec![
                    number(0.0),
                    number(0.0),
                    number(50.0),
                    number(-std::f64::consts::FRAC_PI_2),
                    number(std::f64::consts::PI),
                ],
            ),
            drawing("endFill", vec![]),
        ]))
        .unwrap();

        assert_eq!(program.commands.len(), RETAINED_DRAW_METHODS.len());
        assert_eq!(
            program.commands[0],
            VectorCommand::LineStyle(VectorLineStyle {
                width: 7.0,
                color: 0xcc_cc_cc,
                ..VectorLineStyle::default()
            })
        );
        assert_eq!(
            program.commands[1],
            VectorCommand::BeginFill(VectorFillStyle {
                color: 0x22_22_22,
                alpha: 0.5,
            })
        );
        assert!(matches!(
            &program.commands[4],
            VectorCommand::Polygon { points } if points.len() == 3
        ));
        assert!(matches!(
            program.commands[7],
            VectorCommand::Arc {
                anticlockwise: false,
                ..
            }
        ));
        assert_eq!(program.commands[8], VectorCommand::EndFill);
    }

    #[test]
    fn rejects_unknown_methods_malformed_points_and_unbounded_numbers() {
        assert!(
            VectorProgram::from_draw_payload(&payload(vec![drawing("clear", vec![])])).is_err()
        );
        assert!(
            VectorProgram::from_draw_payload(&payload(vec![drawing(
                "drawPolygon",
                vec![ResolvedValue::Array(vec![
                    number(0.0),
                    number(1.0),
                    number(2.0)
                ])]
            )]))
            .is_err()
        );
        assert!(
            VectorProgram::from_draw_payload(&payload(vec![drawing(
                "drawCircle",
                vec![number(0.0), number(0.0), number(f64::INFINITY)]
            )]))
            .is_err()
        );
    }

    #[test]
    fn retains_javascript_double_precision_until_geometry_lowering() {
        let radius = 16_777_217.0;
        let program = VectorProgram::from_draw_payload(&payload(vec![drawing(
            "drawCircle",
            vec![number(0.0), number(0.0), number(radius)],
        )]))
        .unwrap();
        assert!(matches!(
            program.commands[0],
            VectorCommand::Circle {
                radius: actual,
                ..
            } if actual == radius
        ));
    }

    #[test]
    fn applies_pixi_defaults_and_rejects_out_of_range_colors() {
        let program = VectorProgram::from_draw_payload(&payload(vec![
            drawing("lineStyle", vec![]),
            drawing("beginFill", vec![number(0x00ff_ffff as f64)]),
        ]))
        .unwrap();
        assert_eq!(
            program.commands,
            [
                VectorCommand::LineStyle(VectorLineStyle::default()),
                VectorCommand::BeginFill(VectorFillStyle {
                    color: 0x00ff_ffff,
                    alpha: 1.0,
                }),
            ]
        );
        assert!(
            VectorProgram::from_draw_payload(&payload(vec![drawing(
                "beginFill",
                vec![number(-1.0)]
            )]))
            .is_err()
        );
    }

    #[test]
    fn site_progress_builds_the_official_ring_and_clamped_wedge() {
        let payload = ResolvedValue::Object(BTreeMap::from([
            ("color".to_owned(), number(0x12_34_56 as f64)),
            ("lineWidth".to_owned(), number(10.0)),
            ("progress".to_owned(), number(150.0)),
            ("progressTotal".to_owned(), number(100.0)),
            ("radius".to_owned(), number(20.0)),
        ]));
        let program = site_progress_program(&payload).unwrap();

        assert_eq!(program.commands.len(), 10);
        assert_eq!(
            program.commands[2],
            VectorCommand::Circle {
                center: [0.0, 0.0],
                radius: 25.0,
            }
        );
        assert!(matches!(
            program.commands[7],
            VectorCommand::Arc {
                start_angle: 0.0,
                end_angle,
                ..
            } if end_angle == std::f64::consts::TAU
        ));

        let mut empty = payload.as_object().unwrap().clone();
        empty.insert("progress".to_owned(), number(0.0));
        assert_eq!(
            site_progress_program(&ResolvedValue::Object(empty))
                .unwrap()
                .commands
                .len(),
            3
        );
    }
}
