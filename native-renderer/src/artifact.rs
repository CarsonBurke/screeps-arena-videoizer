use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Error, Result};

const REPLAY_SCHEMA: &str = "screeps-arena-replay-ir";
const REPLAY_VERSION: u32 = 8;
const CONTRACT_SCHEMA: &str = "screeps-arena-renderer-contract";
const CONTRACT_VERSION: u32 = 5;
const SHA256_HEX_LENGTH: usize = 64;

#[derive(Default)]
struct EcmaScriptFormatter;

impl serde_json::ser::Formatter for EcmaScriptFormatter {
    fn write_i64<W>(&mut self, writer: &mut W, value: i64) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.write_f64(writer, value as f64)
    }

    fn write_u64<W>(&mut self, writer: &mut W, value: u64) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        self.write_f64(writer, value as f64)
    }

    fn write_f32<W>(&mut self, writer: &mut W, value: f32) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        let mut buffer = ryu_js::Buffer::new();
        std::io::Write::write_all(writer, buffer.format_finite(value).as_bytes())
    }

    fn write_f64<W>(&mut self, writer: &mut W, value: f64) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        let mut buffer = ryu_js::Buffer::new();
        std::io::Write::write_all(writer, buffer.format_finite(value).as_bytes())
    }
}

fn ecmascript_json_vec<T>(value: &T) -> Result<Vec<u8>>
where
    T: ?Sized + Serialize,
{
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, EcmaScriptFormatter);
    value.serialize(&mut serializer)?;
    Ok(bytes)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReplayArtifact {
    pub renderer_contract: RendererContract,
    pub replay: ReplayIr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RendererContract {
    pub schema: String,
    pub version: u32,
    pub renderer_version: Nullable<String>,
    pub metadata: Value,
    pub resources: Value,
    pub decorations: Vec<Value>,
    pub terrain: Vec<Value>,
    pub world_options: Value,
    pub inventory: RendererInventory,
    pub fingerprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RendererInventory {
    pub object_types: Vec<String>,
    pub processor_types: Vec<String>,
    pub action_types: Vec<String>,
    pub preprocessors: Vec<String>,
    pub calculation_ids: Vec<String>,
    pub drawing_methods: Vec<String>,
    pub expression_operators: Vec<String>,
    pub function_semantics: Vec<String>,
    pub layer_ids: Vec<String>,
    pub renderer_implementation_fingerprints: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReplayIr {
    pub schema: String,
    pub version: u32,
    pub total_ticks: u32,
    pub timeline: TimelineContract,
    pub render_config: Nullable<RenderConfig>,
    pub renderer_contract_fingerprint: Nullable<String>,
    pub random_seed: Nullable<String>,
    pub random_state_at_first_tick: Nullable<u32>,
    pub calculation_outputs: CalculationOutputs,
    pub renderer_graph: RendererGraph,
    pub global_state: BTreeMap<String, Track>,
    pub visual_overlay: VisualOverlay,
    pub object_order: Track,
    pub entities: Vec<Entity>,
    pub action_events: Vec<(u32, String, String)>,
    pub tick_fingerprints: Vec<String>,
    pub fingerprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimelineContract {
    pub frames_per_second: Nullable<String>,
    pub ticks_per_second: Nullable<String>,
    pub substeps_per_second: Nullable<String>,
    pub tick_transition_seconds: Nullable<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RenderConfig {
    pub width: u32,
    pub height: u32,
    pub background_color: u32,
    pub board_frame: BoardFrame,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BoardFrame {
    pub mode: String,
    pub output_width: f64,
    pub output_height: f64,
    pub board_width: f64,
    pub board_height: f64,
    pub world_min_x: f64,
    pub world_min_y: f64,
    pub pivot_x: f64,
    pub pivot_y: f64,
    pub zoom: f64,
    pub x: f64,
    pub y: f64,
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub width: f64,
    pub height: f64,
    pub padding: f64,
    pub pan_x: f64,
    pub pan_y: f64,
}

/// A nullable JSON value whose containing field is nevertheless required.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct Nullable<T>(pub Option<T>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalculationOutputs {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RendererGraph {
    pub columns: Vec<Vec<i32>>,
    pub enabled: bool,
    pub entity_ids: Vec<String>,
    pub offsets: Vec<u32>,
    pub payloads: Vec<Value>,
    pub semantic_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualOverlay {
    pub enabled: bool,
    pub states: Track,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entity {
    pub id: String,
    pub lifetimes: Vec<[u32; 2]>,
    pub properties: BTreeMap<String, Track>,
    pub calculations: BTreeMap<String, Track>,
}

/// `[bounds, values, absentIndices, undefinedIndices, nonFiniteEntries]`.
#[derive(Debug, Deserialize)]
pub struct Track(
    pub Vec<u32>,
    pub Vec<Value>,
    pub Vec<u32>,
    pub Vec<u32>,
    pub Vec<NonFiniteEntry>,
);

/// `[segmentIndex, JSONPointer, numberCode]`, where -1/0/1 encode
/// -Infinity/NaN/+Infinity and the stored JSON leaf is an unambiguous null.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NonFiniteEntry(pub u32, pub String, pub i8);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackValue<'a> {
    Absent,
    Undefined,
    Value(&'a Value),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RendererEventOpcode {
    ActionFinish = 0,
    ActionRun = 1,
    ObjectAlpha = 2,
    ObjectCreate = 3,
    ObjectRemove = 4,
    PreprocessorRun = 5,
    ProcessorDestruct = 6,
    ProcessorRun = 7,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RendererEvent<'a> {
    pub event_index: u32,
    pub tick: u32,
    pub entity_id: Option<&'a str>,
    pub opcode: RendererEventOpcode,
    pub semantic_id: Option<&'a str>,
    pub payload: Option<&'a Value>,
}

#[derive(Clone, Debug)]
pub struct RendererEventIter<'a> {
    replay: &'a ReplayIr,
    tick: u32,
    next_index: u32,
    end_index: u32,
}

/// Replay plus an entity lookup table built once for render-time access.
#[derive(Debug)]
pub struct IndexedReplay {
    artifact: ReplayArtifact,
    entities: BTreeMap<String, usize>,
}

impl ReplayArtifact {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_slice(&fs::read(path)?)
    }

    pub fn events_at(&self, tick: u32) -> Result<RendererEventIter<'_>> {
        renderer_events_at(&self.replay, tick)
    }

    /// Load only the canonical, minified JSON produced by the JS compiler.
    ///
    /// Requiring the compiler serialization removes ambiguity around duplicate
    /// keys, number spellings, and whitespace before fingerprints are checked.
    /// The capture writer appends one line feed after that canonical body.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let canonical_bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let value: Value = serde_json::from_slice(canonical_bytes)?;
        if ecmascript_json_vec(&value)? != canonical_bytes {
            return Err(Error::NonCanonicalJson);
        }
        let root = value
            .as_object()
            .ok_or_else(|| Error::Invalid("ReplayIR artifact must be an object".to_owned()))?;
        if root.len() != 2 || !root.contains_key("rendererContract") || !root.contains_key("replay")
        {
            return Err(Error::Invalid(
                "ReplayIR artifact must contain only rendererContract and replay".to_owned(),
            ));
        }
        validate_required_nullable_fields(root)?;
        verify_fingerprint(
            root.get("rendererContract").expect("checked key"),
            "renderer contract",
        )?;
        verify_fingerprint(root.get("replay").expect("checked key"), "ReplayIR")?;

        let artifact: Self = serde_json::from_value(value)?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn validate(&self) -> Result<()> {
        self.renderer_contract.validate()?;
        self.replay.validate()?;
        if self.replay.renderer_contract_fingerprint.0.as_deref()
            != Some(self.renderer_contract.fingerprint.as_str())
        {
            return Err(Error::Invalid(
                "ReplayIR renderer contract fingerprint mismatch".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn into_indexed(self) -> IndexedReplay {
        let entities = self
            .replay
            .entities
            .iter()
            .enumerate()
            .map(|(index, entity)| (entity.id.clone(), index))
            .collect();
        IndexedReplay {
            artifact: self,
            entities,
        }
    }
}

impl RendererContract {
    fn validate(&self) -> Result<()> {
        if self.schema != CONTRACT_SCHEMA || self.version != CONTRACT_VERSION {
            return Err(Error::Invalid(
                "unsupported renderer contract schema/version".to_owned(),
            ));
        }
        if !is_sha256(&self.fingerprint) {
            return Err(Error::Invalid(
                "invalid renderer contract fingerprint".to_owned(),
            ));
        }
        for (name, values) in self.inventory.fields() {
            validate_sorted_unique(values, name)?;
        }
        for fingerprint in &self.inventory.renderer_implementation_fingerprints {
            if !is_sha256(fingerprint) {
                return Err(Error::Invalid(
                    "invalid renderer implementation fingerprint".to_owned(),
                ));
            }
        }
        let mut object_filter_semantics = 0u8;
        for semantic in &self.inventory.function_semantics {
            let Some((name, fingerprint)) = semantic.rsplit_once(':') else {
                return Err(Error::Invalid(
                    "invalid renderer function semantic".to_owned(),
                ));
            };
            if name.is_empty() || name.contains(':') || !is_sha256(fingerprint) {
                return Err(Error::Invalid(
                    "invalid renderer function semantic".to_owned(),
                ));
            }
            if name == "objectFilter" {
                object_filter_semantics = object_filter_semantics
                    .checked_add(1)
                    .ok_or(Error::ArithmeticOverflow)?;
            }
        }
        if object_filter_semantics > 1 {
            return Err(Error::Invalid(
                "renderer contract has multiple objectFilter semantics".to_owned(),
            ));
        }
        crate::scene_plan::validate_metadata_inventory(self)?;
        Ok(())
    }
}

impl RendererInventory {
    fn fields(&self) -> [(&'static str, &[String]); 10] {
        [
            ("objectTypes", &self.object_types),
            ("processorTypes", &self.processor_types),
            ("actionTypes", &self.action_types),
            ("preprocessors", &self.preprocessors),
            ("calculationIds", &self.calculation_ids),
            ("drawingMethods", &self.drawing_methods),
            ("expressionOperators", &self.expression_operators),
            ("functionSemantics", &self.function_semantics),
            ("layerIds", &self.layer_ids),
            (
                "rendererImplementationFingerprints",
                &self.renderer_implementation_fingerprints,
            ),
        ]
    }
}

impl ReplayIr {
    fn validate(&self) -> Result<()> {
        if self.schema != REPLAY_SCHEMA || self.version != REPLAY_VERSION {
            return Err(Error::Invalid(
                "unsupported ReplayIR schema/version".to_owned(),
            ));
        }
        if self.total_ticks == u32::MAX {
            return Err(Error::Invalid(
                "ReplayIR totalTicks exceeds the native index range".to_owned(),
            ));
        }
        if !is_sha256(&self.fingerprint)
            || self
                .renderer_contract_fingerprint
                .0
                .as_deref()
                .is_some_and(|value| !is_sha256(value))
        {
            return Err(Error::Invalid("invalid ReplayIR fingerprint".to_owned()));
        }
        self.timeline.validate()?;
        if let Some(render_config) = &self.render_config.0 {
            render_config.validate()?;
        }

        for (name, track) in &self.global_state {
            track.validate(
                self.total_ticks,
                false,
                false,
                &format!("globalState.{name}"),
            )?;
        }
        self.object_order
            .validate(self.total_ticks, true, false, "objectOrder")?;
        self.visual_overlay.states.validate(
            self.total_ticks,
            true,
            false,
            "visualOverlay.states",
        )?;
        validate_object_order(&self.object_order)?;

        let mut identities = BTreeSet::new();
        for entity in &self.entities {
            if !identities.insert(entity.id.as_str()) {
                return Err(Error::Invalid(
                    "invalid or duplicate ReplayIR entity".to_owned(),
                ));
            }
            let mut previous_end = 0;
            for (index, [start, end]) in entity.lifetimes.iter().copied().enumerate() {
                if end <= start || end > self.total_ticks + 1 || (index > 0 && start < previous_end)
                {
                    return Err(Error::Invalid(format!(
                        "invalid ReplayIR lifetime for {}",
                        entity.id
                    )));
                }
                previous_end = end;
            }
            for (name, track) in &entity.properties {
                track.validate(
                    self.total_ticks,
                    false,
                    false,
                    &format!("entity {}.{name}", entity.id),
                )?;
            }
            for (name, track) in &entity.calculations {
                track.validate(
                    self.total_ticks,
                    false,
                    true,
                    &format!("entity {} calculation {name}", entity.id),
                )?;
            }
            if !self.calculation_outputs.enabled && !entity.calculations.is_empty() {
                return Err(Error::Invalid(format!(
                    "ReplayIR has disabled calculation outputs for {}",
                    entity.id
                )));
            }
        }

        let mut previous_tick = 0;
        for (index, (tick, entity, _name)) in self.action_events.iter().enumerate() {
            if *tick > self.total_ticks
                || (index > 0 && *tick < previous_tick)
                || !identities.contains(entity.as_str())
            {
                return Err(Error::Invalid("invalid ReplayIR action event".to_owned()));
            }
            previous_tick = *tick;
        }
        self.renderer_graph
            .validate(self.total_ticks, &identities)?;

        if !self.tick_fingerprints.is_empty()
            && self.tick_fingerprints.len() != self.total_ticks as usize + 1
        {
            return Err(Error::Invalid(
                "invalid ReplayIR tick fingerprint count".to_owned(),
            ));
        }
        if self
            .tick_fingerprints
            .iter()
            .any(|fingerprint| !is_sha256(fingerprint))
        {
            return Err(Error::Invalid(
                "invalid ReplayIR tick fingerprint".to_owned(),
            ));
        }
        Ok(())
    }
}

impl TimelineContract {
    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("framesPerSecond", self.frames_per_second.0.as_deref()),
            ("ticksPerSecond", self.ticks_per_second.0.as_deref()),
            ("substepsPerSecond", self.substeps_per_second.0.as_deref()),
            (
                "tickTransitionSeconds",
                self.tick_transition_seconds.0.as_deref(),
            ),
        ] {
            if let Some(value) = value {
                crate::Rational::parse_rate(value, name)?;
            }
        }
        Ok(())
    }
}

impl RenderConfig {
    fn validate(&self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self.background_color > 0x00ff_ffff
            || self.board_frame.output_width != f64::from(self.width)
            || self.board_frame.output_height != f64::from(self.height)
        {
            return Err(Error::Invalid(
                "invalid ReplayIR render configuration".to_owned(),
            ));
        }
        self.board_frame.validate()
    }
}

impl BoardFrame {
    fn validate(&self) -> Result<()> {
        let values = [
            self.output_width,
            self.output_height,
            self.board_width,
            self.board_height,
            self.world_min_x,
            self.world_min_y,
            self.pivot_x,
            self.pivot_y,
            self.zoom,
            self.x,
            self.y,
            self.left,
            self.top,
            self.right,
            self.bottom,
            self.width,
            self.height,
            self.padding,
            self.pan_x,
            self.pan_y,
        ];
        if !matches!(self.mode.as_str(), "auto" | "manual")
            || values.iter().any(|value| !value.is_finite())
            || self.output_width <= 0.0
            || self.output_height <= 0.0
            || self.board_width <= 0.0
            || self.board_height <= 0.0
            || self.zoom <= 0.0
            || self.width <= 0.0
            || self.height <= 0.0
            || self.padding < 0.0
        {
            return Err(Error::Invalid("invalid ReplayIR board frame".to_owned()));
        }
        Ok(())
    }
}

impl RendererGraph {
    fn validate(&self, total_ticks: u32, identities: &BTreeSet<&str>) -> Result<()> {
        if self.columns.len() != 4 {
            return Err(Error::Invalid(
                "ReplayIR renderer graph must have four columns".to_owned(),
            ));
        }
        let event_count = self.columns[0].len();
        if self
            .columns
            .iter()
            .any(|column| column.len() != event_count)
        {
            return Err(Error::Invalid(
                "ReplayIR renderer event columns have unequal lengths".to_owned(),
            ));
        }
        validate_unique_strings(&self.entity_ids, "renderer entity IDs")?;
        validate_unique_strings(&self.semantic_ids, "renderer semantic IDs")?;
        if self
            .entity_ids
            .iter()
            .any(|identity| !identities.contains(identity.as_str()))
        {
            return Err(Error::Invalid(
                "renderer graph references an unknown entity".to_owned(),
            ));
        }
        if self.offsets.len() != total_ticks as usize + 2
            || self.offsets.first().copied() != Some(0)
            || self.offsets.last().copied() != Some(event_count as u32)
            || (!self.enabled && event_count != 0)
        {
            return Err(Error::Invalid(
                "invalid ReplayIR renderer event index".to_owned(),
            ));
        }
        for pair in self.offsets.windows(2) {
            if pair[0] > pair[1] || pair[1] as usize > event_count {
                return Err(Error::Invalid(
                    "invalid ReplayIR renderer event offset".to_owned(),
                ));
            }
        }
        for index in 0..event_count {
            let entity = self.columns[0][index];
            let opcode = self.columns[1][index];
            let semantic = self.columns[2][index];
            let payload = self.columns[3][index];
            if !valid_optional_index(entity, self.entity_ids.len())
                || !(0..=7).contains(&opcode)
                || !valid_optional_index(semantic, self.semantic_ids.len())
                || !valid_optional_index(payload, self.payloads.len())
                || !valid_event_shape(entity, opcode, semantic)
            {
                return Err(Error::Invalid(
                    "invalid ReplayIR renderer event value".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

impl Track {
    fn validate(
        &self,
        total_ticks: u32,
        require_coverage: bool,
        allow_non_finite: bool,
        name: &str,
    ) -> Result<()> {
        let Self(bounds, values, absent, undefined, non_finite) = self;
        if bounds.len() != values.len() * 2 {
            return Err(Error::Invalid(format!("invalid {name} track columns")));
        }
        let mut previous_end = 0;
        for (index, pair) in bounds.chunks_exact(2).enumerate() {
            let [start, end] = [pair[0], pair[1]];
            if end <= start
                || end > total_ticks + 1
                || (index > 0 && start < previous_end)
                || (require_coverage && index > 0 && start != previous_end)
            {
                return Err(Error::Invalid(format!("invalid {name} segment {index}")));
            }
            previous_end = end;
        }
        if require_coverage
            && (values.is_empty()
                || bounds.first().copied() != Some(0)
                || bounds.last().copied() != Some(total_ticks + 1))
        {
            return Err(Error::Invalid(format!(
                "{name} track does not cover the replay"
            )));
        }
        validate_sorted_indices(absent, values.len(), name)?;
        validate_sorted_indices(undefined, values.len(), name)?;
        if undefined
            .iter()
            .any(|index| absent.binary_search(index).is_ok())
        {
            return Err(Error::Invalid(format!(
                "{name} marks one segment absent and undefined"
            )));
        }
        let mut previous_index = None::<u32>;
        let mut segment_pointers = BTreeSet::new();
        for entry in non_finite {
            let NonFiniteEntry(index, pointer, code) = entry;
            if previous_index != Some(*index) {
                segment_pointers.clear();
            }
            if !allow_non_finite
                || *index as usize >= values.len()
                || !matches!(*code, -1..=1)
                || (!pointer.is_empty() && !pointer.starts_with('/'))
                || previous_index.is_some_and(|previous| previous > *index)
                || !segment_pointers.insert(pointer.as_str())
                || absent.binary_search(index).is_ok()
                || undefined.binary_search(index).is_ok()
                || json_pointer(&values[*index as usize], pointer) != Some(&Value::Null)
            {
                return Err(Error::Invalid(format!(
                    "invalid {name} non-finite calculation entry"
                )));
            }
            previous_index = Some(*index);
        }
        Ok(())
    }

    pub fn at(&self, tick: u32) -> Option<TrackValue<'_>> {
        let Self(bounds, values, absent, undefined, _) = self;
        let mut low = 0;
        let mut high = values.len();
        let index = loop {
            if low >= high {
                return None;
            }
            let middle = low + (high - low) / 2;
            let start = bounds[middle * 2];
            let end = bounds[middle * 2 + 1];
            if tick < start {
                high = middle;
            } else if tick >= end {
                low = middle + 1;
            } else {
                break middle;
            }
        };
        if absent.binary_search(&(index as u32)).is_ok() {
            Some(TrackValue::Absent)
        } else if undefined.binary_search(&(index as u32)).is_ok() {
            Some(TrackValue::Undefined)
        } else {
            Some(TrackValue::Value(&values[index]))
        }
    }

    pub fn non_finite_at(&self, tick: u32) -> Result<Vec<(&str, i8)>> {
        let Some(index) = self.segment_index_at(tick) else {
            return Ok(Vec::new());
        };
        Ok(self
            .4
            .iter()
            .filter(|entry| entry.0 as usize == index)
            .map(|entry| (entry.1.as_str(), entry.2))
            .collect())
    }

    fn segment_index_at(&self, tick: u32) -> Option<usize> {
        let mut low = 0;
        let mut high = self.1.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if tick < self.0[middle * 2] {
                high = middle;
            } else if tick >= self.0[middle * 2 + 1] {
                low = middle + 1;
            } else {
                return Some(middle);
            }
        }
        None
    }
}

fn json_pointer<'a>(value: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() {
        return Some(value);
    }
    pointer
        .strip_prefix('/')?
        .split('/')
        .try_fold(value, |current, token| {
            if token.char_indices().any(|(index, character)| {
                character == '~' && !matches!(token.as_bytes().get(index + 1), Some(b'0' | b'1'))
            }) {
                return None;
            }
            let token = token.replace("~1", "/").replace("~0", "~");
            match current {
                Value::Array(values) => {
                    let canonical_index = token == "0"
                        || (!token.starts_with('0')
                            && !token.is_empty()
                            && token.bytes().all(|byte| byte.is_ascii_digit()));
                    canonical_index
                        .then(|| token.parse::<usize>().ok())
                        .flatten()
                        .and_then(|index| values.get(index))
                }
                Value::Object(values) => values.get(&token),
                _ => None,
            }
        })
}

impl Entity {
    pub fn alive_at(&self, tick: u32) -> bool {
        self.lifetimes
            .binary_search_by(|[start, end]| {
                if tick < *start {
                    std::cmp::Ordering::Greater
                } else if tick >= *end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }
}

impl IndexedReplay {
    pub fn artifact(&self) -> &ReplayArtifact {
        &self.artifact
    }

    pub fn entity(&self, id: &str) -> Option<&Entity> {
        self.entities
            .get(id)
            .map(|index| &self.artifact.replay.entities[*index])
    }

    pub fn events_at(&self, tick: u32) -> Result<RendererEventIter<'_>> {
        renderer_events_at(&self.artifact.replay, tick)
    }
}

fn renderer_events_at(replay: &ReplayIr, tick: u32) -> Result<RendererEventIter<'_>> {
    if tick > replay.total_ticks {
        return Err(Error::Invalid(
            "renderer event tick exceeds replay endpoint".to_owned(),
        ));
    }
    let graph = &replay.renderer_graph;
    Ok(RendererEventIter {
        replay,
        tick,
        next_index: graph.offsets[tick as usize],
        end_index: graph.offsets[tick as usize + 1],
    })
}

impl<'a> Iterator for RendererEventIter<'a> {
    type Item = RendererEvent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_index >= self.end_index {
            return None;
        }
        let event_index = self.next_index;
        self.next_index += 1;
        let index = event_index as usize;
        let graph = &self.replay.renderer_graph;
        Some(RendererEvent {
            event_index,
            tick: self.tick,
            entity_id: optional_value(&graph.entity_ids, graph.columns[0][index])
                .map(String::as_str),
            opcode: RendererEventOpcode::try_from(graph.columns[1][index])
                .expect("validated opcode"),
            semantic_id: optional_value(&graph.semantic_ids, graph.columns[2][index])
                .map(String::as_str),
            payload: optional_value(&graph.payloads, graph.columns[3][index]),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.end_index - self.next_index) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for RendererEventIter<'_> {}
impl std::iter::FusedIterator for RendererEventIter<'_> {}

impl TryFrom<i32> for RendererEventOpcode {
    type Error = Error;

    fn try_from(value: i32) -> Result<Self> {
        match value {
            0 => Ok(Self::ActionFinish),
            1 => Ok(Self::ActionRun),
            2 => Ok(Self::ObjectAlpha),
            3 => Ok(Self::ObjectCreate),
            4 => Ok(Self::ObjectRemove),
            5 => Ok(Self::PreprocessorRun),
            6 => Ok(Self::ProcessorDestruct),
            7 => Ok(Self::ProcessorRun),
            _ => Err(Error::Invalid(format!(
                "unknown renderer event opcode {value}"
            ))),
        }
    }
}

fn verify_fingerprint(value: &Value, label: &str) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("{label} must be an object")))?;
    let expected = object
        .get("fingerprint")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid(format!("{label} fingerprint is missing")))?;
    if !is_sha256(expected) {
        return Err(Error::Invalid(format!("{label} fingerprint is invalid")));
    }
    let bytes = ecmascript_json_vec(&ObjectWithoutFingerprint(object))?;
    let actual = encode_hex(Sha256::digest(bytes).as_slice());
    if actual != expected {
        return Err(Error::Invalid(format!("{label} fingerprint mismatch")));
    }
    Ok(())
}

fn validate_required_nullable_fields(root: &serde_json::Map<String, Value>) -> Result<()> {
    let contract = root["rendererContract"]
        .as_object()
        .ok_or_else(|| Error::Invalid("renderer contract must be an object".to_owned()))?;
    if !contract.contains_key("rendererVersion") {
        return Err(Error::Invalid(
            "renderer contract lacks required rendererVersion".to_owned(),
        ));
    }
    let replay = root["replay"]
        .as_object()
        .ok_or_else(|| Error::Invalid("ReplayIR must be an object".to_owned()))?;
    for field in [
        "renderConfig",
        "rendererContractFingerprint",
        "randomSeed",
        "randomStateAtFirstTick",
    ] {
        if !replay.contains_key(field) {
            return Err(Error::Invalid(format!("ReplayIR lacks required {field}")));
        }
    }
    let timeline = replay
        .get("timeline")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Invalid("ReplayIR timeline must be an object".to_owned()))?;
    for field in [
        "framesPerSecond",
        "ticksPerSecond",
        "substepsPerSecond",
        "tickTransitionSeconds",
    ] {
        if !timeline.contains_key(field) {
            return Err(Error::Invalid(format!(
                "ReplayIR timeline lacks required {field}"
            )));
        }
    }
    Ok(())
}

struct ObjectWithoutFingerprint<'a>(&'a serde_json::Map<String, Value>);

impl serde::Serialize for ObjectWithoutFingerprint<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len().saturating_sub(1)))?;
        for (key, value) in self.0 {
            if key != "fingerprint" {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

fn validate_object_order(track: &Track) -> Result<()> {
    for value in &track.1 {
        let order = value
            .as_array()
            .ok_or_else(|| Error::Invalid("invalid ReplayIR object order value".to_owned()))?;
        let mut identities = BTreeSet::new();
        for identity in order {
            let identity = identity
                .as_str()
                .ok_or_else(|| Error::Invalid("invalid ReplayIR object order value".to_owned()))?;
            if !identities.insert(identity) {
                return Err(Error::Invalid(
                    "duplicate ReplayIR object order identity".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_sorted_indices(values: &[u32], length: usize, name: &str) -> Result<()> {
    if values.iter().any(|index| *index as usize >= length)
        || values.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(Error::Invalid(format!("invalid {name} track index")));
    }
    Ok(())
}

fn validate_sorted_unique(values: &[String], name: &str) -> Result<()> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::Invalid(format!(
            "renderer inventory {name} is not sorted and unique"
        )));
    }
    Ok(())
}

fn validate_unique_strings(values: &[String], name: &str) -> Result<()> {
    let mut unique = BTreeSet::new();
    if values.iter().any(|value| !unique.insert(value.as_str())) {
        return Err(Error::Invalid(format!("duplicate {name}")));
    }
    Ok(())
}

fn valid_optional_index(value: i32, length: usize) -> bool {
    value == -1 || value >= 0 && (value as usize) < length
}

fn valid_event_shape(entity: i32, opcode: i32, semantic: i32) -> bool {
    match opcode {
        // object:alpha/create/remove
        2..=4 => entity >= 0 && semantic == -1,
        // preprocessor:run
        5 => entity == -1 && semantic >= 0,
        // action:finish/run and processor:destruct/run
        0 | 1 | 6 | 7 => entity >= 0 && semantic >= 0,
        _ => false,
    }
}

fn optional_value<T>(values: &[T], index: i32) -> Option<&T> {
    (index >= 0).then(|| &values[index as usize])
}

fn is_sha256(value: &str) -> bool {
    value.len() == SHA256_HEX_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
pub(crate) mod tests {
    use serde::Serialize;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::{
        ObjectWithoutFingerprint, ReplayArtifact, TrackValue, ecmascript_json_vec, encode_hex,
    };

    pub(crate) fn signed(mut value: Value) -> Value {
        let object = value.as_object_mut().unwrap();
        let bytes = ecmascript_json_vec(&ObjectWithoutFingerprint(object)).unwrap();
        let fingerprint = encode_hex(Sha256::digest(bytes).as_slice());
        object.insert("fingerprint".to_owned(), Value::String(fingerprint));
        value
    }

    pub(crate) fn artifact_json() -> Vec<u8> {
        let contract = signed(json!({
            "schema": "screeps-arena-renderer-contract",
            "version": 5,
            "rendererVersion": "test",
            "metadata": {
                "layers": [],
                "objects": {
                    "unit": {
                        "actions": [],
                        "calculations": [],
                        "processors": []
                    }
                },
                "preprocessors": []
            },
            "resources": {},
            "decorations": [],
            "terrain": [],
            "worldOptions": {},
            "inventory": {
                "objectTypes": ["unit"],
                "processorTypes": [],
                "actionTypes": [],
                "preprocessors": [],
                "calculationIds": [],
                "drawingMethods": [],
                "expressionOperators": [],
                "functionSemantics": [],
                "layerIds": [],
                "rendererImplementationFingerprints": []
            }
        }));
        let contract_fingerprint = contract["fingerprint"].clone();
        let replay = signed(json!({
            "schema": "screeps-arena-replay-ir",
            "version": 8,
            "totalTicks": 1,
            "timeline": {
                "framesPerSecond": "2",
                "ticksPerSecond": "3",
                "substepsPerSecond": "12",
                "tickTransitionSeconds": "1/4"
            },
            "renderConfig": {
                "width": 640,
                "height": 480,
                "backgroundColor": 1645345,
                "boardFrame": {
                    "mode": "auto",
                    "outputWidth": 640,
                    "outputHeight": 480,
                    "boardWidth": 10000,
                    "boardHeight": 10000,
                    "worldMinX": -50,
                    "worldMinY": -50,
                    "pivotX": -50,
                    "pivotY": -50,
                    "zoom": 0.0448,
                    "x": 96,
                    "y": 16,
                    "left": 96,
                    "top": 16,
                    "right": 544,
                    "bottom": 464,
                    "width": 448,
                    "height": 448,
                    "padding": 16,
                    "panX": 0,
                    "panY": 0
                }
            },
            "rendererContractFingerprint": contract_fingerprint,
            "randomSeed": "test",
            "randomStateAtFirstTick": 123,
            "calculationOutputs": {"enabled": true},
            "rendererGraph": {
                "columns": [[0], [3], [-1], [-1]],
                "enabled": true,
                "entityIds": ["one"],
                "offsets": [0, 1, 1],
                "payloads": [],
                "semanticIds": []
            },
            "globalState": {},
            "visualOverlay": {"enabled": false, "states": [[0, 2], [[]], [], [], []]},
            "objectOrder": [[0, 2], [["one"]], [], [], []],
            "entities": [{
                "id": "one",
                "lifetimes": [[0, 2]],
                "properties": {
                    "value": [[0, 1, 1, 2], [null, 7], [], [0], []]
                },
                "calculations": {}
            }],
            "actionEvents": [],
            "tickFingerprints": []
        }));
        serde_json::to_vec(&json!({
            "rendererContract": contract,
            "replay": replay
        }))
        .unwrap()
    }

    #[test]
    fn loads_signed_artifact_and_indexes_tracks_and_events() {
        let indexed = ReplayArtifact::from_slice(&artifact_json())
            .unwrap()
            .into_indexed();
        let entity = indexed.entity("one").unwrap();
        assert!(entity.alive_at(0));
        assert_eq!(
            entity.properties["value"].at(0),
            Some(TrackValue::Undefined)
        );
        assert_eq!(
            entity.properties["value"].at(1),
            Some(TrackValue::Value(&json!(7)))
        );
        let mut events = indexed.events_at(0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events.next().unwrap().event_index, 0);
        assert_eq!(events.len(), 0);
        assert_eq!(indexed.events_at(1).unwrap().len(), 0);
    }

    #[test]
    fn validates_non_finite_calculation_sidecars_only_on_calculation_tracks() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["replay"]["calculationOutputs"]["enabled"] = json!(true);
        root["replay"]["entities"][0]["calculations"]["nan"] =
            json!([[0, 2], [null], [], [], [[0, "", 0]]]);
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());
        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        assert_eq!(
            artifact.replay.entities[0].calculations["nan"]
                .non_finite_at(1)
                .unwrap(),
            vec![("", 0)]
        );

        let mut invalid = root.clone();
        invalid["replay"]["entities"][0]["properties"]["value"][4] = json!([[1, "", 0]]);
        invalid["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        invalid["replay"] = signed(invalid["replay"].take());
        assert!(ReplayArtifact::from_slice(&serde_json::to_vec(&invalid).unwrap()).is_err());

        let mut invalid_path = root;
        invalid_path["replay"]["entities"][0]["calculations"]["nan"] =
            json!([[0, 2], [{"a": null}], [], [], [[0, "/a/b", 0]]]);
        invalid_path["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        invalid_path["replay"] = signed(invalid_path["replay"].take());
        assert!(ReplayArtifact::from_slice(&serde_json::to_vec(&invalid_path).unwrap()).is_err());
    }

    #[test]
    fn rejects_malformed_function_semantic_fingerprints() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["inventory"]["functionSemantics"] =
            json!(["objectFilter:not-a-digest"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        let contract_fingerprint = root["rendererContract"]["fingerprint"].clone();
        root["replay"]["rendererContractFingerprint"] = contract_fingerprint;
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        assert!(ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).is_err());
    }

    #[test]
    fn rejects_tampering_and_noncanonical_json() {
        let bytes = artifact_json();
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["replay"]["totalTicks"] = json!(2);
        assert!(ReplayArtifact::from_slice(&serde_json::to_vec(&value).unwrap()).is_err());

        let mut with_newline = bytes.clone();
        with_newline.push(b'\n');
        assert!(ReplayArtifact::from_slice(&with_newline).is_ok());
        with_newline.push(b'\n');
        assert!(ReplayArtifact::from_slice(&with_newline).is_err());
    }

    #[test]
    fn ecmascript_canonicalization_rounds_integer_tokens_to_binary64() {
        let value: Value = serde_json::from_str("{\"n\":9007199254740993}").unwrap();
        assert_eq!(
            ecmascript_json_vec(&value).unwrap(),
            br#"{"n":9007199254740992}"#
        );

        let mut artifact: Value = serde_json::from_slice(&artifact_json()).unwrap();
        artifact["rendererContract"]["metadata"]["unsafeInteger"] = value["n"].clone();
        assert!(matches!(
            ReplayArtifact::from_slice(&serde_json::to_vec(&artifact).unwrap()),
            Err(crate::Error::NonCanonicalJson)
        ));
    }

    #[test]
    fn nullable_schema_fields_are_required_and_empty_ids_remain_valid() {
        for field in ["randomSeed", "randomStateAtFirstTick"] {
            let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
            root["replay"].as_object_mut().unwrap().remove(field);
            root["replay"]
                .as_object_mut()
                .unwrap()
                .remove("fingerprint");
            root["replay"] = signed(root["replay"].take());
            assert!(
                ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).is_err(),
                "{field}"
            );
        }
        let mut missing_timeline: Value = serde_json::from_slice(&artifact_json()).unwrap();
        missing_timeline["replay"]["timeline"]
            .as_object_mut()
            .unwrap()
            .remove("substepsPerSecond");
        missing_timeline["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        missing_timeline["replay"] = signed(missing_timeline["replay"].take());
        assert!(
            ReplayArtifact::from_slice(&serde_json::to_vec(&missing_timeline).unwrap()).is_err()
        );

        let mut empty: Value = serde_json::from_slice(&artifact_json()).unwrap();
        empty["replay"]["entities"][0]["id"] = json!("");
        empty["replay"]["objectOrder"][1][0][0] = json!("");
        empty["replay"]["rendererGraph"]["entityIds"][0] = json!("");
        empty["replay"]["actionEvents"] = json!([[0, "", ""]]);
        empty["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        empty["replay"] = signed(empty["replay"].take());
        assert!(ReplayArtifact::from_slice(&serde_json::to_vec(&empty).unwrap()).is_ok());
    }

    #[test]
    fn synthetic_fixture_is_serializable_without_custom_number_rules() {
        // This ensures the preserve_order-backed serializer used by fingerprint
        // verification is the same serializer used for canonical input bytes.
        let value: Value = serde_json::from_slice(&artifact_json()).unwrap();
        assert_eq!(serde_json::to_vec(&value).unwrap(), artifact_json());
        let _ = value.serialize(serde_json::value::Serializer).unwrap();
    }
}
