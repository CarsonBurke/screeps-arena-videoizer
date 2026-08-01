use crate::{Error, Result};

pub(crate) fn downsample_rgba8(source: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    resample_box(source, width, height, 4)
}

pub(crate) fn downsample_r8(source: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    resample_box(source, width, height, 1)
}

fn resample_box(source: &[u8], width: u32, height: u32, channels: u32) -> Result<Vec<u8>> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channels as usize))
        .ok_or(Error::ArithmeticOverflow)?;
    if width == 0 || height == 0 || !matches!(channels, 1 | 4) || source.len() != expected {
        return Err(Error::Invalid(
            "mipmap source pixels are invalid".to_owned(),
        ));
    }
    let output_width = (width / 2).max(1);
    let output_height = (height / 2).max(1);
    let output_bytes = (output_width as usize)
        .checked_mul(output_height as usize)
        .and_then(|pixels| pixels.checked_mul(channels as usize))
        .ok_or(Error::ArithmeticOverflow)?;
    let mut output = vec![0; output_bytes];
    let denominator = u64::from(width) * u64::from(height);
    for output_y in 0..output_height {
        let source_y_start = output_y * height;
        let source_y_end = (output_y + 1) * height;
        let first_source_y = source_y_start / output_height;
        let last_source_y = source_y_end.div_ceil(output_height);
        for output_x in 0..output_width {
            let source_x_start = output_x * width;
            let source_x_end = (output_x + 1) * width;
            let first_source_x = source_x_start / output_width;
            let last_source_x = source_x_end.div_ceil(output_width);
            let mut sums = [0u64; 4];
            for source_y in first_source_y..last_source_y {
                let source_pixel_y_start = source_y * output_height;
                let source_pixel_y_end = (source_y + 1) * output_height;
                let weight_y =
                    source_y_end.min(source_pixel_y_end) - source_y_start.max(source_pixel_y_start);
                for source_x in first_source_x..last_source_x {
                    let source_pixel_x_start = source_x * output_width;
                    let source_pixel_x_end = (source_x + 1) * output_width;
                    let weight_x = source_x_end.min(source_pixel_x_end)
                        - source_x_start.max(source_pixel_x_start);
                    let weight = u64::from(weight_x) * u64::from(weight_y);
                    let source_offset = ((source_y * width + source_x) * channels) as usize;
                    for channel in 0..channels as usize {
                        sums[channel] += u64::from(source[source_offset + channel]) * weight;
                    }
                }
            }
            let output_offset = ((output_y * output_width + output_x) * channels) as usize;
            for channel in 0..channels as usize {
                output[output_offset + channel] =
                    ((sums[channel] + denominator / 2) / denominator) as u8;
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{downsample_r8, downsample_rgba8};

    #[test]
    fn odd_extents_contribute_every_source_texel() {
        assert_eq!(downsample_r8(&[0, 60, 120], 3, 1).unwrap(), vec![60]);
        assert_eq!(
            downsample_r8(&[0, 60, 120, 180, 240], 5, 1).unwrap(),
            vec![48, 192]
        );
        let rgba = [0, 10, 20, 255, 60, 70, 80, 255, 120, 130, 140, 255];
        assert_eq!(
            downsample_rgba8(&rgba, 3, 1).unwrap(),
            vec![60, 70, 80, 255]
        );
    }
}
