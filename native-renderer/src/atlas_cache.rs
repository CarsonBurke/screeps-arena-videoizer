use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::{
    AtlasEntry, AtlasOptions, AtlasRasterAsset, Error, RendererContract, Result, TextureAtlas,
    TextureAtlasPage, assets::expected_atlas_asset_names,
};

const MAGIC: &[u8; 8] = b"SAVATL06";
const CHECKSUM_BYTES: usize = 32;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;

impl TextureAtlas {
    /// Load a content-addressed atlas cache, rebuilding it atomically when the
    /// cache is absent, stale, truncated, or otherwise invalid.
    pub fn load_or_build_cached(
        contract: &RendererContract,
        options: AtlasOptions,
        cache_directory: impl AsRef<Path>,
    ) -> Result<(Self, bool)> {
        Self::load_or_build_cached_with_raster_assets(
            contract,
            options,
            Vec::new(),
            cache_directory,
        )
    }

    /// Cache a combined resource/procedural atlas by the exact raster bytes.
    /// Replay-varying graphics therefore retain the expensive decoded resource
    /// cache without ever reusing geometry from another replay.
    pub fn load_or_build_cached_with_raster_assets(
        contract: &RendererContract,
        options: AtlasOptions,
        raster_assets: Vec<AtlasRasterAsset>,
        cache_directory: impl AsRef<Path>,
    ) -> Result<(Self, bool)> {
        options.validate()?;
        let (filename, expected_names) = raster_cache_identity(contract, options, &raster_assets)?;
        let directory = cache_directory.as_ref();
        fs::create_dir_all(directory)?;
        let path = directory.join(filename);
        if let Ok(atlas) = read_cache(&path, contract, options, &expected_names) {
            return Ok((atlas, true));
        }
        let lock_path = directory.join(format!(
            ".{}.lock",
            path.file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| Error::Invalid("atlas cache path is invalid".to_owned()))?
        ));
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        lock_exclusive(&lock)?;
        if let Ok(atlas) = read_cache(&path, contract, options, &expected_names) {
            return Ok((atlas, true));
        }
        let atlas = Self::build_with_raster_assets(contract, options, raster_assets)?;
        write_cache_atomic(&path, contract, options, &atlas)?;
        Ok((atlas, false))
    }
}

pub fn atlas_cache_filename(contract: &RendererContract, options: AtlasOptions) -> String {
    let mut hasher = Sha256::new();
    hasher.update(options.svg_scale.to_bits().to_le_bytes());
    hasher.update(options.max_asset_dimension.to_le_bytes());
    hasher.update(options.max_texture_dimension.to_le_bytes());
    hasher.update(options.padding.to_le_bytes());
    let suffix = hex(&hasher.finalize()[..8]);
    format!("{}-{suffix}.atlas-v6", contract.fingerprint)
}

fn read_cache(
    path: &Path,
    contract: &RendererContract,
    options: AtlasOptions,
    expected_names: &std::collections::BTreeSet<String>,
) -> std::io::Result<TextureAtlas> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_CACHE_BYTES as u64 || metadata.len() < CHECKSUM_BYTES as u64 {
        return Err(invalid_cache("atlas cache exceeds the byte limit"));
    }
    let mut reader = LimitedReader::new(File::open(path)?, metadata.len() as usize);
    if reader.bytes(8)? != MAGIC {
        return Err(invalid_cache("atlas cache magic is invalid"));
    }
    if reader.string(64)? != contract.fingerprint {
        return Err(invalid_cache("atlas cache renderer fingerprint is invalid"));
    }
    if reader.u32()? != options.svg_scale.to_bits()
        || reader.u32()? != options.max_asset_dimension
        || reader.u32()? != options.max_texture_dimension
        || reader.u32()? != options.padding
    {
        return Err(invalid_cache("atlas cache options are invalid"));
    }

    let entry_count = reader.u32()? as usize;
    if entry_count != expected_names.len() {
        return Err(invalid_cache("atlas cache resource count is invalid"));
    }
    let maximum_name_length = expected_names.iter().map(String::len).max().unwrap_or(0);
    let mut raw_entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let name_length = reader.u32()? as usize;
        if name_length > maximum_name_length
            || name_length > reader.remaining().saturating_sub(CHECKSUM_BYTES)
        {
            return Err(invalid_cache("atlas cache resource name is invalid"));
        }
        let name = reader.string(name_length)?;
        if !expected_names.contains(&name) {
            return Err(invalid_cache("atlas cache resource name is unknown"));
        }
        raw_entries.push((
            name,
            reader.u32()?,
            reader.u32()?,
            reader.u32()?,
            reader.u32()?,
            reader.u32()?,
            f32::from_bits(reader.u32()?),
            f32::from_bits(reader.u32()?),
        ));
    }

    let page_count = reader.u32()? as usize;
    if page_count == 0 || page_count > expected_names.len().max(1) {
        return Err(invalid_cache("atlas cache page count is invalid"));
    }
    let mut pages = Vec::with_capacity(page_count);
    let mut common_extent = None;
    let mut total_bytes = 0usize;
    for _ in 0..page_count {
        let width = reader.u32()?;
        let height = reader.u32()?;
        if width == 0
            || height == 0
            || width > options.max_texture_dimension
            || height > options.max_texture_dimension
        {
            return Err(invalid_cache("atlas cache page extent is invalid"));
        }
        if common_extent
            .replace((width, height))
            .is_some_and(|extent| extent != (width, height))
        {
            return Err(invalid_cache("atlas cache pages do not share one extent"));
        }
        let byte_count = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| invalid_cache("atlas cache page size overflows"))?;
        total_bytes = total_bytes
            .checked_add(byte_count)
            .ok_or_else(|| invalid_cache("atlas cache size overflows"))?;
        if total_bytes > MAX_CACHE_BYTES
            || byte_count > reader.remaining().saturating_sub(CHECKSUM_BYTES)
        {
            return Err(invalid_cache("atlas cache page data is invalid"));
        }
        pages.push(TextureAtlasPage {
            width,
            height,
            rgba: reader.bytes(byte_count)?,
        });
    }
    reader.verify_checksum()?;

    let (page_width, page_height) = common_extent.expect("page count checked");
    let mut entries = BTreeMap::new();
    for (name, page, x, y, width, height, logical_width, logical_height) in raw_entries {
        if page as usize >= pages.len()
            || width == 0
            || height == 0
            || !logical_width.is_finite()
            || !logical_height.is_finite()
            || logical_width < 0.0
            || logical_height < 0.0
            || x.checked_add(width).is_none_or(|right| right > page_width)
            || y.checked_add(height)
                .is_none_or(|bottom| bottom > page_height)
        {
            return Err(invalid_cache("atlas cache entry bounds are invalid"));
        }
        if entries
            .insert(
                name,
                AtlasEntry {
                    page,
                    x,
                    y,
                    width,
                    height,
                    logical_width,
                    logical_height,
                    u_min: x as f32 / page_width as f32,
                    v_min: y as f32 / page_height as f32,
                    u_max: (x + width) as f32 / page_width as f32,
                    v_max: (y + height) as f32 / page_height as f32,
                },
            )
            .is_some()
        {
            return Err(invalid_cache("atlas cache repeats a resource"));
        }
    }
    Ok(TextureAtlas {
        entries,
        pages,
        padding: options.padding,
    })
}

fn raster_cache_identity(
    contract: &RendererContract,
    options: AtlasOptions,
    raster_assets: &[AtlasRasterAsset],
) -> Result<(String, std::collections::BTreeSet<String>)> {
    let mut expected_names = expected_atlas_asset_names(contract)?;
    if raster_assets.is_empty() {
        return Ok((atlas_cache_filename(contract, options), expected_names));
    }
    let mut hasher = Sha256::new();
    let mut ordered_assets = raster_assets.iter().collect::<Vec<_>>();
    ordered_assets.sort_by(|left, right| left.name.cmp(&right.name));
    for asset in ordered_assets {
        let expected_bytes = (asset.width as usize)
            .checked_mul(asset.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(Error::ArithmeticOverflow)?;
        if asset.name.is_empty()
            || !expected_names.insert(asset.name.clone())
            || asset.width == 0
            || asset.height == 0
            || asset.width > options.max_asset_dimension
            || asset.height > options.max_asset_dimension
            || !asset.logical_width.is_finite()
            || !asset.logical_height.is_finite()
            || asset.logical_width < 0.0
            || asset.logical_height < 0.0
            || asset.rgba.len() != expected_bytes
        {
            return Err(Error::Invalid(format!(
                "procedural atlas asset {} is invalid or duplicated",
                asset.name
            )));
        }
        hasher.update((asset.name.len() as u64).to_le_bytes());
        hasher.update(asset.name.as_bytes());
        hasher.update(asset.width.to_le_bytes());
        hasher.update(asset.height.to_le_bytes());
        hasher.update(asset.logical_width.to_bits().to_le_bytes());
        hasher.update(asset.logical_height.to_bits().to_le_bytes());
        hasher.update(&asset.rgba);
    }
    let base = atlas_cache_filename(contract, options);
    let stem = base
        .strip_suffix(".atlas-v6")
        .expect("atlas cache filename has a fixed suffix");
    Ok((
        format!("{stem}-p{}.atlas-v6", hex(&hasher.finalize()[..8])),
        expected_names,
    ))
}

fn write_cache_atomic(
    path: &Path,
    contract: &RendererContract,
    options: AtlasOptions,
    atlas: &TextureAtlas,
) -> Result<()> {
    let (temporary_path, mut file) = create_temporary(path)?;
    let result = (|| -> std::io::Result<()> {
        let checksum = {
            let mut writer = HashingWriter::new(&mut file);
            writer.write_all(MAGIC)?;
            writer.write_all(contract.fingerprint.as_bytes())?;
            writer.write_all(&options.svg_scale.to_bits().to_le_bytes())?;
            writer.write_all(&options.max_asset_dimension.to_le_bytes())?;
            writer.write_all(&options.max_texture_dimension.to_le_bytes())?;
            writer.write_all(&options.padding.to_le_bytes())?;
            write_u32(&mut writer, atlas.entries.len())?;
            for (name, entry) in &atlas.entries {
                write_u32(&mut writer, name.len())?;
                writer.write_all(name.as_bytes())?;
                for value in [entry.page, entry.x, entry.y, entry.width, entry.height] {
                    writer.write_all(&value.to_le_bytes())?;
                }
                writer.write_all(&entry.logical_width.to_bits().to_le_bytes())?;
                writer.write_all(&entry.logical_height.to_bits().to_le_bytes())?;
            }
            write_u32(&mut writer, atlas.pages.len())?;
            for page in &atlas.pages {
                writer.write_all(&page.width.to_le_bytes())?;
                writer.write_all(&page.height.to_le_bytes())?;
                writer.write_all(&page.rgba)?;
            }
            writer.finish()
        };
        file.write_all(&checksum)?;
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
        Error::Invalid("atlas cache path must have a parent directory".to_owned())
    })?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| Error::Invalid("atlas cache path is invalid".to_owned()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid("system clock precedes the Unix epoch".to_owned()))?
        .as_nanos();
    for attempt in 0..32u32 {
        let temporary = parent.join(format!(
            ".{filename}.{}.{}.tmp",
            std::process::id(),
            nonce + u128::from(attempt)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Invalid(
        "could not allocate an atlas cache temporary file".to_owned(),
    ))
}

fn write_u32(writer: &mut impl Write, value: usize) -> std::io::Result<()> {
    let value = u32::try_from(value).map_err(|_| invalid_cache("atlas cache count exceeds u32"))?;
    writer.write_all(&value.to_le_bytes())
}

fn invalid_cache(message: &str) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, message)
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        // SAFETY: `file` owns a live descriptor for the duration of this call.
        // The kernel releases the advisory lock if this process exits.
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
        "atlas disk-cache coordination requires an OS file-lock implementation",
    ))
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

struct LimitedReader<R> {
    inner: R,
    remaining: usize,
    hasher: Sha256,
}

impl<R: Read> LimitedReader<R> {
    fn new(inner: R, remaining: usize) -> Self {
        Self {
            inner,
            remaining,
            hasher: Sha256::new(),
        }
    }

    const fn remaining(&self) -> usize {
        self.remaining
    }

    fn bytes(&mut self, length: usize) -> std::io::Result<Vec<u8>> {
        if length > self.remaining {
            return Err(invalid_cache("atlas cache is truncated"));
        }
        let mut output = vec![0; length];
        self.inner.read_exact(&mut output)?;
        self.remaining -= length;
        self.hasher.update(&output);
        Ok(output)
    }

    fn string(&mut self, length: usize) -> std::io::Result<String> {
        String::from_utf8(self.bytes(length)?)
            .map_err(|_| invalid_cache("atlas cache text is not UTF-8"))
    }

    fn u32(&mut self) -> std::io::Result<u32> {
        let bytes: [u8; 4] = self
            .bytes(4)?
            .try_into()
            .expect("requested exactly four bytes");
        Ok(u32::from_le_bytes(bytes))
    }

    fn verify_checksum(&mut self) -> std::io::Result<()> {
        if self.remaining != CHECKSUM_BYTES {
            return Err(invalid_cache("atlas cache has trailing bytes"));
        }
        let mut checksum = [0; CHECKSUM_BYTES];
        self.inner.read_exact(&mut checksum)?;
        self.remaining = 0;
        let actual = std::mem::take(&mut self.hasher).finalize();
        if actual.as_slice() != checksum {
            return Err(invalid_cache("atlas cache checksum is invalid"));
        }
        Ok(())
    }
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> [u8; CHECKSUM_BYTES] {
        self.hasher.finalize().into()
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hasher.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};

    use crate::artifact::{Nullable, RendererContract, RendererInventory};
    use crate::{AtlasOptions, AtlasRasterAsset, TextureAtlas, atlas_cache_filename};

    fn contract() -> RendererContract {
        let svg = STANDARD.encode(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="3"><rect width="2" height="3" fill="#fff"/></svg>"##,
        );
        RendererContract {
            schema: "screeps-arena-renderer-contract".to_owned(),
            version: 5,
            renderer_version: Nullable(Some("test".to_owned())),
            metadata: Value::Object(Default::default()),
            resources: json!({
                "unit": format!("data:image/svg+xml;base64,{svg}")
            }),
            decorations: Vec::new(),
            terrain: Vec::new(),
            world_options: Value::Object(Default::default()),
            inventory: RendererInventory {
                object_types: Vec::new(),
                processor_types: Vec::new(),
                action_types: Vec::new(),
                preprocessors: Vec::new(),
                calculation_ids: Vec::new(),
                drawing_methods: Vec::new(),
                expression_operators: Vec::new(),
                function_semantics: Vec::new(),
                layer_ids: Vec::new(),
                renderer_implementation_fingerprints: Vec::new(),
            },
            fingerprint: "ab".repeat(32),
        }
    }

    fn temporary_directory() -> PathBuf {
        std::env::temp_dir().join(format!(
            "screeps-arena-atlas-cache-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("worker")
        ))
    }

    #[test]
    fn cache_round_trips_and_recovers_from_corruption() {
        let contract = contract();
        let options = AtlasOptions::default();
        let directory = temporary_directory();
        let _ = std::fs::remove_dir_all(&directory);

        let (built, was_cached) =
            TextureAtlas::load_or_build_cached(&contract, options, &directory).unwrap();
        assert!(!was_cached);
        let (loaded, was_cached) =
            TextureAtlas::load_or_build_cached(&contract, options, &directory).unwrap();
        assert!(was_cached);
        assert_eq!(loaded.entries, built.entries);
        assert_eq!(loaded.pages[0].rgba, built.pages[0].rgba);

        let path = directory.join(atlas_cache_filename(&contract, options));
        let mut bytes = std::fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x80;
        std::fs::write(path, bytes).unwrap();
        let (recovered, was_cached) =
            TextureAtlas::load_or_build_cached(&contract, options, &directory).unwrap();
        assert!(!was_cached);
        assert_eq!(recovered.entries, built.entries);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn procedural_raster_bytes_participate_in_cache_identity() {
        let contract = contract();
        let options = AtlasOptions::default();
        let directory = temporary_directory().join("procedural");
        let _ = std::fs::remove_dir_all(&directory);
        let asset = |red| AtlasRasterAsset {
            name: "$graphics.circle".to_owned(),
            width: 1,
            height: 1,
            logical_width: 1.0,
            logical_height: 1.0,
            rgba: vec![red, 0, 0, 255],
        };
        let (_, was_cached) = TextureAtlas::load_or_build_cached_with_raster_assets(
            &contract,
            options,
            vec![asset(10)],
            &directory,
        )
        .unwrap();
        assert!(!was_cached);
        let (loaded, was_cached) = TextureAtlas::load_or_build_cached_with_raster_assets(
            &contract,
            options,
            vec![asset(10)],
            &directory,
        )
        .unwrap();
        assert!(was_cached);
        let entry = loaded.entries["$graphics.circle"];
        let page = &loaded.pages[entry.page as usize];
        let offset = ((entry.y * page.width + entry.x) * 4) as usize;
        assert_eq!(&page.rgba[offset..offset + 4], &[10, 0, 0, 255]);

        let (_, was_cached) = TextureAtlas::load_or_build_cached_with_raster_assets(
            &contract,
            options,
            vec![asset(20)],
            &directory,
        )
        .unwrap();
        assert!(!was_cached);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cache_round_trips_materialized_decoration_entries() {
        let mut contract = contract();
        let png = {
            use image::ImageEncoder;

            let mut bytes = Vec::new();
            image::codecs::png::PngEncoder::new(&mut bytes)
                .write_image(&[1, 2, 3, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .unwrap();
            STANDARD.encode(bytes)
        };
        contract.decorations = vec![json!({
            "decoration": {
                "type": "floorLandscape",
                "floorForegroundUrl": format!("data:image/png;base64,{png}")
            }
        })];
        let directory = temporary_directory().join("decorations");
        let _ = std::fs::remove_dir_all(&directory);
        let options = AtlasOptions::default();
        let (built, built_hit) =
            TextureAtlas::load_or_build_cached(&contract, options, &directory).unwrap();
        let (loaded, loaded_hit) =
            TextureAtlas::load_or_build_cached(&contract, options, &directory).unwrap();

        assert!(!built_hit);
        assert!(loaded_hit);
        assert_eq!(built.entries, loaded.entries);
        assert!(loaded.entries.contains_key(&crate::decoration_asset_name(
            0,
            &["decoration", "floorForegroundUrl"]
        )));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_cold_workers_build_an_atlas_once() {
        let contract = std::sync::Arc::new(contract());
        let directory = temporary_directory().join("concurrent");
        let _ = std::fs::remove_dir_all(&directory);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers = (0..4)
            .map(|_| {
                let contract = std::sync::Arc::clone(&contract);
                let directory = directory.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    TextureAtlas::load_or_build_cached(
                        &contract,
                        AtlasOptions::default(),
                        directory,
                    )
                    .map(|(_, hit)| hit)
                })
            })
            .collect::<Vec<_>>();
        let hits = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(hits.iter().filter(|hit| !**hit).count(), 1);
        assert_eq!(hits.iter().filter(|hit| **hit).count(), 3);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
