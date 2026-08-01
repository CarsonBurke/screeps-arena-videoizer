use bytemuck::{Pod, Zeroable};
use sha2::{Digest, Sha256};

use crate::{Error, Result, VectorCommand, VectorFillStyle, VectorLineStyle, VectorProgram};

const CLOSE_POINT_EPSILON: f64 = 1.0e-4;
const MAX_ARC_SEGMENTS: usize = 2_048;
const MAX_VECTOR_VERTICES: usize = 1_048_576;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct VectorVertex {
    pub position: [f32; 2],
    /// Unpremultiplied style RGB and style alpha. Object tint and world alpha
    /// remain dynamic instance properties and are applied by the GPU shader.
    pub color_alpha: [f32; 4],
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VectorGeometryId([u8; 32]);

#[derive(Clone, Debug, PartialEq)]
pub struct VectorMesh {
    /// Non-indexed triangles in Pixi primitive order: each shape's fill first,
    /// followed by its stroke.
    vertices: Vec<VectorVertex>,
    geometry_id: VectorGeometryId,
}

impl Default for VectorMesh {
    fn default() -> Self {
        let mut mesh = Self {
            vertices: Vec::new(),
            geometry_id: VectorGeometryId([0; 32]),
        };
        mesh.refresh_geometry_id();
        mesh
    }
}

impl VectorMesh {
    pub fn vertices(&self) -> &[VectorVertex] {
        &self.vertices
    }

    pub const fn geometry_id(&self) -> VectorGeometryId {
        self.geometry_id
    }

    fn refresh_geometry_id(&mut self) {
        let mut hasher = Sha256::new();
        hasher.update(b"screeps-arena-vector-mesh-v1");
        hasher.update((self.vertices.len() as u64).to_le_bytes());
        hasher.update(bytemuck::cast_slice(&self.vertices));
        self.geometry_id = VectorGeometryId(hasher.finalize().into());
    }
}

#[derive(Clone, Copy)]
struct Style {
    color: u32,
    alpha: f64,
}

impl From<VectorFillStyle> for Style {
    fn from(value: VectorFillStyle) -> Self {
        Self {
            color: value.color,
            alpha: value.alpha,
        }
    }
}

impl From<VectorLineStyle> for Style {
    fn from(value: VectorLineStyle) -> Self {
        Self {
            color: value.color,
            alpha: value.alpha,
        }
    }
}

struct Tessellator {
    mesh: VectorMesh,
    fill: Option<VectorFillStyle>,
    line: Option<VectorLineStyle>,
    current_path: Option<Vec<[f64; 2]>>,
}

pub fn tessellate_vector_program(program: &VectorProgram) -> Result<VectorMesh> {
    let mut tessellator = Tessellator {
        mesh: VectorMesh::default(),
        fill: None,
        line: None,
        current_path: None,
    };
    for command in &program.commands {
        tessellator.command(command)?;
    }
    tessellator.finish_path()?;
    tessellator.mesh.refresh_geometry_id();
    Ok(tessellator.mesh)
}

impl Tessellator {
    fn command(&mut self, command: &VectorCommand) -> Result<()> {
        match command {
            VectorCommand::BeginFill(style) => {
                self.start_path()?;
                self.fill = (style.alpha > 0.0).then_some(*style);
            }
            VectorCommand::EndFill => {
                self.finish_path()?;
                self.fill = None;
            }
            VectorCommand::LineStyle(style) => {
                self.start_path()?;
                if style.native && style.width > 0.0 && style.alpha > 0.0 {
                    return Err(Error::Invalid(
                        "native Pixi Graphics lines require a line-list vector pipeline".to_owned(),
                    ));
                }
                self.line = (style.width > 0.0 && style.alpha > 0.0).then_some(*style);
            }
            VectorCommand::Arc {
                center,
                radius,
                start_angle,
                end_angle,
                anticlockwise,
            } => self.arc(*center, *radius, *start_angle, *end_angle, *anticlockwise)?,
            VectorCommand::Circle { center, radius } => {
                let points = circle_points(*center, [*radius, *radius], [0.0, 0.0])?;
                self.draw_shape(&points, true, ShapeFill::Radial(*center))?;
            }
            VectorCommand::Ellipse { center, half_size } => {
                let points = circle_points(*center, *half_size, [0.0, 0.0])?;
                self.draw_shape(&points, true, ShapeFill::Radial(*center))?;
            }
            VectorCommand::Polygon { points } => {
                self.draw_shape(points, true, ShapeFill::Polygon)?;
            }
            VectorCommand::Rect { origin, size } => {
                let points = rectangle_points(*origin, *size);
                self.draw_shape(&points, true, ShapeFill::Rectangle)?;
            }
            VectorCommand::RoundedRect {
                origin,
                size,
                radius,
            } => {
                let half_size = [size[0] / 2.0, size[1] / 2.0];
                let center = [origin[0] + half_size[0], origin[1] + half_size[1]];
                let radius = radius.max(0.0).min(half_size[0].min(half_size[1]));
                let points = circle_points(
                    center,
                    [radius, radius],
                    [half_size[0] - radius, half_size[1] - radius],
                )?;
                self.draw_shape(&points, true, ShapeFill::Radial(center))?;
            }
            VectorCommand::MoveTo(point) => {
                self.start_path()?;
                self.current_path = Some(vec![*point]);
            }
            VectorCommand::LineTo(point) => {
                if self.current_path.is_none() {
                    self.current_path = Some(vec![[0.0, 0.0]]);
                }
                let path = self.current_path.as_mut().expect("path was initialized");
                if path.last() != Some(point) {
                    path.push(*point);
                }
            }
        }
        Ok(())
    }

    /// Pixi's `startPoly`: style changes and moveTo flush the current path,
    /// then retain its last point as the beginning of the next path.
    fn start_path(&mut self) -> Result<()> {
        let Some(path) = self.current_path.take() else {
            self.current_path = Some(Vec::new());
            return Ok(());
        };
        if path.len() > 1 {
            let last = *path.last().expect("nonempty path");
            self.draw_shape(&path, false, ShapeFill::Polygon)?;
            self.current_path = Some(vec![last]);
        } else {
            self.current_path = Some(path);
        }
        Ok(())
    }

    fn finish_path(&mut self) -> Result<()> {
        let Some(path) = self.current_path.take() else {
            return Ok(());
        };
        if path.len() > 1 {
            self.draw_shape(&path, false, ShapeFill::Polygon)?;
        }
        Ok(())
    }

    fn arc(
        &mut self,
        center: [f64; 2],
        radius: f64,
        mut start: f64,
        mut end: f64,
        anticlockwise: bool,
    ) -> Result<()> {
        if start == end {
            return Ok(());
        }
        if !anticlockwise && end <= start {
            end += std::f64::consts::TAU;
        } else if anticlockwise && start <= end {
            start += std::f64::consts::TAU;
        }
        let sweep = end - start;
        if sweep == 0.0 {
            return Ok(());
        }
        let start_point = [
            center[0] + start.cos() * radius,
            center[1] + start.sin() * radius,
        ];
        if let Some(path) = self.current_path.as_mut() {
            let append = path.last().is_none_or(|last| {
                (last[0] - start_point[0]).abs() >= CLOSE_POINT_EPSILON
                    || (last[1] - start_point[1]).abs() >= CLOSE_POINT_EPSILON
            });
            if append {
                path.push(start_point);
            }
        } else {
            self.current_path = Some(vec![start_point]);
        }
        let segment_count = arc_segment_count(sweep.abs() * radius, sweep.abs());
        let path = self.current_path.as_mut().expect("arc initialized a path");
        path.reserve(segment_count);
        for segment in 1..=segment_count {
            let angle = start + sweep * segment as f64 / segment_count as f64;
            path.push([
                center[0] + angle.cos() * radius,
                center[1] + angle.sin() * radius,
            ]);
        }
        Ok(())
    }

    fn draw_shape(
        &mut self,
        points: &[[f64; 2]],
        closed_stroke: bool,
        fill_kind: ShapeFill,
    ) -> Result<()> {
        if let Some(fill) = self.fill {
            self.fill_shape(points, fill_kind, fill.into())?;
        }
        if let Some(line) = self.line {
            self.stroke_shape(points, closed_stroke, line)?;
        }
        Ok(())
    }

    fn fill_shape(&mut self, points: &[[f64; 2]], kind: ShapeFill, style: Style) -> Result<()> {
        if points.len() < 3 {
            return Ok(());
        }
        match kind {
            ShapeFill::Radial(center) => {
                for index in 1..points.len() {
                    self.push_triangle(points[index - 1], center, points[index], style)?;
                }
                self.push_triangle(points[0], center, points[points.len() - 1], style)
            }
            ShapeFill::Rectangle if points.len() == 4 => {
                self.push_triangle(points[0], points[1], points[3], style)?;
                self.push_triangle(points[1], points[3], points[2], style)
            }
            ShapeFill::Polygon | ShapeFill::Rectangle => {
                let mut points = points.to_vec();
                fix_outer_orientation(&mut points);
                let coordinates = points
                    .iter()
                    .flat_map(|point| point.iter().copied())
                    .collect::<Vec<_>>();
                let indices = earcutr::earcut(&coordinates, &[], 2).map_err(|error| {
                    Error::Invalid(format!("vector polygon triangulation failed: {error}"))
                })?;
                for triangle in indices.chunks_exact(3) {
                    self.push_triangle(
                        points[triangle[0]],
                        points[triangle[1]],
                        points[triangle[2]],
                        style,
                    )?;
                }
                Ok(())
            }
        }
    }

    fn stroke_shape(
        &mut self,
        points: &[[f64; 2]],
        closed_shape: bool,
        line: VectorLineStyle,
    ) -> Result<()> {
        if points.len() < 2 {
            return Ok(());
        }
        let strip = pixi_line_strip(points, closed_shape, line)?;
        let style = Style::from(line);
        let epsilon_squared = CLOSE_POINT_EPSILON * CLOSE_POINT_EPSILON;
        for triangle in strip.windows(3) {
            let area = triangle[0][0] * (triangle[1][1] - triangle[2][1])
                + triangle[1][0] * (triangle[2][1] - triangle[0][1])
                + triangle[2][0] * (triangle[0][1] - triangle[1][1]);
            if area.abs() >= epsilon_squared {
                self.push_triangle(triangle[0], triangle[1], triangle[2], style)?;
            }
        }
        Ok(())
    }

    fn push_triangle(
        &mut self,
        first: [f64; 2],
        second: [f64; 2],
        third: [f64; 2],
        style: Style,
    ) -> Result<()> {
        if self.mesh.vertices.len() > MAX_VECTOR_VERTICES - 3 {
            return Err(Error::Invalid(format!(
                "vector program exceeds the {MAX_VECTOR_VERTICES}-vertex tessellation limit"
            )));
        }
        let red = f64::from((style.color >> 16) & 0xff) / 255.0;
        let green = f64::from((style.color >> 8) & 0xff) / 255.0;
        let blue = f64::from(style.color & 0xff) / 255.0;
        let color_alpha = [red as f32, green as f32, blue as f32, style.alpha as f32];
        self.mesh
            .vertices
            .extend([first, second, third].map(|position| VectorVertex {
                position: [position[0] as f32, position[1] as f32],
                color_alpha,
            }));
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ShapeFill {
    Radial([f64; 2]),
    Rectangle,
    Polygon,
}

fn arc_segment_count(length: f64, sweep: f64) -> usize {
    let default = (sweep / std::f64::consts::TAU).ceil() as usize * 40;
    if length == 0.0 || length.is_nan() {
        return default;
    }
    ((length / 10.0).ceil() as usize).clamp(8, MAX_ARC_SEGMENTS)
}

fn rectangle_points(origin: [f64; 2], size: [f64; 2]) -> Vec<[f64; 2]> {
    if size[0] < 0.0 || size[1] < 0.0 {
        return Vec::new();
    }
    vec![
        origin,
        [origin[0] + size[0], origin[1]],
        [origin[0] + size[0], origin[1] + size[1]],
        [origin[0], origin[1] + size[1]],
    ]
}

/// Exact point order and segment rule from Pixi 7's `buildCircle`.
fn circle_points(center: [f64; 2], radius: [f64; 2], straight: [f64; 2]) -> Result<Vec<[f64; 2]>> {
    let [x, y] = center;
    let [rx, ry] = radius;
    let [dx, dy] = straight;
    if !(rx >= 0.0 && ry >= 0.0 && dx >= 0.0 && dy >= 0.0) {
        return Ok(Vec::new());
    }
    let n = (2.3 * (rx + ry).sqrt()).ceil() as usize;
    let coordinate_count = n
        .checked_mul(8)
        .and_then(|count| count.checked_add(if dx != 0.0 { 4 } else { 0 }))
        .and_then(|count| count.checked_add(if dy != 0.0 { 4 } else { 0 }))
        .ok_or(Error::ArithmeticOverflow)?;
    if coordinate_count == 0 {
        return Ok(Vec::new());
    }
    if coordinate_count / 2 > MAX_VECTOR_VERTICES {
        return Err(Error::Invalid(
            "vector primitive exceeds the tessellation point limit".to_owned(),
        ));
    }
    if n == 0 {
        return Ok(vec![
            [x + dx, y + dy],
            [x - dx, y + dy],
            [x - dx, y - dy],
            [x + dx, y - dy],
        ]);
    }

    let mut coordinates = vec![0.0; coordinate_count];
    let mut first_quadrant = 0;
    let mut second_quadrant = n * 4 + if dx != 0.0 { 2 } else { 0 } + 2;
    let mut third_quadrant = second_quadrant;
    let mut fourth_quadrant = coordinate_count;
    coordinates[first_quadrant] = x + dx + rx;
    first_quadrant += 1;
    coordinates[first_quadrant] = y + dy;
    first_quadrant += 1;
    second_quadrant -= 1;
    coordinates[second_quadrant] = y + dy;
    second_quadrant -= 1;
    coordinates[second_quadrant] = x - dx - rx;
    if dy != 0.0 {
        coordinates[third_quadrant] = x - dx - rx;
        third_quadrant += 1;
        coordinates[third_quadrant] = y - dy;
        third_quadrant += 1;
        fourth_quadrant -= 1;
        coordinates[fourth_quadrant] = y - dy;
        fourth_quadrant -= 1;
        coordinates[fourth_quadrant] = x + dx + rx;
    }
    for index in 1..n {
        let angle = std::f64::consts::FRAC_PI_2 * index as f64 / n as f64;
        let x_offset = dx + angle.cos() * rx;
        let y_offset = dy + angle.sin() * ry;
        coordinates[first_quadrant] = x + x_offset;
        first_quadrant += 1;
        coordinates[first_quadrant] = y + y_offset;
        first_quadrant += 1;
        second_quadrant -= 1;
        coordinates[second_quadrant] = y + y_offset;
        second_quadrant -= 1;
        coordinates[second_quadrant] = x - x_offset;
        coordinates[third_quadrant] = x - x_offset;
        third_quadrant += 1;
        coordinates[third_quadrant] = y - y_offset;
        third_quadrant += 1;
        fourth_quadrant -= 1;
        coordinates[fourth_quadrant] = y - y_offset;
        fourth_quadrant -= 1;
        coordinates[fourth_quadrant] = x + x_offset;
    }
    coordinates[first_quadrant] = x + dx;
    first_quadrant += 1;
    coordinates[first_quadrant] = y + dy + ry;
    first_quadrant += 1;
    fourth_quadrant -= 1;
    coordinates[fourth_quadrant] = y - dy - ry;
    fourth_quadrant -= 1;
    coordinates[fourth_quadrant] = x + dx;
    if dx != 0.0 {
        coordinates[first_quadrant] = x - dx;
        first_quadrant += 1;
        coordinates[first_quadrant] = y + dy + ry;
        fourth_quadrant -= 1;
        coordinates[fourth_quadrant] = y - dy - ry;
        fourth_quadrant -= 1;
        coordinates[fourth_quadrant] = x - dx;
    }
    debug_assert_eq!(first_quadrant, second_quadrant);
    debug_assert_eq!(third_quadrant, fourth_quadrant);
    Ok(coordinates
        .chunks_exact(2)
        .map(|point| [point[0], point[1]])
        .collect())
}

fn fix_outer_orientation(points: &mut [[f64; 2]]) {
    if points.len() < 3 {
        return;
    }
    let mut area = 0.0;
    let mut previous = points[points.len() - 1];
    for point in points.iter() {
        area += (point[0] - previous[0]) * (point[1] + previous[1]);
        previous = *point;
    }
    if area > 0.0 {
        points.reverse();
    }
}

/// Port of Pixi 7's default non-native, butt-cap, miter-join line builder.
fn pixi_line_strip(
    source: &[[f64; 2]],
    closed_shape: bool,
    style: VectorLineStyle,
) -> Result<Vec<[f64; 2]>> {
    let mut points = source.to_vec();
    if closed_shape {
        if points.len() > 1
            && (points[0][0] - points[points.len() - 1][0]).abs() < CLOSE_POINT_EPSILON
            && (points[0][1] - points[points.len() - 1][1]).abs() < CLOSE_POINT_EPSILON
        {
            points.pop();
        }
        if points.len() < 2 {
            return Ok(Vec::new());
        }
        let midpoint = [
            (points[0][0] + points[points.len() - 1][0]) * 0.5,
            (points[0][1] + points[points.len() - 1][1]) * 0.5,
        ];
        points.insert(0, midpoint);
        points.push(midpoint);
    }
    if points.len() < 2 {
        return Ok(Vec::new());
    }
    let width = style.width / 2.0;
    let width_squared = width * width;
    let inner_weight = (1.0 - style.alignment) * 2.0;
    let outer_weight = style.alignment * 2.0;
    let mut vertices = Vec::with_capacity(points.len() * 4);
    let mut perpendicular = segment_perpendicular(points[0], points[1], width);
    push_pair(
        &mut vertices,
        points[0],
        perpendicular,
        inner_weight,
        outer_weight,
    );

    for index in 1..points.len() - 1 {
        let previous = points[index - 1];
        let current = points[index];
        let next = points[index + 1];
        perpendicular = segment_perpendicular(previous, current, width);
        let next_perpendicular = segment_perpendicular(current, next, width);
        let dx0 = current[0] - previous[0];
        let dy0 = previous[1] - current[1];
        let dx1 = current[0] - next[0];
        let dy1 = next[1] - current[1];
        let dot = dx0 * dx1 + dy0 * dy1;
        let cross = dy0 * dx1 - dy1 * dx0;
        let clockwise = cross < 0.0;

        if cross.abs() < 1.0e-3 * dot.abs() {
            push_pair(
                &mut vertices,
                current,
                perpendicular,
                inner_weight,
                outer_weight,
            );
            if dot >= 0.0 {
                vertices.push([
                    current[0] - next_perpendicular[0] * outer_weight,
                    current[1] - next_perpendicular[1] * outer_weight,
                ]);
                vertices.push([
                    current[0] + next_perpendicular[0] * inner_weight,
                    current[1] + next_perpendicular[1] * inner_weight,
                ]);
            }
            continue;
        }

        let c1 = (-perpendicular[0] + previous[0]) * (-perpendicular[1] + current[1])
            - (-perpendicular[0] + current[0]) * (-perpendicular[1] + previous[1]);
        let c2 = (-next_perpendicular[0] + next[0]) * (-next_perpendicular[1] + current[1])
            - (-next_perpendicular[0] + current[0]) * (-next_perpendicular[1] + next[1]);
        let px = (dx0 * c2 - dx1 * c1) / cross;
        let py = (dy1 * c1 - dy0 * c2) / cross;
        let distance_squared = (px - current[0]).powi(2) + (py - current[1]).powi(2);
        let inner_miter = [
            current[0] + (px - current[0]) * inner_weight,
            current[1] + (py - current[1]) * inner_weight,
        ];
        let outer_miter = [
            current[0] - (px - current[0]) * outer_weight,
            current[1] - (py - current[1]) * outer_weight,
        ];
        let smaller_segment_squared = (dx0 * dx0 + dy0 * dy0).min(dx1 * dx1 + dy1 * dy1);
        let inside_weight = if clockwise {
            inner_weight
        } else {
            outer_weight
        };
        let inside_diagonal_squared =
            smaller_segment_squared + inside_weight * inside_weight * width_squared;
        let miter_within_segment = distance_squared <= inside_diagonal_squared;
        let miter_within_limit = width_squared == 0.0 || distance_squared / width_squared <= 100.0;

        if miter_within_segment && miter_within_limit {
            vertices.push(inner_miter);
            vertices.push(outer_miter);
        } else {
            push_pair(
                &mut vertices,
                current,
                perpendicular,
                inner_weight,
                outer_weight,
            );
            let repeated = if clockwise { outer_miter } else { inner_miter };
            vertices.push(repeated);
            vertices.push(repeated);
            push_pair(
                &mut vertices,
                current,
                next_perpendicular,
                inner_weight,
                outer_weight,
            );
        }
    }
    perpendicular =
        segment_perpendicular(points[points.len() - 2], points[points.len() - 1], width);
    push_pair(
        &mut vertices,
        points[points.len() - 1],
        perpendicular,
        inner_weight,
        outer_weight,
    );
    Ok(vertices)
}

fn segment_perpendicular(from: [f64; 2], to: [f64; 2], width: f64) -> [f64; 2] {
    let mut perpendicular = [to[1] - from[1], from[0] - to[0]];
    let length = perpendicular[0].hypot(perpendicular[1]);
    // Pixi performs this unchecked division. Degenerate spans become NaN and
    // only triangles touching them fail its area test; valid prefix/suffix
    // spans remain drawable. NaNs stay in this temporary strip and never reach
    // `push_triangle`.
    perpendicular[0] = perpendicular[0] / length * width;
    perpendicular[1] = perpendicular[1] / length * width;
    perpendicular
}

fn push_pair(
    vertices: &mut Vec<[f64; 2]>,
    point: [f64; 2],
    perpendicular: [f64; 2],
    inner_weight: f64,
    outer_weight: f64,
) {
    vertices.push([
        point[0] - perpendicular[0] * inner_weight,
        point[1] - perpendicular[1] * inner_weight,
    ]);
    vertices.push([
        point[0] + perpendicular[0] * outer_weight,
        point[1] + perpendicular[1] * outer_weight,
    ]);
}

#[cfg(test)]
mod tests {
    use super::{VectorMesh, arc_segment_count, tessellate_vector_program};
    use crate::{
        ResolvedValue, VectorCommand, VectorFillStyle, VectorLineStyle, VectorProgram,
        site_progress_program,
    };
    use std::collections::BTreeMap;

    fn triangle_area(mesh: &VectorMesh) -> f64 {
        mesh.vertices
            .chunks_exact(3)
            .map(|triangle| {
                let [a, b, c] = [
                    triangle[0].position,
                    triangle[1].position,
                    triangle[2].position,
                ];
                f64::from(
                    ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs() / 2.0,
                )
            })
            .sum()
    }

    #[test]
    fn geometry_identity_is_content_stable_and_shape_sensitive() {
        let program = |width| VectorProgram {
            commands: vec![
                VectorCommand::BeginFill(VectorFillStyle {
                    color: 0x12_34_56,
                    alpha: 0.75,
                }),
                VectorCommand::Rect {
                    origin: [1.0, 2.0],
                    size: [width, 4.0],
                },
            ],
        };
        let first = tessellate_vector_program(&program(3.0)).unwrap();
        let same = tessellate_vector_program(&program(3.0)).unwrap();
        let different = tessellate_vector_program(&program(3.5)).unwrap();
        assert_eq!(first.geometry_id(), same.geometry_id());
        assert_ne!(first.geometry_id(), different.geometry_id());
    }

    #[test]
    fn arc_segmentation_matches_pixis_adaptive_bounds() {
        assert_eq!(arc_segment_count(1.0, 0.01), 8);
        assert_eq!(arc_segment_count(81.0, 1.0), 9);
        assert_eq!(arc_segment_count(50_000.0, 10.0), 2_048);
        assert_eq!(arc_segment_count(0.0, std::f64::consts::TAU), 40);
    }

    #[test]
    fn fills_concave_polygons_with_earcut_and_preserves_style_color() {
        let program = VectorProgram {
            commands: vec![
                VectorCommand::BeginFill(VectorFillStyle {
                    color: 0x12_34_56,
                    alpha: 0.5,
                }),
                VectorCommand::Polygon {
                    points: vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [2.0, 2.0], [0.0, 4.0]],
                },
            ],
        };
        let mesh = tessellate_vector_program(&program).unwrap();
        assert_eq!(mesh.vertices.len(), 9);
        assert_eq!(triangle_area(&mesh), 12.0);
        assert_eq!(
            mesh.vertices[0].color_alpha,
            [
                0x12 as f32 / 255.0,
                0x34 as f32 / 255.0,
                0x56 as f32 / 255.0,
                0.5
            ]
        );
    }

    #[test]
    fn style_changes_flush_open_paths_from_the_last_point() {
        let program = VectorProgram {
            commands: vec![
                VectorCommand::LineStyle(VectorLineStyle {
                    width: 2.0,
                    color: 0xff_00_00,
                    ..VectorLineStyle::default()
                }),
                VectorCommand::MoveTo([0.0, 0.0]),
                VectorCommand::LineTo([10.0, 0.0]),
                VectorCommand::LineStyle(VectorLineStyle {
                    width: 4.0,
                    color: 0x00_ff_00,
                    ..VectorLineStyle::default()
                }),
                VectorCommand::LineTo([10.0, 10.0]),
            ],
        };
        let mesh = tessellate_vector_program(&program).unwrap();
        assert!(!mesh.vertices.is_empty());
        assert!(
            mesh.vertices
                .iter()
                .any(|vertex| vertex.color_alpha[0] == 1.0)
        );
        assert!(
            mesh.vertices
                .iter()
                .any(|vertex| vertex.color_alpha[1] == 1.0)
        );
    }

    #[test]
    fn site_progress_tessellates_ring_and_wedge_without_atlas_assets() {
        let payload = ResolvedValue::Object(BTreeMap::from([
            ("progress".to_owned(), ResolvedValue::Number(25.0)),
            ("progressTotal".to_owned(), ResolvedValue::Number(100.0)),
            ("color".to_owned(), ResolvedValue::Number(0xaa_bb_cc as f64)),
            ("radius".to_owned(), ResolvedValue::Number(10.0)),
            ("lineWidth".to_owned(), ResolvedValue::Number(2.0)),
        ]));
        let program = site_progress_program(&payload).unwrap();
        let mesh = tessellate_vector_program(&program).unwrap();
        assert!(!mesh.vertices.is_empty());
        assert!(mesh.vertices.iter().all(|vertex| {
            vertex.color_alpha[..3]
                == [
                    0xaa as f32 / 255.0,
                    0xbb as f32 / 255.0,
                    0xcc as f32 / 255.0,
                ]
        }));
        assert!(triangle_area(&mesh) > 100.0);
    }

    #[test]
    fn negative_rectangles_match_pixis_empty_geometry_and_native_lines_fail_closed() {
        let negative = VectorProgram {
            commands: vec![
                VectorCommand::BeginFill(VectorFillStyle {
                    color: 0xff_ff_ff,
                    alpha: 1.0,
                }),
                VectorCommand::Rect {
                    origin: [0.0, 0.0],
                    size: [-1.0, 4.0],
                },
            ],
        };
        assert!(
            tessellate_vector_program(&negative)
                .unwrap()
                .vertices
                .is_empty()
        );

        let native = VectorProgram {
            commands: vec![VectorCommand::LineStyle(VectorLineStyle {
                width: 1.0,
                native: true,
                ..VectorLineStyle::default()
            })],
        };
        assert!(tessellate_vector_program(&native).is_err());
    }

    #[test]
    fn degenerate_stroke_points_drop_only_adjacent_nan_triangles() {
        let program = VectorProgram {
            commands: vec![
                VectorCommand::LineStyle(VectorLineStyle {
                    width: 2.0,
                    color: 0xff_ff_ff,
                    ..VectorLineStyle::default()
                }),
                VectorCommand::Polygon {
                    points: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 0.0], [10.0, 10.0]],
                },
            ],
        };
        let mesh = tessellate_vector_program(&program).unwrap();
        assert!(!mesh.vertices.is_empty());
        assert!(mesh.vertices.iter().all(|vertex| {
            vertex.position.iter().all(|value| value.is_finite())
                && vertex.color_alpha.iter().all(|value| value.is_finite())
        }));
    }
}
