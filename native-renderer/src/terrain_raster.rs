use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Error, RendererContract, Result, TerrainGeometry, TerrainGeometryTimeline};

const TERRAIN_RASTER_VERSION: u32 = 2;
const MAX_RASTER_DIMENSION: u32 = 4_096;
const MAX_TOTAL_MASK_BYTES: usize = 256 * 1024 * 1024;
const MAX_MEMORY_CACHE_BYTES: usize = 128 * 1024 * 1024;
const CACHE_MAGIC: &[u8; 8] = b"SAVTRF02";
const CHECKSUM_BYTES: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainRasterMask {
    pub width: u32,
    pub height: u32,
    /// Content identity including source path, view box, raster extent, and
    /// fill/stroke style. GPU banks use this to deduplicate layers without
    /// rehashing multi-megabyte alpha planes.
    pub fingerprint: String,
    /// Straight-alpha coverage. Terrain colors and tiled textures are applied
    /// later so one mask can feed terrain, lighting, and effects phases.
    pub alpha: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainRasterMasks {
    pub width: u32,
    pub height: u32,
    pub wall: Option<Arc<TerrainRasterMask>>,
    pub wall_stroke: Option<Arc<TerrainRasterMask>>,
    pub swamp: Option<Arc<TerrainRasterMask>>,
    pub swamp_stroke: Option<Arc<TerrainRasterMask>>,
    pub private_ramparts: BTreeMap<String, Arc<TerrainRasterMask>>,
    pub private_rampart_strokes: BTreeMap<String, Arc<TerrainRasterMask>>,
    pub fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainRasterStyle {
    pub wall_stroke_width: f64,
    pub swamp_stroke_width: f64,
    pub private_rampart_stroke_width: f64,
}

impl Default for TerrainRasterStyle {
    fn default() -> Self {
        Self {
            wall_stroke_width: 10.0,
            swamp_stroke_width: 50.0,
            private_rampart_stroke_width: 25.0,
        }
    }
}

impl TerrainRasterStyle {
    pub fn from_contract(contract: &RendererContract) -> Result<Self> {
        let wall_decoration =
            find_landscape(&contract.decorations, &["landscape", "wallLandscape"]);
        let floor_decoration =
            find_landscape(&contract.decorations, &["landscape", "floorLandscape"]);
        Ok(Self {
            wall_stroke_width: decoration_width(wall_decoration, "strokeWidth", 10.0)?,
            swamp_stroke_width: decoration_width(floor_decoration, "swampStrokeWidth", 50.0)?,
            private_rampart_stroke_width: 25.0,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum CoverageSpec {
    Fill,
    Stroke(f64),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerrainRasterCacheStats {
    pub component_requests: usize,
    pub memory_hits: usize,
    pub disk_hits: usize,
    pub rasterized: usize,
    pub streamed: usize,
    pub resident_components: usize,
    pub resident_bytes: usize,
    pub peak_resident_bytes: usize,
    pub evictions: usize,
}

#[derive(Debug)]
struct CachedComponent {
    mask: Arc<TerrainRasterMask>,
    stamp: u64,
}

#[derive(Debug)]
pub struct TerrainRasterCache {
    directory: Option<PathBuf>,
    max_memory_bytes: usize,
    components: HashMap<String, CachedComponent>,
    expected_uses: HashMap<String, usize>,
    disk_admissions: HashSet<String>,
    planned: bool,
    lru: VecDeque<(String, u64)>,
    next_stamp: u64,
    stats: TerrainRasterCacheStats,
}

impl TerrainRasterCache {
    pub fn new(directory: Option<PathBuf>) -> Result<Self> {
        Self::with_capacity(directory, MAX_MEMORY_CACHE_BYTES)
    }

    fn with_capacity(directory: Option<PathBuf>, max_memory_bytes: usize) -> Result<Self> {
        if let Some(directory) = &directory {
            fs::create_dir_all(directory)?;
        }
        Ok(Self {
            directory,
            max_memory_bytes,
            components: HashMap::new(),
            expected_uses: HashMap::new(),
            disk_admissions: HashSet::new(),
            planned: false,
            lru: VecDeque::new(),
            next_stamp: 1,
            stats: TerrainRasterCacheStats::default(),
        })
    }

    /// Supply the complete geometry set before loading it. Components used
    /// only once then stream through without occupying the reusable cache.
    pub fn plan<'a>(
        &mut self,
        geometries: impl IntoIterator<Item = &'a TerrainGeometry>,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.expected_uses.clear();
        self.disk_admissions.clear();
        self.planned = true;
        for geometry in geometries {
            validate_workload(geometry, width, height)?;
            for (path, spec) in coverage_jobs(geometry, None) {
                let fingerprint =
                    component_fingerprint(path, geometry.view_box, width, height, spec);
                let uses = self.expected_uses.entry(fingerprint).or_default();
                *uses = uses.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
            }
        }
        self.disk_admissions.extend(
            self.expected_uses
                .iter()
                .filter(|(_, uses)| **uses > 1)
                .map(|(fingerprint, _)| fingerprint.clone()),
        );
        Ok(())
    }

    /// Plan cache admission from exact half-open terrain spans. A component
    /// present for more than one replay tick is durable even when its geometry
    /// occurs only once in the unique-geometry table.
    pub fn plan_timeline(
        &mut self,
        timeline: &TerrainGeometryTimeline,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.plan_timeline_internal(timeline, width, height, None)
    }

    pub fn plan_timeline_styled(
        &mut self,
        timeline: &TerrainGeometryTimeline,
        width: u32,
        height: u32,
        style: TerrainRasterStyle,
    ) -> Result<()> {
        self.plan_timeline_internal(timeline, width, height, Some(style))
    }

    fn plan_timeline_internal(
        &mut self,
        timeline: &TerrainGeometryTimeline,
        width: u32,
        height: u32,
        style: Option<TerrainRasterStyle>,
    ) -> Result<()> {
        if let Some(style) = style {
            validate_style(style)?;
        }
        self.expected_uses.clear();
        self.disk_admissions.clear();
        self.planned = true;
        for geometry in timeline.geometries.values() {
            validate_workload_styled(geometry, width, height, style.is_some())?;
            for (path, spec) in coverage_jobs(geometry, style) {
                let fingerprint =
                    component_fingerprint(path, geometry.view_box, width, height, spec);
                let uses = self.expected_uses.entry(fingerprint).or_default();
                *uses = uses.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
            }
        }
        self.disk_admissions.extend(
            self.expected_uses
                .iter()
                .filter(|(_, uses)| **uses > 1)
                .map(|(fingerprint, _)| fingerprint.clone()),
        );
        let mut durations = HashMap::<String, u64>::new();
        for span in &timeline.spans {
            let geometry = timeline.geometries.get(&span.fingerprint).ok_or_else(|| {
                Error::Invalid("terrain span references an unknown geometry".to_owned())
            })?;
            let duration =
                u64::from(span.end_tick.checked_sub(span.start_tick).ok_or_else(|| {
                    Error::Invalid("terrain span ends before it starts".to_owned())
                })?);
            for (path, spec) in coverage_jobs(geometry, style) {
                let fingerprint =
                    component_fingerprint(path, geometry.view_box, width, height, spec);
                let total = durations.entry(fingerprint).or_default();
                *total = total
                    .checked_add(duration)
                    .ok_or(Error::ArithmeticOverflow)?;
            }
        }
        self.disk_admissions.extend(
            durations
                .into_iter()
                .filter(|(_, duration)| *duration > 1)
                .map(|(fingerprint, _)| fingerprint),
        );
        Ok(())
    }

    pub fn load(
        &mut self,
        geometry: &TerrainGeometry,
        width: u32,
        height: u32,
    ) -> Result<TerrainRasterMasks> {
        validate_workload(geometry, width, height)?;
        let wall = geometry
            .wall_path
            .as_deref()
            .map(|path| {
                self.load_component(path, geometry.view_box, width, height, CoverageSpec::Fill)
            })
            .transpose()?;
        let swamp = geometry
            .swamp_path
            .as_deref()
            .map(|path| {
                self.load_component(path, geometry.view_box, width, height, CoverageSpec::Fill)
            })
            .transpose()?;
        let private_ramparts = geometry
            .private_rampart_paths
            .iter()
            .map(|(user, path)| {
                self.load_component(path, geometry.view_box, width, height, CoverageSpec::Fill)
                    .map(|mask| (user.clone(), mask))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(TerrainRasterMasks {
            width,
            height,
            wall,
            wall_stroke: None,
            swamp,
            swamp_stroke: None,
            private_ramparts,
            private_rampart_strokes: BTreeMap::new(),
            fingerprint: raster_fingerprint(geometry, width, height),
        })
    }

    pub fn load_styled(
        &mut self,
        geometry: &TerrainGeometry,
        width: u32,
        height: u32,
        style: TerrainRasterStyle,
    ) -> Result<TerrainRasterMasks> {
        validate_style(style)?;
        validate_workload_styled(geometry, width, height, true)?;
        let mut masks = self.load(geometry, width, height)?;
        masks.wall_stroke = geometry
            .wall_path
            .as_deref()
            .map(|path| {
                self.load_component(
                    path,
                    geometry.view_box,
                    width,
                    height,
                    CoverageSpec::Stroke(style.wall_stroke_width),
                )
            })
            .transpose()?;
        masks.swamp_stroke = geometry
            .swamp_path
            .as_deref()
            .map(|path| {
                self.load_component(
                    path,
                    geometry.view_box,
                    width,
                    height,
                    CoverageSpec::Stroke(style.swamp_stroke_width),
                )
            })
            .transpose()?;
        masks.private_rampart_strokes = geometry
            .private_rampart_paths
            .iter()
            .map(|(user, path)| {
                self.load_component(
                    path,
                    geometry.view_box,
                    width,
                    height,
                    CoverageSpec::Stroke(style.private_rampart_stroke_width),
                )
                .map(|mask| (user.clone(), mask))
            })
            .collect::<Result<_>>()?;
        masks.fingerprint = styled_raster_fingerprint(geometry, width, height, style);
        Ok(masks)
    }

    pub const fn stats(&self) -> TerrainRasterCacheStats {
        self.stats
    }

    fn load_component(
        &mut self,
        source_path: &str,
        view_box: u32,
        width: u32,
        height: u32,
        spec: CoverageSpec,
    ) -> Result<Arc<TerrainRasterMask>> {
        self.stats.component_requests = self
            .stats
            .component_requests
            .checked_add(1)
            .ok_or(Error::ArithmeticOverflow)?;
        let fingerprint = component_fingerprint(source_path, view_box, width, height, spec);
        let should_admit_memory = self
            .expected_uses
            .get(&fingerprint)
            .is_none_or(|remaining| *remaining > 1);
        let should_publish_disk = !self.planned || self.disk_admissions.contains(&fingerprint);
        if self.components.contains_key(&fingerprint) {
            self.stats.memory_hits = self
                .stats
                .memory_hits
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow)?;
            let mask = self.touch(&fingerprint)?;
            self.finish_planned_use(&fingerprint)?;
            return Ok(mask);
        }
        let (mask, disk_hit) = match &self.directory {
            Some(directory) => load_component_cached(
                directory,
                source_path,
                view_box,
                width,
                height,
                should_publish_disk,
                spec,
            )?,
            None => (
                rasterize_path(source_path, view_box, width, height, spec)?,
                false,
            ),
        };
        if disk_hit {
            self.stats.disk_hits = self
                .stats
                .disk_hits
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow)?;
        } else {
            self.stats.rasterized = self
                .stats
                .rasterized
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow)?;
            if !should_publish_disk {
                self.stats.streamed = self
                    .stats
                    .streamed
                    .checked_add(1)
                    .ok_or(Error::ArithmeticOverflow)?;
            }
        }
        let mask = Arc::new(mask);
        if should_admit_memory {
            self.admit(fingerprint.clone(), Arc::clone(&mask))?;
        }
        self.finish_planned_use(&fingerprint)?;
        Ok(mask)
    }

    fn touch(&mut self, fingerprint: &str) -> Result<Arc<TerrainRasterMask>> {
        let stamp = self.allocate_stamp()?;
        let component = self
            .components
            .get_mut(fingerprint)
            .expect("component existence checked by caller");
        component.stamp = stamp;
        let mask = Arc::clone(&component.mask);
        self.lru.push_back((fingerprint.to_owned(), stamp));
        Ok(mask)
    }

    fn admit(&mut self, fingerprint: String, mask: Arc<TerrainRasterMask>) -> Result<()> {
        let bytes = mask.alpha.len();
        if bytes > self.max_memory_bytes {
            return Ok(());
        }
        while self
            .stats
            .resident_bytes
            .checked_add(bytes)
            .ok_or(Error::ArithmeticOverflow)?
            > self.max_memory_bytes
        {
            if !self.evict_oldest()? {
                return Ok(());
            }
        }
        let stamp = self.allocate_stamp()?;
        self.stats.resident_bytes = self
            .stats
            .resident_bytes
            .checked_add(bytes)
            .ok_or(Error::ArithmeticOverflow)?;
        self.stats.peak_resident_bytes = self
            .stats
            .peak_resident_bytes
            .max(self.stats.resident_bytes);
        self.components
            .insert(fingerprint.clone(), CachedComponent { mask, stamp });
        self.lru.push_back((fingerprint, stamp));
        self.stats.resident_components = self.components.len();
        Ok(())
    }

    fn evict_oldest(&mut self) -> Result<bool> {
        while let Some((fingerprint, stamp)) = self.lru.pop_front() {
            let is_current = self
                .components
                .get(&fingerprint)
                .is_some_and(|component| component.stamp == stamp);
            if !is_current {
                continue;
            }
            self.remove_component(&fingerprint)?;
            self.stats.evictions = self
                .stats
                .evictions
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn finish_planned_use(&mut self, fingerprint: &str) -> Result<()> {
        let exhausted = match self.expected_uses.get_mut(fingerprint) {
            Some(remaining) => {
                *remaining = remaining.checked_sub(1).ok_or(Error::ArithmeticOverflow)?;
                *remaining == 0
            }
            None => false,
        };
        if exhausted {
            self.expected_uses.remove(fingerprint);
            self.remove_component(fingerprint)?;
        }
        Ok(())
    }

    fn remove_component(&mut self, fingerprint: &str) -> Result<()> {
        if let Some(component) = self.components.remove(fingerprint) {
            self.stats.resident_bytes = self
                .stats
                .resident_bytes
                .checked_sub(component.mask.alpha.len())
                .ok_or(Error::ArithmeticOverflow)?;
            self.stats.resident_components = self.components.len();
        }
        Ok(())
    }

    fn allocate_stamp(&mut self) -> Result<u64> {
        let stamp = self.next_stamp;
        self.next_stamp = self
            .next_stamp
            .checked_add(1)
            .ok_or(Error::ArithmeticOverflow)?;
        Ok(stamp)
    }
}

impl TerrainRasterMasks {
    pub fn rasterize(geometry: &TerrainGeometry, width: u32, height: u32) -> Result<Self> {
        TerrainRasterCache::new(None)?.load(geometry, width, height)
    }

    /// Load independently content-addressed path masks, rebuilding each
    /// component atomically when absent or corrupt. Unchanged wall/swamp masks
    /// are therefore shared even when one owner's rampart geometry changes.
    pub fn load_or_rasterize_cached(
        geometry: &TerrainGeometry,
        width: u32,
        height: u32,
        cache_directory: impl AsRef<Path>,
    ) -> Result<(Self, bool)> {
        let mut cache = TerrainRasterCache::new(Some(cache_directory.as_ref().to_path_buf()))?;
        let masks = cache.load(geometry, width, height)?;
        Ok((masks, cache.stats().rasterized == 0))
    }
}

fn load_component_cached(
    directory: &Path,
    source_path: &str,
    view_box: u32,
    width: u32,
    height: u32,
    publish_on_miss: bool,
    spec: CoverageSpec,
) -> Result<(TerrainRasterMask, bool)> {
    let fingerprint = component_fingerprint(source_path, view_box, width, height, spec);
    let path = directory.join(format!("{fingerprint}.terrain-coverage-v2"));
    if let Ok(mask) = read_component(&path, width, height, &fingerprint) {
        return Ok((mask, true));
    }
    if !publish_on_miss {
        return Ok((
            rasterize_path(source_path, view_box, width, height, spec)?,
            false,
        ));
    }
    let lock_path = directory.join(format!(".{fingerprint}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock_exclusive(&lock)?;
    if let Ok(mask) = read_component(&path, width, height, &fingerprint) {
        return Ok((mask, true));
    }
    let mask = rasterize_path(source_path, view_box, width, height, spec)?;
    write_component_atomic(&path, &fingerprint, &mask)?;
    Ok((mask, false))
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        // SAFETY: `file` owns a live descriptor for the duration of this call.
        // `flock` does not access Rust memory, and the lock is released by the
        // kernel when this file description is closed, including after a crash.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "terrain disk-cache coordination requires an OS file-lock implementation",
    ))
}

fn unix_nanos() -> Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|_| Error::Invalid("system clock precedes the Unix epoch".to_owned()))
}

fn read_component(
    path: &Path,
    width: u32,
    height: u32,
    expected_fingerprint: &str,
) -> std::io::Result<TerrainRasterMask> {
    let pixel_count = width as usize * height as usize;
    let expected_bytes = 8 + 64 + 8 + pixel_count + CHECKSUM_BYTES;
    let metadata = fs::metadata(path)?;
    if usize::try_from(metadata.len()).ok() != Some(expected_bytes) {
        return Err(invalid_cache("terrain mask cache byte length is invalid"));
    }
    let bytes = fs::read(path)?;
    let (payload, checksum) = bytes.split_at(bytes.len() - CHECKSUM_BYTES);
    if Sha256::digest(payload).as_slice() != checksum {
        return Err(invalid_cache("terrain mask cache checksum is invalid"));
    }
    let mut cursor = CacheCursor::new(payload);
    if cursor.bytes(8)? != CACHE_MAGIC {
        return Err(invalid_cache("terrain mask cache magic is invalid"));
    }
    if cursor.string(64)? != expected_fingerprint
        || cursor.u32()? != width
        || cursor.u32()? != height
    {
        return Err(invalid_cache("terrain mask cache identity is invalid"));
    }
    let alpha = cursor.bytes(pixel_count)?.to_vec();
    if !cursor.remaining().is_empty() {
        return Err(invalid_cache(
            "terrain mask cache has trailing payload bytes",
        ));
    }
    Ok(TerrainRasterMask {
        width,
        height,
        fingerprint: expected_fingerprint.to_owned(),
        alpha,
    })
}

fn write_component_atomic(path: &Path, fingerprint: &str, mask: &TerrainRasterMask) -> Result<()> {
    let mut payload = Vec::new();
    payload.extend_from_slice(CACHE_MAGIC);
    payload.extend_from_slice(fingerprint.as_bytes());
    payload.extend_from_slice(&mask.width.to_le_bytes());
    payload.extend_from_slice(&mask.height.to_le_bytes());
    payload.extend_from_slice(&mask.alpha);
    let checksum = Sha256::digest(&payload);
    let (temporary_path, mut file) = create_temporary(path)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(&payload)?;
        file.write_all(checksum.as_slice())?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn create_temporary(path: &Path) -> Result<(PathBuf, File)> {
    let parent = path.parent().ok_or_else(|| {
        Error::Invalid("terrain mask cache path must have a parent directory".to_owned())
    })?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| Error::Invalid("terrain mask cache path is invalid".to_owned()))?;
    let nonce = unix_nanos()?;
    for attempt in 0..32u32 {
        let candidate = parent.join(format!(
            ".{filename}.partial-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Invalid(
        "could not allocate a terrain mask cache temporary file".to_owned(),
    ))
}

struct CacheCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> CacheCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    fn bytes(&mut self, count: usize) -> std::io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| invalid_cache("terrain mask cache is truncated"))?;
        let bytes = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> std::io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().expect("four-byte cursor read"),
        ))
    }

    fn string(&mut self, count: usize) -> std::io::Result<String> {
        String::from_utf8(self.bytes(count)?.to_vec())
            .map_err(|_| invalid_cache("terrain mask cache string is invalid"))
    }
}

fn invalid_cache(message: &str) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, message)
}

fn rasterize_path(
    path: &str,
    view_box: u32,
    width: u32,
    height: u32,
    spec: CoverageSpec,
) -> Result<TerrainRasterMask> {
    let attributes = match spec {
        CoverageSpec::Fill => r##"fill="#fff""##.to_owned(),
        CoverageSpec::Stroke(stroke_width) => {
            format!(r##"fill="none" stroke="#fff" stroke-width="{stroke_width}""##)
        }
    };
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {view_box} {view_box}" shape-rendering="optimizeSpeed"><path {attributes} d="{path}"/></svg>"##
    );
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())
        .map_err(|error| Error::Invalid(format!("generated terrain SVG is invalid: {error}")))?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or_else(|| {
        Error::Invalid("generated terrain mask has invalid dimensions".to_owned())
    })?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or(Error::ArithmeticOverflow)?;
    let mut alpha = Vec::with_capacity(pixel_count);
    alpha.extend(pixmap.data().chunks_exact(4).map(|pixel| pixel[3]));
    Ok(TerrainRasterMask {
        width,
        height,
        fingerprint: component_fingerprint(path, view_box, width, height, spec),
        alpha,
    })
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_RASTER_DIMENSION || height > MAX_RASTER_DIMENSION {
        return Err(Error::Invalid(format!(
            "terrain raster dimensions must be within 1..={MAX_RASTER_DIMENSION}"
        )));
    }
    (width as usize)
        .checked_mul(height as usize)
        .ok_or(Error::ArithmeticOverflow)?;
    Ok(())
}

fn find_landscape<'a>(decorations: &'a [Value], types: &[&str]) -> Option<&'a Value> {
    decorations.iter().find(|item| {
        item.get("decoration")
            .and_then(|decoration| decoration.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| types.contains(&kind))
    })
}

fn decoration_width(decoration: Option<&Value>, field: &str, default: f64) -> Result<f64> {
    let Some(decoration) = decoration else {
        return Ok(default);
    };
    let width = decoration
        .get(field)
        .and_then(Value::as_f64)
        .filter(|width| width.is_finite() && *width >= 0.0 && *width <= 10_000.0)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "renderer landscape decoration {field} must be finite and within 0..=10000"
            ))
        })?;
    Ok(width)
}

fn validate_style(style: TerrainRasterStyle) -> Result<()> {
    for (field, width) in [
        ("wallStrokeWidth", style.wall_stroke_width),
        ("swampStrokeWidth", style.swamp_stroke_width),
        (
            "privateRampartStrokeWidth",
            style.private_rampart_stroke_width,
        ),
    ] {
        if !width.is_finite() || !(0.0..=10_000.0).contains(&width) {
            return Err(Error::Invalid(format!(
                "terrain {field} must be finite and within 0..=10000"
            )));
        }
    }
    Ok(())
}

fn coverage_jobs(
    geometry: &TerrainGeometry,
    style: Option<TerrainRasterStyle>,
) -> Vec<(&str, CoverageSpec)> {
    let mut jobs = Vec::with_capacity(
        (usize::from(geometry.wall_path.is_some())
            + usize::from(geometry.swamp_path.is_some())
            + geometry.private_rampart_paths.len())
            * if style.is_some() { 2 } else { 1 },
    );
    if let Some(path) = geometry.wall_path.as_deref() {
        jobs.push((path, CoverageSpec::Fill));
        if let Some(style) = style {
            jobs.push((path, CoverageSpec::Stroke(style.wall_stroke_width)));
        }
    }
    if let Some(path) = geometry.swamp_path.as_deref() {
        jobs.push((path, CoverageSpec::Fill));
        if let Some(style) = style {
            jobs.push((path, CoverageSpec::Stroke(style.swamp_stroke_width)));
        }
    }
    for path in geometry.private_rampart_paths.values().map(String::as_str) {
        jobs.push((path, CoverageSpec::Fill));
        if let Some(style) = style {
            jobs.push((
                path,
                CoverageSpec::Stroke(style.private_rampart_stroke_width),
            ));
        }
    }
    jobs
}

fn validate_workload(geometry: &TerrainGeometry, width: u32, height: u32) -> Result<()> {
    validate_workload_styled(geometry, width, height, false)
}

fn validate_workload_styled(
    geometry: &TerrainGeometry,
    width: u32,
    height: u32,
    styled: bool,
) -> Result<()> {
    validate_dimensions(width, height)?;
    let mask_count = usize::from(geometry.wall_path.is_some())
        .checked_add(usize::from(geometry.swamp_path.is_some()))
        .and_then(|value| value.checked_add(geometry.private_rampart_paths.len()))
        .ok_or(Error::ArithmeticOverflow)?;
    let mask_count = mask_count
        .checked_mul(if styled { 2 } else { 1 })
        .ok_or(Error::ArithmeticOverflow)?;
    let total_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(mask_count))
        .ok_or(Error::ArithmeticOverflow)?;
    if total_bytes > MAX_TOTAL_MASK_BYTES {
        return Err(Error::Invalid(format!(
            "terrain mask workload exceeds the {} MiB aggregate limit",
            MAX_TOTAL_MASK_BYTES / 1024 / 1024
        )));
    }
    Ok(())
}

fn raster_fingerprint(geometry: &TerrainGeometry, width: u32, height: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(TERRAIN_RASTER_VERSION.to_le_bytes());
    hasher.update(geometry.room_size.to_le_bytes());
    hasher.update(geometry.view_box.to_le_bytes());
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    hash_optional_path(&mut hasher, geometry.wall_path.as_deref());
    hash_optional_path(&mut hasher, geometry.swamp_path.as_deref());
    for (user, path) in &geometry.private_rampart_paths {
        hash_bytes(&mut hasher, user.as_bytes());
        hash_bytes(&mut hasher, path.as_bytes());
    }
    encode_hex(hasher.finalize().as_slice())
}

fn styled_raster_fingerprint(
    geometry: &TerrainGeometry,
    width: u32,
    height: u32,
    style: TerrainRasterStyle,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raster_fingerprint(geometry, width, height));
    hasher.update(style.wall_stroke_width.to_bits().to_le_bytes());
    hasher.update(style.swamp_stroke_width.to_bits().to_le_bytes());
    hasher.update(style.private_rampart_stroke_width.to_bits().to_le_bytes());
    encode_hex(hasher.finalize().as_slice())
}

fn component_fingerprint(
    path: &str,
    view_box: u32,
    width: u32,
    height: u32,
    spec: CoverageSpec,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(TERRAIN_RASTER_VERSION.to_le_bytes());
    hasher.update(view_box.to_le_bytes());
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    match spec {
        CoverageSpec::Fill => hasher.update([0]),
        CoverageSpec::Stroke(stroke_width) => {
            hasher.update([1]);
            hasher.update(stroke_width.to_bits().to_le_bytes());
        }
    }
    hash_bytes(&mut hasher, path.as_bytes());
    encode_hex(hasher.finalize().as_slice())
}

fn hash_optional_path(hasher: &mut Sha256, path: Option<&str>) {
    hasher.update([u8::from(path.is_some())]);
    if let Some(path) = path {
        hash_bytes(hasher, path.as_bytes());
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
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{
        TerrainGeometry, TerrainGeometrySpan, TerrainGeometryTimeline, TerrainRasterCache,
        TerrainRasterMasks, TerrainRasterStyle, TerrainSwampTexture,
    };

    fn geometry() -> TerrainGeometry {
        let path = "M 100 150 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z ".to_owned();
        TerrainGeometry {
            room_size: 5,
            view_box: 500,
            wall_path: Some(path.clone()),
            swamp_path: None,
            private_rampart_paths: BTreeMap::from([("owner".to_owned(), path)]),
            private_rampart_colors: BTreeMap::from([("owner".to_owned(), 0x83_2b_cd)]),
            swamp_texture: TerrainSwampTexture::Animated,
            fingerprint: "ab".repeat(32),
        }
    }

    #[test]
    fn rasterizes_separate_reusable_coverage_masks() {
        let masks = TerrainRasterMasks::rasterize(&geometry(), 50, 50).unwrap();
        let wall = masks.wall.as_ref().unwrap();
        assert_eq!(wall.alpha.len(), 2_500);
        assert_eq!(wall.alpha[15 * 50 + 15], 255);
        assert_eq!(wall.alpha[0], 0);
        assert!(masks.swamp.is_none());
        assert_eq!(
            masks.private_ramparts["owner"].alpha,
            masks.wall.as_ref().unwrap().alpha
        );
        assert_eq!(masks.fingerprint.len(), 64);
    }

    #[test]
    fn rasterizes_official_default_strokes_separately_from_fills() {
        let mut cache = TerrainRasterCache::new(None).unwrap();
        let masks = cache
            .load_styled(&geometry(), 50, 50, TerrainRasterStyle::default())
            .unwrap();
        let wall_stroke = masks.wall_stroke.as_ref().unwrap();
        assert_eq!(wall_stroke.alpha[15 * 50 + 15], 0);
        assert!(wall_stroke.alpha[15 * 50 + 10] > 0);
        assert!(masks.private_rampart_strokes.contains_key("owner"));
        assert_eq!(cache.stats().component_requests, 4);
        assert_eq!(cache.stats().memory_hits, 1);
        assert_eq!(cache.stats().rasterized, 3);
    }

    #[test]
    fn decoration_stroke_widths_follow_first_landscape_precedence() {
        let decorations = serde_json::json!([
            {
                "strokeWidth": 34,
                "swampStrokeWidth": 30,
                "decoration": {"type": "landscape"}
            },
            {
                "strokeWidth": 20,
                "decoration": {"type": "wallLandscape"}
            }
        ]);
        let decorations = decorations.as_array().unwrap();
        let wall = super::find_landscape(decorations, &["landscape", "wallLandscape"]);
        let floor = super::find_landscape(decorations, &["landscape", "floorLandscape"]);
        assert_eq!(
            super::decoration_width(wall, "strokeWidth", 10.0).unwrap(),
            34.0
        );
        assert_eq!(
            super::decoration_width(floor, "swampStrokeWidth", 50.0).unwrap(),
            30.0
        );
        assert!(
            super::decoration_width(
                Some(&serde_json::json!({"strokeWidth": "wide"})),
                "strokeWidth",
                10.0
            )
            .is_err()
        );
    }

    #[test]
    fn raster_identity_includes_dimensions_and_rejects_oversized_work() {
        let first = TerrainRasterMasks::rasterize(&geometry(), 50, 50).unwrap();
        let second = TerrainRasterMasks::rasterize(&geometry(), 51, 50).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint);
        assert!(TerrainRasterMasks::rasterize(&geometry(), 8_193, 1).is_err());
    }

    #[test]
    fn cache_round_trips_and_recovers_from_corruption() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("screeps-terrain-mask-test-{nonce}"));
        let geometry = geometry();
        let (built, hit) =
            TerrainRasterMasks::load_or_rasterize_cached(&geometry, 50, 50, &directory).unwrap();
        assert!(!hit);
        // Wall and rampart share one mask file. Its empty lock file is retained
        // so every process coordinates on the same inode.
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "terrain-coverage-v2"))
                .count(),
            1
        );
        let (loaded, hit) =
            TerrainRasterMasks::load_or_rasterize_cached(&geometry, 50, 50, &directory).unwrap();
        assert!(hit);
        assert_eq!(loaded, built);

        let path = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "terrain-coverage-v2")
            })
            .unwrap()
            .path();
        fs::write(path, b"corrupt").unwrap();
        let (recovered, hit) =
            TerrainRasterMasks::load_or_rasterize_cached(&geometry, 50, 50, &directory).unwrap();
        assert!(!hit);
        assert_eq!(recovered, built);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cache_identity_hashes_paths_instead_of_trusting_geometry_fingerprint() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("screeps-terrain-identity-test-{nonce}"));
        let first = geometry();
        let mut second = geometry();
        second.wall_path = Some(
            "M 200 250 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
                .to_owned(),
        );
        assert_eq!(first.fingerprint, second.fingerprint);

        let (first_masks, first_hit) =
            TerrainRasterMasks::load_or_rasterize_cached(&first, 50, 50, &directory).unwrap();
        let (second_masks, second_hit) =
            TerrainRasterMasks::load_or_rasterize_cached(&second, 50, 50, &directory).unwrap();
        assert!(!first_hit);
        assert!(!second_hit);
        assert_ne!(
            first_masks.wall.unwrap().alpha,
            second_masks.wall.unwrap().alpha
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn memory_cache_shares_identical_components_without_copying() {
        let mut cache = TerrainRasterCache::new(None).unwrap();
        let masks = cache.load(&geometry(), 50, 50).unwrap();
        let wall = masks.wall.as_ref().unwrap();
        let rampart = &masks.private_ramparts["owner"];
        assert!(std::sync::Arc::ptr_eq(wall, rampart));
        assert_eq!(
            cache.stats(),
            super::TerrainRasterCacheStats {
                component_requests: 2,
                memory_hits: 1,
                disk_hits: 0,
                rasterized: 1,
                streamed: 0,
                resident_components: 1,
                resident_bytes: 2_500,
                peak_resident_bytes: 2_500,
                evictions: 0,
            }
        );
    }

    #[test]
    fn memory_cache_reuses_static_components_across_geometry_changes() {
        let mut cache = TerrainRasterCache::new(None).unwrap();
        let first = geometry();
        let mut second = geometry();
        second.private_rampart_paths.insert(
            "owner".to_owned(),
            "M 200 250 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
                .to_owned(),
        );
        cache.plan([&first, &second], 50, 50).unwrap();
        let first_masks = cache.load(&first, 50, 50).unwrap();
        let second_masks = cache.load(&second, 50, 50).unwrap();

        assert!(std::sync::Arc::ptr_eq(
            first_masks.wall.as_ref().unwrap(),
            second_masks.wall.as_ref().unwrap()
        ));
        assert_eq!(cache.stats().component_requests, 4);
        assert_eq!(cache.stats().memory_hits, 2);
        assert_eq!(cache.stats().rasterized, 2);
        assert_eq!(cache.stats().resident_components, 0);
        assert_eq!(cache.stats().resident_bytes, 0);
        assert_eq!(cache.stats().peak_resident_bytes, 2_500);
    }

    #[test]
    fn workers_share_one_disk_raster_under_an_os_released_lock() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("screeps-terrain-lock-test-{nonce}"));
        fs::create_dir_all(&directory).unwrap();
        let barrier = Arc::new(Barrier::new(4));
        let workers = (0..4)
            .map(|_| {
                let worker_directory = directory.clone();
                let worker_barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    worker_barrier.wait();
                    let mut cache = TerrainRasterCache::new(Some(worker_directory)).unwrap();
                    cache.load(&geometry(), 50, 50).unwrap();
                    cache.stats()
                })
            })
            .collect::<Vec<_>>();
        let stats = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(stats.iter().map(|stats| stats.rasterized).sum::<usize>(), 1);
        assert_eq!(stats.iter().map(|stats| stats.disk_hits).sum::<usize>(), 3);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn planned_one_shot_miss_streams_without_disk_publication() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("screeps-terrain-stream-test-{nonce}"));
        let mut geometry = geometry();
        geometry.private_rampart_paths.clear();
        let mut cache = TerrainRasterCache::new(Some(directory.clone())).unwrap();
        cache.plan([&geometry], 50, 50).unwrap();
        cache.load(&geometry, 50, 50).unwrap();

        assert_eq!(cache.stats().rasterized, 1);
        assert_eq!(cache.stats().streamed, 1);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn long_lived_unique_geometry_is_published_for_cross_replay_reuse() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("screeps-terrain-durable-test-{nonce}"));
        let mut geometry = geometry();
        geometry.private_rampart_paths.clear();
        let fingerprint = geometry.fingerprint.clone();
        let timeline = TerrainGeometryTimeline {
            geometries: BTreeMap::from([(fingerprint.clone(), geometry)]),
            spans: vec![TerrainGeometrySpan {
                start_tick: 0,
                end_tick: 2_001,
                fingerprint,
                swamp_animation_start_tick: 0,
            }],
        };
        let mut cache = TerrainRasterCache::new(Some(directory.clone())).unwrap();
        cache.plan_timeline(&timeline, 50, 50).unwrap();
        cache
            .load(timeline.geometries.values().next().unwrap(), 50, 50)
            .unwrap();

        assert_eq!(cache.stats().rasterized, 1);
        assert_eq!(cache.stats().streamed, 0);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recurring_one_tick_spans_are_durable_by_total_duration() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("screeps-terrain-recurring-test-{nonce}"));
        let mut first = geometry();
        first.private_rampart_paths.clear();
        first.fingerprint = "11".repeat(32);
        let mut second = geometry();
        second.private_rampart_paths.clear();
        second.wall_path = Some(
            "M 200 250 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
                .to_owned(),
        );
        second.fingerprint = "22".repeat(32);
        let timeline = TerrainGeometryTimeline {
            geometries: BTreeMap::from([
                (first.fingerprint.clone(), first),
                (second.fingerprint.clone(), second),
            ]),
            spans: vec![
                TerrainGeometrySpan {
                    start_tick: 0,
                    end_tick: 1,
                    fingerprint: "11".repeat(32),
                    swamp_animation_start_tick: 0,
                },
                TerrainGeometrySpan {
                    start_tick: 1,
                    end_tick: 2,
                    fingerprint: "22".repeat(32),
                    swamp_animation_start_tick: 1,
                },
                TerrainGeometrySpan {
                    start_tick: 2,
                    end_tick: 3,
                    fingerprint: "11".repeat(32),
                    swamp_animation_start_tick: 2,
                },
            ],
        };
        let mut cache = TerrainRasterCache::new(Some(directory.clone())).unwrap();
        cache.plan_timeline(&timeline, 50, 50).unwrap();
        for geometry in timeline.geometries.values() {
            cache.load(geometry, 50, 50).unwrap();
        }

        assert_eq!(cache.stats().rasterized, 2);
        assert_eq!(cache.stats().streamed, 1);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lru_bounds_the_union_of_unplanned_dynamic_components() {
        let mut cache = TerrainRasterCache::with_capacity(None, 2_500).unwrap();
        let first = geometry();
        cache.load(&first, 50, 50).unwrap();
        let mut second = geometry();
        let second_path =
            "M 200 250 a 50 50 0 0 1 50 -50 a 50 50 0 0 1 50 50 a 50 50 0 0 1 -50 50 a 50 50 0 0 1 -50 -50 Z "
                .to_owned();
        second.wall_path = Some(second_path.clone());
        second
            .private_rampart_paths
            .insert("owner".to_owned(), second_path);
        cache.load(&second, 50, 50).unwrap();

        assert_eq!(cache.stats().resident_components, 1);
        assert_eq!(cache.stats().resident_bytes, 2_500);
        assert_eq!(cache.stats().peak_resident_bytes, 2_500);
        assert_eq!(cache.stats().evictions, 1);
    }
}
