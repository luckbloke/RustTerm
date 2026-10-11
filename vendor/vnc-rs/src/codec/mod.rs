mod cursor;
mod raw;
mod tight;
mod trle;
mod zlib;
mod zrle;
pub(crate) use cursor::Decoder as CursorDecoder;
pub(crate) use raw::Decoder as RawDecoder;
pub(crate) use tight::Decoder as TightDecoder;
pub(crate) use trle::Decoder as TrleDecoder;
pub(crate) use zrle::Decoder as ZrleDecoder;

use crate::{PixelFormat, Rect, VncError, VncLimits};

fn uninit_vec(len: usize) -> Vec<u8> {
    vec![0; len]
}

fn ensure_payload_limit(field: &'static str, len: usize, limit: usize) -> Result<(), VncError> {
    if len > limit {
        return Err(VncError::LimitExceeded {
            field,
            actual: len as u64,
            limit: limit as u64,
        });
    }
    Ok(())
}

fn checked_buffer_size(
    rect: &Rect,
    bytes_per_pixel: usize,
    limits: &VncLimits,
) -> Result<usize, VncError> {
    let pixels = usize::from(rect.width)
        .checked_mul(usize::from(rect.height))
        .ok_or(VncError::IntegerOverflow("rectangle pixels"))?;
    let bytes = pixels
        .checked_mul(bytes_per_pixel)
        .ok_or(VncError::IntegerOverflow("decoded rectangle bytes"))?;
    ensure_payload_limit("decoded rectangle", bytes, limits.max_decoded_payload_bytes)?;
    Ok(bytes)
}

fn checked_pixel_count(rect: &Rect) -> Result<usize, VncError> {
    usize::from(rect.width)
        .checked_mul(usize::from(rect.height))
        .ok_or(VncError::IntegerOverflow("rectangle pixels"))
}

fn checked_rgb_size(rect: &Rect, limits: &VncLimits) -> Result<usize, VncError> {
    let bytes = checked_pixel_count(rect)?
        .checked_mul(3)
        .ok_or(VncError::IntegerOverflow("decoded RGB rectangle bytes"))?;
    ensure_payload_limit(
        "decoded RGB rectangle",
        bytes,
        limits.max_decoded_payload_bytes,
    )?;
    Ok(bytes)
}

fn checked_rgba_size(rect: &Rect, limits: &VncLimits) -> Result<usize, VncError> {
    checked_buffer_size(rect, 4, limits)
}

fn alpha_shift(format: &PixelFormat) -> Result<u32, VncError> {
    let pixel_mask = ((format.red_max as u32) << format.red_shift)
        | ((format.green_max as u32) << format.green_shift)
        | ((format.blue_max as u32) << format.blue_shift);

    match pixel_mask {
        0xff_ff_ff_00 => Ok(0),
        0xff_ff_00_ff => Ok(8),
        0xff_00_ff_ff => Ok(16),
        0x00_ff_ff_ff => Ok(24),
        _ => Err(VncError::WrongPixelFormat),
    }
}

fn rgb_to_pixel(format: &PixelFormat, alpha_shift: u32, rgb: &[u8]) -> Result<[u8; 4], VncError> {
    let [red, green, blue] = rgb else {
        return Err(VncError::InvalidImageData);
    };
    let alpha = 255_u32;
    Ok(
        ((u32::from(*red) & u32::from(format.red_max)) << format.red_shift
            | (u32::from(*green) & u32::from(format.green_max)) << format.green_shift
            | (u32::from(*blue) & u32::from(format.blue_max)) << format.blue_shift
            | (alpha << alpha_shift))
            .to_le_bytes(),
    )
}
