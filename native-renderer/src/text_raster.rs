use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use fontdue::{Font, FontSettings};
use sha2::{Digest, Sha256};

use crate::{AtlasRasterAsset, Error, ResolvedValue, Result};

const ROBOTO_REGULAR_CANDIDATES: [&str; 4] = [
    "/usr/share/fonts/TTF/Roboto-Regular.ttf",
    "/usr/share/fonts/truetype/roboto/unhinted/RobotoTTF/Roboto-Regular.ttf",
    "/usr/share/fonts/truetype/roboto/Roboto-Regular.ttf",
    "/usr/local/share/fonts/Roboto-Regular.ttf",
];

#[derive(Clone, Debug, PartialEq)]
struct TextSpec {
    text: char,
    font_size: f32,
    stage_zoom: f32,
    fill: u32,
}

pub(crate) fn lower_supported_text_payload(
    payload: &ResolvedValue,
    stage_zoom: f64,
) -> Result<Option<ResolvedValue>> {
    let Some(spec) = TextSpec::parse(payload, Some(stage_zoom))? else {
        return Ok(None);
    };
    let font_bytes = read_roboto_regular()?;
    let font_sha256 = format!("{:x}", Sha256::digest(&font_bytes));
    let mut payload = payload
        .as_object()
        .expect("parsed text payload is an object")
        .clone();
    payload.insert(
        "texture".to_owned(),
        ResolvedValue::String(spec.asset_name(&font_sha256)),
    );
    payload.insert("$nativeTextRaster".to_owned(), ResolvedValue::Bool(true));
    payload.insert(
        "$nativeTextFontSha256".to_owned(),
        ResolvedValue::String(font_sha256),
    );
    payload.insert(
        "$textStageZoom".to_owned(),
        ResolvedValue::Number(stage_zoom),
    );
    Ok(Some(ResolvedValue::Object(payload)))
}

pub(crate) fn text_raster_assets(
    activations: impl Iterator<Item = ResolvedValue>,
) -> Result<Vec<AtlasRasterAsset>> {
    let activations = activations.collect::<Vec<_>>();
    if activations.is_empty() {
        return Ok(Vec::new());
    }
    let font_bytes = read_roboto_regular()?;
    let font_sha256 = format!("{:x}", Sha256::digest(&font_bytes));
    let mut specs = BTreeMap::<String, TextSpec>::new();
    for payload in activations {
        let Some(spec) = TextSpec::parse(&payload, None)? else {
            continue;
        };
        if payload
            .get("$nativeTextFontSha256")
            .and_then(ResolvedValue::as_string)
            != Some(font_sha256.as_str())
        {
            return Err(Error::Invalid(
                "Roboto Regular font changed after text lowering".to_owned(),
            ));
        }
        specs.entry(spec.asset_name(&font_sha256)).or_insert(spec);
    }
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let font = Font::from_bytes(font_bytes, FontSettings::default())
        .map_err(|error| Error::Invalid(format!("failed to parse Roboto Regular font: {error}")))?;
    specs
        .into_iter()
        .map(|(name, spec)| spec.rasterize(name, &font))
        .collect()
}

fn read_roboto_regular() -> Result<Vec<u8>> {
    let font_path = roboto_regular_path().ok_or_else(|| {
        Error::Invalid(
            "native text adapter requires an installed Roboto Regular TrueType font".to_owned(),
        )
    })?;
    fs::read(&font_path).map_err(|error| {
        Error::Invalid(format!(
            "failed to read Roboto Regular font {}: {error}",
            font_path.display()
        ))
    })
}

fn roboto_regular_path() -> Option<PathBuf> {
    std::env::var_os("SCREEPS_VIDEOIZER_ROBOTO_REGULAR")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            ROBOTO_REGULAR_CANDIDATES
                .iter()
                .map(Path::new)
                .find(|path| path.is_file())
                .map(Path::to_path_buf)
        })
}

impl TextSpec {
    fn parse(payload: &ResolvedValue, stage_zoom: Option<f64>) -> Result<Option<Self>> {
        let payload = payload
            .as_object()
            .ok_or_else(|| Error::Invalid("text payload must resolve to an object".to_owned()))?;
        let Some(text) = payload.get("text").and_then(ResolvedValue::as_string) else {
            return Ok(None);
        };
        let mut characters = text.chars();
        let Some(text) = characters.next() else {
            return Ok(None);
        };
        if characters.next().is_some() || !text.is_ascii_uppercase() {
            return Ok(None);
        }
        let Some(style) = payload.get("style").and_then(ResolvedValue::as_object) else {
            return Ok(None);
        };
        if style
            .keys()
            .any(|key| !matches!(key.as_str(), "align" | "fill" | "fontFamily" | "fontSize"))
        {
            return Ok(None);
        }
        if style
            .get("fontFamily")
            .and_then(ResolvedValue::as_string)
            .unwrap_or("Arial")
            != "Roboto, sans-serif"
        {
            return Ok(None);
        }
        let Some(base_font_size) = style.get("fontSize").and_then(ResolvedValue::as_number) else {
            return Ok(None);
        };
        if !base_font_size.is_finite() || !(1.0..=512.0).contains(&base_font_size) {
            return Ok(None);
        }
        let stage_zoom = stage_zoom.or_else(|| {
            payload
                .get("$textStageZoom")
                .and_then(ResolvedValue::as_number)
        });
        let Some(stage_zoom) = stage_zoom else {
            return Ok(None);
        };
        if !stage_zoom.is_finite() || !(0.01..=16.0).contains(&stage_zoom) {
            return Ok(None);
        }
        let Some(fill) = style.get("fill") else {
            return Ok(None);
        };
        let fill = match fill {
            ResolvedValue::Number(value)
                if value.is_finite() && (0.0..=f64::from(0xff_ff_ff)).contains(value) =>
            {
                value.floor() as u32
            }
            ResolvedValue::String(value) => parse_hex_color(value)?,
            _ => return Ok(None),
        };
        Ok(Some(Self {
            text,
            font_size: (base_font_size * stage_zoom) as f32,
            stage_zoom: stage_zoom as f32,
            fill,
        }))
    }

    fn asset_name(&self, font_sha256: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"screeps-arena-text-v1");
        hasher.update((self.text as u32).to_le_bytes());
        hasher.update(self.font_size.to_bits().to_le_bytes());
        hasher.update(self.stage_zoom.to_bits().to_le_bytes());
        hasher.update(self.fill.to_le_bytes());
        hasher.update(font_sha256.as_bytes());
        format!("$text:{:x}", hasher.finalize())
    }

    fn rasterize(&self, name: String, font: &Font) -> Result<AtlasRasterAsset> {
        let (metrics, coverage) = font.rasterize(self.text, self.font_size);
        let line = font
            .horizontal_line_metrics(self.font_size)
            .ok_or_else(|| {
                Error::Invalid("Roboto Regular lacks horizontal line metrics".to_owned())
            })?;
        let width = metrics.advance_width.ceil().max(1.0) as u32;
        let height = (line.ascent - line.descent).ceil().max(1.0) as u32;
        let pixel_count = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or(Error::ArithmeticOverflow)?;
        let mut rgba = vec![
            0_u8;
            pixel_count
                .checked_mul(4)
                .ok_or(Error::ArithmeticOverflow)?
        ];
        let baseline = line.ascent.ceil() as i32;
        let origin_x = metrics.xmin;
        let origin_y = baseline - metrics.ymin - metrics.height as i32;
        let red = ((self.fill >> 16) & 0xff) as u8;
        let green = ((self.fill >> 8) & 0xff) as u8;
        let blue = (self.fill & 0xff) as u8;
        for source_y in 0..metrics.height {
            for source_x in 0..metrics.width {
                let target_x = origin_x + source_x as i32;
                let target_y = origin_y + source_y as i32;
                if target_x < 0
                    || target_y < 0
                    || target_x >= width as i32
                    || target_y >= height as i32
                {
                    continue;
                }
                let alpha = coverage[source_y * metrics.width + source_x];
                let target = (target_y as usize * width as usize + target_x as usize) * 4;
                rgba[target] = premultiply(red, alpha);
                rgba[target + 1] = premultiply(green, alpha);
                rgba[target + 2] = premultiply(blue, alpha);
                rgba[target + 3] = alpha;
            }
        }
        Ok(AtlasRasterAsset {
            name,
            width,
            height,
            logical_width: width as f32 / self.stage_zoom,
            logical_height: height as f32 / self.stage_zoom,
            rgba,
        })
    }
}

fn parse_hex_color(value: &str) -> Result<u32> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return Err(Error::Invalid(
            "native text fill must be a six-digit hexadecimal color".to_owned(),
        ));
    }
    u32::from_str_radix(value, 16)
        .map_err(|_| Error::Invalid("native text fill is not hexadecimal".to_owned()))
}

fn premultiply(color: u8, alpha: u8) -> u8 {
    ((u16::from(color) * u16::from(alpha) + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::ResolvedValue;

    use super::{TextSpec, lower_supported_text_payload, premultiply, text_raster_assets};

    fn payload(text: &str, fill: ResolvedValue) -> ResolvedValue {
        ResolvedValue::Object(BTreeMap::from([
            (
                "anchor".to_owned(),
                ResolvedValue::Object(BTreeMap::from([
                    ("x".to_owned(), ResolvedValue::Number(0.5)),
                    ("y".to_owned(), ResolvedValue::Number(0.5)),
                ])),
            ),
            (
                "style".to_owned(),
                ResolvedValue::Object(BTreeMap::from([
                    (
                        "align".to_owned(),
                        ResolvedValue::String("center".to_owned()),
                    ),
                    ("fill".to_owned(), fill),
                    (
                        "fontFamily".to_owned(),
                        ResolvedValue::String("Roboto, sans-serif".to_owned()),
                    ),
                    ("fontSize".to_owned(), ResolvedValue::Number(60.0)),
                ])),
            ),
            ("text".to_owned(), ResolvedValue::String(text.to_owned())),
        ]))
    }

    #[test]
    fn supports_captured_body_label_shape_and_deduplicates_color_forms() {
        let numeric = payload("M", ResolvedValue::Number(0xaa_b7_c5 as f64));
        let string = payload("M", ResolvedValue::String("#aab7c5".to_owned()));
        let numeric = lower_supported_text_payload(&numeric, 0.1984)
            .unwrap()
            .unwrap();
        let string = lower_supported_text_payload(&string, 0.1984)
            .unwrap()
            .unwrap();
        assert_eq!(numeric.get("texture"), string.get("texture"));
        assert!(
            numeric
                .get("texture")
                .unwrap()
                .as_string()
                .unwrap()
                .starts_with("$text:")
        );
    }

    #[test]
    fn text_asset_identity_and_materialization_require_the_lowered_font_bytes() {
        let lowered = lower_supported_text_payload(
            &payload("M", ResolvedValue::Number(0xaa_b7_c5 as f64)),
            0.1984,
        )
        .unwrap()
        .unwrap();
        let texture = lowered.get("texture").unwrap().as_string().unwrap();
        let font_sha256 = lowered
            .get("$nativeTextFontSha256")
            .unwrap()
            .as_string()
            .unwrap();
        assert_eq!(font_sha256.len(), 64);
        let spec = TextSpec::parse(&lowered, None).unwrap().unwrap();
        assert_eq!(texture, spec.asset_name(font_sha256));
        assert_ne!(texture, spec.asset_name(&"0".repeat(64)));

        let mut forged = lowered;
        let ResolvedValue::Object(payload) = &mut forged else {
            unreachable!()
        };
        payload.insert(
            "$nativeTextFontSha256".to_owned(),
            ResolvedValue::String("0".repeat(64)),
        );
        assert_eq!(
            text_raster_assets([forged].into_iter())
                .unwrap_err()
                .to_string(),
            "Roboto Regular font changed after text lowering"
        );
    }

    #[test]
    fn rejects_unimplemented_text_shapes_without_claiming_support() {
        assert!(
            lower_supported_text_payload(
                &payload("name", ResolvedValue::Number(0xff_ff_ff as f64)),
                0.1984,
            )
            .unwrap()
            .is_none()
        );
        let mut stroked = payload("A", ResolvedValue::Number(0xff_ff_ff as f64));
        let ResolvedValue::Object(payload) = &mut stroked else {
            unreachable!()
        };
        let ResolvedValue::Object(style) = payload.get_mut("style").unwrap() else {
            unreachable!()
        };
        style.insert("stroke".to_owned(), ResolvedValue::Number(0.0));
        assert!(
            lower_supported_text_payload(&stroked, 0.1984)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn raster_channels_are_premultiplied() {
        assert_eq!(premultiply(0xaa, 0x80), 85);
        assert_eq!(
            TextSpec::parse(
                &payload("H", ResolvedValue::Number(0x56_cf_5e as f64)),
                Some(0.1984),
            )
            .unwrap()
            .unwrap()
            .fill,
            0x56_cf_5e
        );
    }
}
