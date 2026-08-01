use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Entity, Error, FrameSample, ReplayArtifact, Result, Timeline, Track, TrackValue};

const RENDERER_TERRAIN_CELL_SIZE: u32 = 100;
const MAX_SUPPORTED_ROOM_SIZE: u32 = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainSwampTexture {
    Disabled,
    Static,
    Animated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainGeometry {
    pub room_size: u32,
    pub view_box: u32,
    pub wall_path: Option<String>,
    pub swamp_path: Option<String>,
    pub private_rampart_paths: BTreeMap<String, String>,
    pub private_rampart_colors: BTreeMap<String, u32>,
    pub swamp_texture: TerrainSwampTexture,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainGeometrySpan {
    pub start_tick: u32,
    pub end_tick: u32,
    pub fingerprint: String,
    pub swamp_animation_start_tick: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainGeometryTimeline {
    pub geometries: BTreeMap<String, TerrainGeometry>,
    pub spans: Vec<TerrainGeometrySpan>,
}

impl TerrainGeometrySpan {
    /// Exact retained Pixi ticker phase for this span at one output frame.
    pub fn swamp_phase_seconds(&self, frame: FrameSample, timeline: Timeline) -> Result<f64> {
        if frame.tick < self.start_tick || frame.tick >= self.end_tick {
            return Err(Error::Invalid(
                "terrain frame tick lies outside its geometry span".to_owned(),
            ));
        }
        let start = timeline.apply_tick_time(self.swamp_animation_start_tick)?;
        Ok(frame.time.checked_sub(start)?.as_f64())
    }
}

impl TerrainGeometryTimeline {
    pub fn span_at(&self, tick: u32) -> Option<&TerrainGeometrySpan> {
        self.spans
            .binary_search_by(|span| {
                if tick < span.start_tick {
                    std::cmp::Ordering::Greater
                } else if tick >= span.end_tick {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()
            .map(|index| &self.spans[index])
    }
}

impl TerrainGeometry {
    fn refresh_fingerprint(&mut self) {
        let mut hasher = Sha256::new();
        hasher.update(self.room_size.to_le_bytes());
        hasher.update(self.view_box.to_le_bytes());
        hasher.update([match self.swamp_texture {
            TerrainSwampTexture::Disabled => 0,
            TerrainSwampTexture::Static => 1,
            TerrainSwampTexture::Animated => 2,
        }]);
        hash_optional_path(&mut hasher, &self.wall_path);
        hash_optional_path(&mut hasher, &self.swamp_path);
        for (user, path) in &self.private_rampart_paths {
            hash_bytes(&mut hasher, user.as_bytes());
            hash_bytes(&mut hasher, path.as_bytes());
            hasher.update(self.private_rampart_colors[user].to_le_bytes());
        }
        self.fingerprint = encode_hex(hasher.finalize().as_slice());
    }
}

/// Reconstructs the official retained terrain inputs from authenticated static
/// map cells plus dynamic replay objects. The resulting paths are suitable for
/// rasterizing once per distinct geometry fingerprint and reusing across all
/// temporal views that share it.
#[derive(Debug)]
pub struct TerrainGeometryCompiler<'a> {
    artifact: &'a ReplayArtifact,
    room_size: u32,
    view_box: u32,
    raster_dimensions: [u32; 2],
    static_walls: CellGrid,
    static_swamps: CellGrid,
}

impl<'a> TerrainGeometryCompiler<'a> {
    pub fn new(artifact: &'a ReplayArtifact) -> Result<Self> {
        let options = artifact
            .renderer_contract
            .world_options
            .as_object()
            .ok_or_else(|| Error::Invalid("renderer worldOptions must be an object".to_owned()))?;
        let room_size = required_positive_u32(options.get("ROOM_SIZE"), "ROOM_SIZE")?;
        if room_size > MAX_SUPPORTED_ROOM_SIZE {
            return Err(Error::Invalid(format!(
                "renderer ROOM_SIZE {room_size} exceeds native terrain limit {MAX_SUPPORTED_ROOM_SIZE}"
            )));
        }
        let cell_size = required_positive_u32(options.get("CELL_SIZE"), "CELL_SIZE")?;
        if cell_size != RENDERER_TERRAIN_CELL_SIZE {
            return Err(Error::Invalid(format!(
                "terrain path adapter requires renderer CELL_SIZE {RENDERER_TERRAIN_CELL_SIZE}, got {cell_size}"
            )));
        }
        let default_view_box = room_size
            .checked_mul(cell_size)
            .ok_or(Error::ArithmeticOverflow)?;
        let view_box = match options.get("VIEW_BOX") {
            Some(value) => required_positive_u32(Some(value), "VIEW_BOX")?,
            None => default_view_box,
        };
        let raster_dimensions = renderer_raster_dimensions(artifact, options)?;
        let mut static_walls = CellGrid::new(room_size)?;
        let mut static_swamps = CellGrid::new(room_size)?;
        for (index, object) in artifact.renderer_contract.terrain.iter().enumerate() {
            let Some(kind) = object.get("type").and_then(Value::as_str) else {
                return Err(Error::Invalid(format!(
                    "renderer terrain object {index} lacks a string type"
                )));
            };
            let target = match kind {
                "wall" => &mut static_walls,
                "swamp" => &mut static_swamps,
                _ => continue,
            };
            let (x, y) = cell_coordinates(object, room_size, &format!("terrain object {index}"))?;
            target.insert(x, y);
        }
        Ok(Self {
            artifact,
            room_size,
            view_box,
            raster_dimensions,
            static_walls,
            static_swamps,
        })
    }

    pub fn geometry_at(&self, tick: u32) -> Result<TerrainGeometry> {
        if tick > self.artifact.replay.total_ticks {
            return Err(Error::Invalid(
                "terrain geometry tick exceeds replay endpoint".to_owned(),
            ));
        }
        let swamp_texture = swamp_texture_at(self.artifact, tick)?;
        let replacement_tick = self
            .artifact
            .replay
            .global_state
            .get("setTerrain")
            .and_then(|track| latest_truthy_tick(track, tick));
        let (mut walls, mut swamps) = if let Some(replacement_tick) = replacement_tick {
            let mut walls = CellGrid::new(self.room_size)?;
            let mut swamps = CellGrid::new(self.room_size)?;
            for entity in &self.artifact.replay.entities {
                if !entity.alive_at(replacement_tick) {
                    continue;
                }
                match entity_string(entity, "type", replacement_tick)? {
                    Some("wall") => {
                        let (x, y) = entity_cell(entity, replacement_tick, self.room_size)?;
                        walls.insert(x, y);
                    }
                    Some("swamp") => {
                        let (x, y) = entity_cell(entity, replacement_tick, self.room_size)?;
                        swamps.insert(x, y);
                    }
                    _ => {}
                }
            }
            (walls, swamps)
        } else {
            (self.static_walls.clone(), self.static_swamps.clone())
        };
        let users = global_user_colors(self.artifact, tick)?;
        let mut ramparts = users
            .keys()
            .map(|user| Ok((user.clone(), CellGrid::new(self.room_size)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;

        for entity in &self.artifact.replay.entities {
            if !entity.alive_at(tick) {
                continue;
            }
            let Some(kind) = entity_string(entity, "type", tick)? else {
                continue;
            };
            match kind {
                "wall" => {
                    let (x, y) = entity_cell(entity, tick, self.room_size)?;
                    walls.insert(x, y);
                }
                "constructedWall" if replacement_tick != Some(tick) => {
                    let (x, y) = entity_cell(entity, tick, self.room_size)?;
                    walls.insert(x, y);
                }
                "swamp" => {
                    let (x, y) = entity_cell(entity, tick, self.room_size)?;
                    swamps.insert(x, y);
                }
                "rampart" if !entity_bool(entity, "isPublic", tick)?.unwrap_or(false) => {
                    let Some(user) = entity_string(entity, "user", tick)? else {
                        continue;
                    };
                    let Some(grid) = ramparts.get_mut(user) else {
                        continue;
                    };
                    let (x, y) = entity_cell(entity, tick, self.room_size)?;
                    grid.insert(x, y);
                }
                _ => {}
            }
        }

        let wall_path = render_path(&walls, false);
        let swamp_path = render_path(&swamps, false);
        let private_rampart_paths = ramparts
            .into_iter()
            .filter_map(|(user, grid)| render_path(&grid, true).map(|path| (user, path)))
            .collect::<BTreeMap<_, _>>();
        let private_rampart_colors = private_rampart_paths
            .keys()
            .map(|user| {
                users
                    .get(user)
                    .copied()
                    .flatten()
                    .ok_or_else(|| {
                        Error::Invalid(format!(
                            "terrain user {user} with a private rampart lacks a #RRGGBB color"
                        ))
                    })
                    .map(|color| (user.clone(), color))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut geometry = TerrainGeometry {
            room_size: self.room_size,
            view_box: self.view_box,
            wall_path,
            swamp_path,
            private_rampart_paths,
            private_rampart_colors,
            swamp_texture,
            fingerprint: String::new(),
        };
        geometry.refresh_fingerprint();
        Ok(geometry)
    }

    pub const fn raster_dimensions(&self) -> [u32; 2] {
        self.raster_dimensions
    }

    /// Ticks at which terrain geometry can differ from the preceding tick.
    /// This lets callers compile static arenas once instead of rebuilding a
    /// 100×100 path for every replay state.
    pub fn geometry_change_ticks(&self) -> Vec<u32> {
        let mut ticks = BTreeSet::from([0]);
        for property in ["gameData", "setTerrain", "users"] {
            if let Some(track) = self.artifact.replay.global_state.get(property) {
                insert_track_boundaries(track, self.artifact.replay.total_ticks, &mut ticks);
            }
        }
        for entity in &self.artifact.replay.entities {
            let terrain_relevant = entity
                .properties
                .get("type")
                .is_some_and(track_contains_terrain_type);
            if !terrain_relevant {
                continue;
            }
            for [start, end] in &entity.lifetimes {
                if *start <= self.artifact.replay.total_ticks {
                    ticks.insert(*start);
                }
                if *end <= self.artifact.replay.total_ticks {
                    ticks.insert(*end);
                }
            }
            for property in ["type", "x", "y", "isPublic", "user"] {
                if let Some(track) = entity.properties.get(property) {
                    insert_track_boundaries(track, self.artifact.replay.total_ticks, &mut ticks);
                }
            }
        }
        ticks.into_iter().collect()
    }

    pub fn compile_timeline(&self) -> Result<TerrainGeometryTimeline> {
        let endpoint = self
            .artifact
            .replay
            .total_ticks
            .checked_add(1)
            .ok_or(Error::ArithmeticOverflow)?;
        let mut boundaries = self.geometry_change_ticks();
        boundaries.push(endpoint);
        let mut geometries = BTreeMap::new();
        let mut spans = Vec::<TerrainGeometrySpan>::new();
        let mut previous_requested_wall_path = None::<Option<String>>;
        let mut effective_wall_path = None::<String>;
        let mut previous_requested_swamp_path = None::<Option<String>>;
        let mut effective_swamp_path = None::<String>;
        let mut effective_swamp_texture = TerrainSwampTexture::Animated;
        let mut swamp_animation_start_tick = 0;
        let mut previous_rampart_paths = BTreeMap::<String, String>::new();
        let mut effective_rampart_colors = BTreeMap::<String, u32>::new();
        for pair in boundaries.windows(2) {
            let [start_tick, end_tick] = [pair[0], pair[1]];
            let mut geometry = self.geometry_at(start_tick)?;
            let requested_wall_path = geometry.wall_path.clone();
            let wall_path_changed = previous_requested_wall_path
                .as_ref()
                .is_none_or(|previous| previous != &requested_wall_path);
            if wall_path_changed && requested_wall_path.is_some() {
                effective_wall_path.clone_from(&requested_wall_path);
            } else {
                geometry.wall_path.clone_from(&effective_wall_path);
            }
            previous_requested_wall_path = Some(requested_wall_path);

            let requested_swamp_path = geometry.swamp_path.clone();
            let swamp_path_changed = previous_requested_swamp_path
                .as_ref()
                .is_none_or(|previous| previous != &requested_swamp_path);
            if swamp_path_changed && requested_swamp_path.is_some() {
                effective_swamp_path.clone_from(&requested_swamp_path);
                effective_swamp_texture = geometry.swamp_texture;
                swamp_animation_start_tick = start_tick;
            } else {
                geometry.swamp_path.clone_from(&effective_swamp_path);
                geometry.swamp_texture = effective_swamp_texture;
            }
            previous_requested_swamp_path = Some(requested_swamp_path);
            effective_rampart_colors
                .retain(|user, _| geometry.private_rampart_paths.contains_key(user));
            for (user, path) in &geometry.private_rampart_paths {
                let path_changed = previous_rampart_paths.get(user) != Some(path);
                if path_changed {
                    effective_rampart_colors
                        .insert(user.clone(), geometry.private_rampart_colors[user]);
                } else {
                    geometry.private_rampart_colors.insert(
                        user.clone(),
                        *effective_rampart_colors.get(user).ok_or_else(|| {
                            Error::Invalid(
                                "terrain rampart color latch lacks an existing user".to_owned(),
                            )
                        })?,
                    );
                }
            }
            previous_rampart_paths = geometry.private_rampart_paths.clone();
            geometry.refresh_fingerprint();
            let fingerprint = geometry.fingerprint.clone();
            geometries.entry(fingerprint.clone()).or_insert(geometry);
            if let Some(previous) = spans.last_mut()
                && previous.fingerprint == fingerprint
                && previous.end_tick == start_tick
                && previous.swamp_animation_start_tick == swamp_animation_start_tick
            {
                previous.end_tick = end_tick;
                continue;
            }
            spans.push(TerrainGeometrySpan {
                start_tick,
                end_tick,
                fingerprint,
                swamp_animation_start_tick,
            });
        }
        Ok(TerrainGeometryTimeline { geometries, spans })
    }
}

fn renderer_raster_dimensions(
    artifact: &ReplayArtifact,
    options: &serde_json::Map<String, Value>,
) -> Result<[u32; 2]> {
    let size = options
        .get("RENDER_SIZE")
        .or_else(|| options.get("size"))
        .and_then(Value::as_object);
    if let Some(size) = size {
        return Ok([
            required_positive_u32(size.get("width"), "size.width")?,
            required_positive_u32(size.get("height"), "size.height")?,
        ]);
    }
    artifact
        .replay
        .render_config
        .0
        .as_ref()
        .map(|config| [config.width, config.height])
        .ok_or_else(|| {
            Error::Invalid("terrain renderer lacks RENDER_SIZE, size, and renderConfig".to_owned())
        })
}

#[derive(Clone, Debug)]
struct CellGrid {
    room_size: u32,
    cells: Vec<bool>,
}

impl CellGrid {
    fn new(room_size: u32) -> Result<Self> {
        let len = usize::try_from(room_size)
            .ok()
            .and_then(|size| size.checked_mul(size))
            .ok_or(Error::ArithmeticOverflow)?;
        Ok(Self {
            room_size,
            cells: vec![false; len],
        })
    }

    fn contains(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x >= self.room_size as i32 || y >= self.room_size as i32 {
            return false;
        }
        self.cells[y as usize * self.room_size as usize + x as usize]
    }

    fn insert(&mut self, x: u32, y: u32) {
        self.cells[y as usize * self.room_size as usize + x as usize] = true;
    }
}

struct PathBuilder<'a> {
    grid: &'a CellGrid,
    diagonal_connect: bool,
    visited: Vec<bool>,
    path: String,
}

impl<'a> PathBuilder<'a> {
    fn new(grid: &'a CellGrid, diagonal_connect: bool) -> Self {
        Self {
            grid,
            diagonal_connect,
            visited: vec![false; grid.cells.len()],
            path: String::new(),
        }
    }

    fn build(mut self) -> String {
        let size = self.grid.room_size as i32;
        for x in 0..size {
            for y in 0..size {
                if !self.grid.contains(x, y) || self.visited(x, y) {
                    continue;
                }
                self.push(format!("M {} {} ", x * 100, y * 100 + 50));
                self.set_visited(x, y);
                let mut horizontal = 0;
                while x + horizontal < size && self.grid.contains(x + horizontal, y) {
                    horizontal += 1;
                }
                let mut vertical = 0;
                while y + vertical < size && self.grid.contains(x, y + vertical) {
                    vertical += 1;
                }
                if vertical < horizontal {
                    if self.top_left_is_square(x, y) {
                        self.top_left_down(x, y);
                        self.top_left_up(x, y);
                    } else {
                        self.top_left(x, y);
                    }
                    if x < size - 1 && self.grid.contains(x + 1, y) {
                        if self.top_right_is_square(x, y) {
                            self.top_right_up(x, y);
                        } else {
                            self.push("h 50 ");
                        }
                        self.recurse(x + 1, y, true);
                        self.push("h -50 ");
                    } else {
                        if self.top_right_is_square(x, y) {
                            self.top_right_up(x, y);
                            self.top_right_down(x, y);
                        } else {
                            self.top_right(x, y);
                        }
                        self.bottom_right(x, y);
                    }
                    self.bottom_left(x, y);
                } else {
                    if self.top_left_is_square(x, y) {
                        self.top_left_down(x, y);
                        self.top_left_up(x, y);
                    } else {
                        self.top_left(x, y);
                    }
                    if self.top_right_is_square(x, y) {
                        self.top_right_up(x, y);
                        self.top_right_down(x, y);
                    } else {
                        self.top_right(x, y);
                    }
                    if y < size - 1 && self.grid.contains(x, y + 1) {
                        self.push("v 50 ");
                        self.recurse(x, y + 1, false);
                        self.push("v -50 ");
                    } else {
                        self.bottom_right(x, y);
                        self.bottom_left(x, y);
                    }
                }
                self.push("Z ");
            }
        }
        self.path
    }

    fn recurse(&mut self, x: i32, y: i32, horizontal: bool) {
        if self.visited(x, y) {
            self.push(if horizontal { "v 100 " } else { "h -100 " });
            return;
        }
        let size = self.grid.room_size as i32;
        if horizontal {
            if self.top_left_is_square(x, y) {
                self.top_left_up(x, y);
            } else {
                self.push("h 50 ");
            }
            if x < size - 1 && self.grid.contains(x + 1, y) {
                if self.top_right_is_square(x, y) {
                    self.top_right_up(x, y);
                } else {
                    self.push("h 50 ");
                }
                self.recurse(x + 1, y, true);
                self.push("h -100 ");
            } else {
                if self.top_right_is_square(x, y) {
                    self.top_right_up(x, y);
                    self.top_right_down(x, y);
                } else {
                    self.top_right(x, y);
                }
                self.bottom_right(x, y);
                self.push("h -50 ");
            }
        } else {
            if self.top_right_is_square(x, y) {
                self.top_right_down(x, y);
            } else {
                self.push("v 50 ");
            }
            if y < size - 1 && self.grid.contains(x, y + 1) {
                self.push("v 50 ");
                self.recurse(x, y + 1, false);
                self.push("v -50 ");
            } else {
                self.bottom_right(x, y);
                self.bottom_left(x, y);
            }
            if x == 0 || y == 0 || self.grid.contains(x - 1, y - 1) {
                self.top_left_down(x, y);
            } else {
                self.push("v -50 ");
            }
        }
        self.set_visited(x, y);
    }

    fn top_left_is_square(&self, x: i32, y: i32) -> bool {
        x == 0
            || y == 0
            || self.grid.contains(x - 1, y - 1)
                && (self.diagonal_connect
                    || self.grid.contains(x - 1, y)
                    || self.grid.contains(x, y - 1))
    }

    fn top_right_is_square(&self, x: i32, y: i32) -> bool {
        x == self.grid.room_size as i32 - 1
            || y == 0
            || self.grid.contains(x + 1, y - 1)
                && (self.diagonal_connect
                    || self.grid.contains(x + 1, y)
                    || self.grid.contains(x, y - 1))
    }

    fn top_left_down(&mut self, x: i32, y: i32) {
        if x > 0 && y > 0 && !self.grid.contains(x - 1, y) {
            self.push("a 50 50 0 0 0 -50 -50 h 50 ");
        } else {
            self.push("v -50 ");
        }
    }

    fn top_left_up(&mut self, x: i32, y: i32) {
        if y > 0 && x > 0 && !self.grid.contains(x, y - 1) {
            self.push("v -50 a 50 50 0 0 0 50 50 ");
        } else {
            self.push("h 50 ");
        }
    }

    fn top_left(&mut self, x: i32, y: i32) {
        if x == 0 || self.grid.contains(x - 1, y) || y == 0 || self.grid.contains(x, y - 1) {
            self.push("v -50 h 50 ");
        } else {
            self.push("a 50 50 0 0 1 50 -50 ");
        }
    }

    fn top_right_up(&mut self, x: i32, y: i32) {
        if y > 0 && x < self.grid.room_size as i32 - 1 && !self.grid.contains(x, y - 1) {
            self.push("a 50 50 0 0 0 50 -50 v 50 ");
        } else {
            self.push("h 50 ");
        }
    }

    fn top_right_down(&mut self, x: i32, y: i32) {
        if x < self.grid.room_size as i32 - 1 && y > 0 && !self.grid.contains(x + 1, y) {
            self.push("h 50 a 50 50 0 0 0 -50 50 ");
        } else {
            self.push("v 50 ");
        }
    }

    fn top_right(&mut self, x: i32, y: i32) {
        if x == self.grid.room_size as i32 - 1
            || self.grid.contains(x + 1, y)
            || y == 0
            || self.grid.contains(x, y - 1)
        {
            self.push("h 50 v 50 ");
        } else {
            self.push("a 50 50 0 0 1 50 50 ");
        }
    }

    fn bottom_right(&mut self, x: i32, y: i32) {
        let last = self.grid.room_size as i32 - 1;
        if x == last
            || self.grid.contains(x + 1, y)
            || y == last
            || self.grid.contains(x, y + 1)
            || self.grid.contains(x + 1, y + 1)
                && (self.grid.contains(x + 1, y) || self.diagonal_connect)
        {
            self.push("v 50 h -50 ");
        } else {
            self.push("a 50 50 0 0 1 -50 50 ");
        }
    }

    fn bottom_left(&mut self, x: i32, y: i32) {
        let last = self.grid.room_size as i32 - 1;
        if x == 0
            || self.grid.contains(x - 1, y)
            || y == last
            || self.grid.contains(x, y + 1)
            || self.grid.contains(x - 1, y + 1)
                && (self.grid.contains(x - 1, y) || self.diagonal_connect)
        {
            self.push("h -50 v -50 ");
        } else {
            self.push("a 50 50 0 0 1 -50 -50 ");
        }
    }

    fn visited(&self, x: i32, y: i32) -> bool {
        self.visited[y as usize * self.grid.room_size as usize + x as usize]
    }

    fn set_visited(&mut self, x: i32, y: i32) {
        self.visited[y as usize * self.grid.room_size as usize + x as usize] = true;
    }

    fn push(&mut self, text: impl AsRef<str>) {
        self.path.push_str(text.as_ref());
    }
}

fn render_path(grid: &CellGrid, diagonal_connect: bool) -> Option<String> {
    grid.cells
        .iter()
        .any(|occupied| *occupied)
        .then(|| PathBuilder::new(grid, diagonal_connect).build())
}

fn required_positive_u32(value: Option<&Value>, name: &str) -> Result<u32> {
    let value = value
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::Invalid(format!("renderer worldOptions.{name} is invalid")))?;
    Ok(value)
}

fn cell_coordinates(value: &Value, room_size: u32, label: &str) -> Result<(u32, u32)> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("{label} must be an object")))?;
    let x = coordinate(object.get("x"), room_size, &format!("{label}.x"))?;
    let y = coordinate(object.get("y"), room_size, &format!("{label}.y"))?;
    Ok((x, y))
}

fn coordinate(value: Option<&Value>, room_size: u32, label: &str) -> Result<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value < room_size)
        .ok_or_else(|| Error::Invalid(format!("{label} is outside the terrain grid")))
}

fn global_user_colors(
    artifact: &ReplayArtifact,
    tick: u32,
) -> Result<BTreeMap<String, Option<u32>>> {
    let Some(value) = artifact
        .replay
        .global_state
        .get("users")
        .and_then(|track| track.at(tick))
    else {
        return Ok(BTreeMap::new());
    };
    match value {
        TrackValue::Absent | TrackValue::Undefined => Ok(BTreeMap::new()),
        TrackValue::Value(Value::Object(object)) => object
            .iter()
            .map(|(user, value)| {
                let color = value
                    .get("color")
                    .and_then(Value::as_str)
                    .and_then(parse_hex_color);
                Ok((user.clone(), color))
            })
            .collect(),
        TrackValue::Value(_) => Err(Error::Invalid(
            "global state users must be an object".to_owned(),
        )),
    }
}

fn swamp_texture_at(artifact: &ReplayArtifact, tick: u32) -> Result<TerrainSwampTexture> {
    let value = artifact
        .replay
        .global_state
        .get("gameData")
        .and_then(|track| track.at(tick));
    let game_data = match value {
        None | Some(TrackValue::Absent | TrackValue::Undefined) => {
            return Ok(TerrainSwampTexture::Animated);
        }
        Some(TrackValue::Value(Value::Object(game_data))) => game_data,
        Some(TrackValue::Value(_)) => {
            return Err(Error::Invalid(
                "global state gameData must be an object".to_owned(),
            ));
        }
    };
    Ok(
        match game_data.get("swampTexture").and_then(Value::as_str) {
            None | Some("animated") => TerrainSwampTexture::Animated,
            Some("disabled") => TerrainSwampTexture::Disabled,
            Some(_) => TerrainSwampTexture::Static,
        },
    )
}

fn parse_hex_color(value: &str) -> Option<u32> {
    let digits = value.strip_prefix('#')?;
    (digits.len() == 6)
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
}

fn latest_truthy_tick(track: &Track, tick: u32) -> Option<u32> {
    let Track(bounds, values, absent, undefined) = track;
    let mut latest = None;
    for (index, pair) in bounds.chunks_exact(2).enumerate() {
        let start = pair[0];
        if start > tick {
            break;
        }
        if absent.binary_search(&(index as u32)).is_ok()
            || undefined.binary_search(&(index as u32)).is_ok()
            || !json_truthy(&values[index])
        {
            continue;
        }
        latest = Some(tick.min(pair[1] - 1));
    }
    latest
}

fn insert_track_boundaries(track: &Track, total_ticks: u32, ticks: &mut BTreeSet<u32>) {
    ticks.extend(track.0.iter().copied().filter(|tick| *tick <= total_ticks));
}

fn track_contains_terrain_type(track: &Track) -> bool {
    track.1.iter().any(|value| {
        matches!(
            value.as_str(),
            Some("wall" | "constructedWall" | "swamp" | "rampart")
        )
    })
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value
            .as_f64()
            .is_some_and(|value| value != 0.0 && !value.is_nan()),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn entity_cell(entity: &Entity, tick: u32, room_size: u32) -> Result<(u32, u32)> {
    let x = entity_json(entity, "x", tick);
    let y = entity_json(entity, "y", tick);
    Ok((
        coordinate(x, room_size, &format!("entity {} x", entity.id))?,
        coordinate(y, room_size, &format!("entity {} y", entity.id))?,
    ))
}

fn entity_string<'a>(entity: &'a Entity, property: &str, tick: u32) -> Result<Option<&'a str>> {
    match entity_json(entity, property, tick) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Error::Invalid(format!(
            "entity {} property {property} must be a string",
            entity.id
        ))),
    }
}

fn entity_bool(entity: &Entity, property: &str, tick: u32) -> Result<Option<bool>> {
    match entity_json(entity, property, tick) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(Error::Invalid(format!(
            "entity {} property {property} must be a boolean",
            entity.id
        ))),
    }
}

fn entity_json<'a>(entity: &'a Entity, property: &str, tick: u32) -> Option<&'a Value> {
    match entity
        .properties
        .get(property)
        .and_then(|track| track.at(tick))
    {
        Some(TrackValue::Value(value)) => Some(value),
        None | Some(TrackValue::Absent | TrackValue::Undefined) => None,
    }
}

fn hash_optional_path(hasher: &mut Sha256, path: &Option<String>) {
    match path {
        Some(path) => {
            hasher.update([1]);
            hash_bytes(hasher, path.as_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{CellGrid, render_path};
    use crate::artifact::tests::{artifact_json, signed};
    use crate::{ReplayArtifact, TerrainGeometryCompiler, TerrainSwampTexture};

    #[test]
    fn reproduces_official_single_cell_rounded_path() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({
                "CELL_SIZE": 100,
                "ROOM_SIZE": 5,
                "VIEW_BOX": 500,
                "RENDER_SIZE": {"width": 32, "height": 16}
            }),
        );
        contract.insert(
            "terrain".to_owned(),
            json!([
                {"type": "wall", "x": 1, "y": 1},
                {"type": "swamp", "x": 2, "y": 3}
            ]),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();

        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();
        assert_eq!(compiler.raster_dimensions(), [32, 16]);
        let geometry = compiler.geometry_at(0).unwrap();
        assert_eq!(geometry.view_box, 500);
        assert_eq!(
            geometry.wall_path.as_deref(),
            Some(
                "M 100 150 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
            )
        );
        assert_eq!(
            geometry.swamp_path.as_deref(),
            Some(
                "M 200 350 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
            )
        );
        assert_eq!(geometry.fingerprint.len(), 64);
    }

    #[test]
    fn matches_official_connected_and_diagonal_path_vectors() {
        type PathVector<'a> = (&'a [(u32, u32)], bool, &'a str);
        let vectors: &[PathVector<'_>] = &[
            (
                &[(1, 1), (2, 1)],
                false,
                "M 100 150 a 50 50 0 0 1 50 -50 h 50 h 50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 h -50 h -50 a 50 50 0 0 1 -50 -50 Z ",
            ),
            (
                &[(1, 1), (1, 2)],
                false,
                "M 100 150 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 v 50 v 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 v -50 v -50 Z ",
            ),
            (
                &[(1, 1), (2, 2)],
                false,
                "M 100 150 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z M 200 250 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z ",
            ),
            (
                &[(1, 1), (2, 2)],
                true,
                "M 100 150 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 v 50 h -50 a 50 50 0 0 1 -50 -50 Z M 200 250 a 50 50 0 0 0 -50 -50 h 50 v -50 a 50 50 0 0 0 50 50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z ",
            ),
            (
                &[(1, 1), (2, 1), (1, 2)],
                false,
                "M 100 150 a 50 50 0 0 1 50 -50 h 50 v 50 v 50 h 50 a 50 50 0 0 0 -50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 v -50 v -50 Z M 200 150 v -50 h 50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 h -50 v -50 Z ",
            ),
        ];
        for (cells, diagonal_connect, expected) in vectors {
            let mut grid = CellGrid::new(5).unwrap();
            for &(x, y) in *cells {
                grid.insert(x, y);
            }
            assert_eq!(
                render_path(&grid, *diagonal_connect).as_deref(),
                Some(*expected)
            );
        }
    }

    #[test]
    fn combines_dynamic_walls_and_owner_private_ramparts_by_tick() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay.insert(
            "globalState".to_owned(),
            json!({
                "users": [[0, 2], [{"owner": {"color": "#ff00ff"}}], [], []]
            }),
        );
        replay["entities"][0]["properties"] = json!({
            "type": [[0, 1, 1, 2], ["constructedWall", "rampart"], [], []],
            "x": [[0, 2], [2], [], []],
            "y": [[0, 2], [2], [], []],
            "user": [[0, 2], ["owner"], [], []],
            "isPublic": [[0, 2], [false], [], []]
        });
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();
        assert_eq!(compiler.geometry_change_ticks(), vec![0, 1]);
        let timeline = compiler.compile_timeline().unwrap();
        assert_eq!(timeline.spans.len(), 2);
        assert_eq!(timeline.spans[0].start_tick, 0);
        assert_eq!(timeline.spans[0].end_tick, 1);
        assert_eq!(timeline.spans[1].start_tick, 1);
        assert_eq!(timeline.spans[1].end_tick, 2);

        let wall_tick = compiler.geometry_at(0).unwrap();
        assert!(wall_tick.wall_path.is_some());
        assert!(wall_tick.private_rampart_paths.is_empty());
        let rampart_tick = compiler.geometry_at(1).unwrap();
        assert!(rampart_tick.wall_path.is_none());
        assert_eq!(
            rampart_tick
                .private_rampart_paths
                .get("owner")
                .map(String::as_str),
            Some(
                "M 200 250 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
            )
        );
        assert_eq!(rampart_tick.private_rampart_colors["owner"], 0xff00ff);
        assert_ne!(wall_tick.fingerprint, rampart_tick.fingerprint);
    }

    #[test]
    fn rampart_color_is_latched_until_that_users_path_is_rebuilt() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay.insert(
            "globalState".to_owned(),
            json!({
                "users": [
                    [0, 1, 1, 2],
                    [
                        {"owner": {"color": "#112233"}},
                        {"owner": {"color": "#445566"}}
                    ],
                    [],
                    []
                ]
            }),
        );
        replay["entities"][0]["properties"] = json!({
            "type": [[0, 2], ["rampart"], [], []],
            "x": [[0, 2], [2], [], []],
            "y": [[0, 2], [2], [], []],
            "user": [[0, 2], ["owner"], [], []],
            "isPublic": [[0, 2], [false], [], []]
        });
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();

        let first = compiler.geometry_at(0).unwrap();
        let second = compiler.geometry_at(1).unwrap();
        assert_eq!(first.private_rampart_paths, second.private_rampart_paths);
        assert_eq!(first.private_rampart_colors["owner"], 0x11_22_33);
        assert_eq!(second.private_rampart_colors["owner"], 0x44_55_66);
        assert_ne!(first.fingerprint, second.fingerprint);
        let timeline = compiler.compile_timeline().unwrap();
        assert_eq!(timeline.spans.len(), 1);
        assert_eq!(
            timeline.geometries[&timeline.spans[0].fingerprint].private_rampart_colors["owner"],
            0x11_22_33
        );
    }

    #[test]
    fn swamp_texture_mode_is_latched_until_the_path_is_rebuilt() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        contract.insert(
            "terrain".to_owned(),
            json!([{"type": "swamp", "x": 1, "y": 1}]),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay.insert(
            "globalState".to_owned(),
            json!({
                "gameData": [
                    [0, 1, 1, 2],
                    [{"swampTexture": "animated"}, {"swampTexture": "disabled"}],
                    [],
                    []
                ]
            }),
        );
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();

        let first = compiler.geometry_at(0).unwrap();
        let second = compiler.geometry_at(1).unwrap();
        assert_eq!(first.swamp_path, second.swamp_path);
        assert_eq!(first.swamp_texture, TerrainSwampTexture::Animated);
        assert_eq!(second.swamp_texture, TerrainSwampTexture::Disabled);
        assert_ne!(first.fingerprint, second.fingerprint);
        let timeline = compiler.compile_timeline().unwrap();
        assert_eq!(timeline.spans.len(), 1);
        assert_eq!(
            timeline.geometries[&timeline.spans[0].fingerprint].swamp_texture,
            TerrainSwampTexture::Animated
        );
        assert_eq!(timeline.spans[0].swamp_animation_start_tick, 0);
    }

    #[test]
    fn swamp_path_rebuild_applies_the_current_texture_mode_and_restarts_phase() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        contract.insert(
            "terrain".to_owned(),
            json!([{"type": "swamp", "x": 1, "y": 1}]),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay.insert(
            "globalState".to_owned(),
            json!({
                "gameData": [
                    [0, 1, 1, 2],
                    [{"swampTexture": "animated"}, {"swampTexture": "disabled"}],
                    [],
                    []
                ]
            }),
        );
        replay["entities"][0]["properties"] = json!({
            "type": [[0, 2], ["swamp"], [], []],
            "x": [[0, 1, 1, 2], [2, 3], [], []],
            "y": [[0, 2], [2], [], []]
        });
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();

        let timeline = compiler.compile_timeline().unwrap();
        assert_eq!(timeline.spans.len(), 2);
        assert_eq!(timeline.spans[0].swamp_animation_start_tick, 0);
        assert_eq!(timeline.spans[1].swamp_animation_start_tick, 1);
        assert_eq!(
            timeline.geometries[&timeline.spans[1].fingerprint].swamp_texture,
            TerrainSwampTexture::Disabled
        );
    }

    #[test]
    fn swamp_phase_uses_renderer_apply_time_instead_of_naive_tick_time() {
        use crate::{Rational, Timeline};

        let timeline = Timeline::new(
            2,
            Rational::new(3, 1).unwrap(),
            Rational::new(3, 1).unwrap(),
            Rational::new(12, 1).unwrap(),
            Rational::new(1, 4).unwrap(),
        )
        .unwrap();
        let frame = timeline.sample(1).unwrap();
        assert_eq!(frame.tick, 2);
        let span = super::TerrainGeometrySpan {
            start_tick: 2,
            end_tick: 3,
            fingerprint: "ab".repeat(32),
            swamp_animation_start_tick: 2,
        };
        assert_eq!(span.swamp_phase_seconds(frame, timeline).unwrap(), 0.0);
    }

    #[test]
    fn empty_wall_and_swamp_requests_retain_the_previous_displayed_sprites() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        contract.insert(
            "terrain".to_owned(),
            json!([
                {"type": "wall", "x": 1, "y": 1},
                {"type": "swamp", "x": 2, "y": 2}
            ]),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay.insert(
            "globalState".to_owned(),
            json!({
                "setTerrain": [[0, 1, 1, 2], [false, true], [], []]
            }),
        );
        replay["entities"][0]["properties"] = json!({
            "type": [[0, 2], ["unit"], [], []]
        });
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();

        assert!(compiler.geometry_at(1).unwrap().wall_path.is_none());
        assert!(compiler.geometry_at(1).unwrap().swamp_path.is_none());
        let timeline = compiler.compile_timeline().unwrap();
        assert_eq!(timeline.spans.len(), 1);
        let geometry = &timeline.geometries[&timeline.spans[0].fingerprint];
        assert!(geometry.wall_path.is_some());
        assert!(geometry.swamp_path.is_some());
        assert_eq!(timeline.spans[0].swamp_animation_start_tick, 0);
    }

    #[test]
    fn set_terrain_replaces_static_cells_and_persists_the_replacement() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        contract.insert(
            "terrain".to_owned(),
            json!([{"type": "wall", "x": 1, "y": 1}]),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay.insert(
            "globalState".to_owned(),
            json!({
                "setTerrain": [[0, 1, 1, 2], [true, false], [], []]
            }),
        );
        replay["entities"][0]["properties"] = json!({
            "type": [[0, 1, 1, 2], ["wall", "creep"], [], []],
            "x": [[0, 2], [2], [], []],
            "y": [[0, 2], [2], [], []]
        });
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();

        assert_eq!(
            compiler.geometry_at(1).unwrap().wall_path.as_deref(),
            Some(
                "M 200 250 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
            )
        );
    }

    #[test]
    fn set_terrain_excludes_constructed_walls_only_on_the_replacement_tick() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay.insert(
            "globalState".to_owned(),
            json!({
                "setTerrain": [[0, 1, 1, 2], [true, false], [], []]
            }),
        );
        replay["entities"][0]["properties"] = json!({
            "type": [[0, 2], ["constructedWall"], [], []],
            "x": [[0, 2], [2], [], []],
            "y": [[0, 2], [2], [], []]
        });
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();

        assert!(compiler.geometry_at(0).unwrap().wall_path.is_none());
        assert!(compiler.geometry_at(1).unwrap().wall_path.is_some());
    }

    #[test]
    fn rejects_room_sizes_that_could_exhaust_memory() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 4294967295_u32}),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();

        assert!(TerrainGeometryCompiler::new(&artifact).is_err());
    }

    #[test]
    fn change_points_include_sparse_ends_while_displayed_walls_remain_latched() {
        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        let contract = artifact["rendererContract"].as_object_mut().unwrap();
        contract.insert(
            "worldOptions".to_owned(),
            json!({"CELL_SIZE": 100, "ROOM_SIZE": 5}),
        );
        artifact["rendererContract"] = signed(Value::Object(contract.clone()));
        let contract_fingerprint = artifact["rendererContract"]["fingerprint"].clone();
        let replay = artifact["replay"].as_object_mut().unwrap();
        replay.insert(
            "rendererContractFingerprint".to_owned(),
            contract_fingerprint,
        );
        replay["entities"][0]["properties"] = json!({
            "type": [[0, 1], ["wall"], [], []],
            "x": [[0, 2], [2], [], []],
            "y": [[0, 2], [2], [], []]
        });
        artifact["replay"] = signed(Value::Object(replay.clone()));
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()).unwrap();
        let compiler = TerrainGeometryCompiler::new(&artifact).unwrap();

        assert_eq!(compiler.geometry_change_ticks(), vec![0, 1]);
        assert!(compiler.geometry_at(0).unwrap().wall_path.is_some());
        assert!(compiler.geometry_at(1).unwrap().wall_path.is_none());
        assert_eq!(compiler.compile_timeline().unwrap().spans.len(), 1);
    }
}
