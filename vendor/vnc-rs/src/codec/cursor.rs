use crate::{PixelFormat, Rect, VncError, VncEvent, VncLimits};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{checked_buffer_size, ensure_payload_limit, uninit_vec};

pub struct Decoder {
    limits: VncLimits,
}

impl Decoder {
    pub fn new(limits: VncLimits) -> Self {
        Self { limits }
    }

    pub async fn decode<S, F, Fut>(
        &mut self,
        format: &PixelFormat,
        rect: &Rect,
        input: &mut S,
        output_func: &F,
    ) -> Result<(), VncError>
    where
        S: AsyncRead + Unpin,
        F: Fn(VncEvent) -> Fut,
        Fut: Future<Output = Result<(), VncError>>,
    {
        if format.bits_per_pixel != 32 {
            return Err(VncError::WrongPixelFormat);
        }
        let pixels_length = checked_buffer_size(rect, 4, &self.limits)?;
        let row_mask_bytes = usize::from(rect.width).div_ceil(8);
        let mask_length = row_mask_bytes
            .checked_mul(usize::from(rect.height))
            .ok_or(VncError::IntegerOverflow("cursor mask bytes"))?;
        ensure_payload_limit(
            "cursor mask",
            mask_length,
            self.limits.max_encoded_payload_bytes,
        )?;

        let mut pixels = uninit_vec(pixels_length);
        input.read_exact(&mut pixels).await?;
        let mut mask = uninit_vec(mask_length);
        input.read_exact(&mut mask).await?;
        let mut image = pixels;

        let pixel_mask = ((format.red_max as u32) << format.red_shift)
            | ((format.green_max as u32) << format.green_shift)
            | ((format.blue_max as u32) << format.blue_shift);
        let mut alpha_idx = match pixel_mask {
            0xff_ff_ff_00 => 3,
            0xff_ff_00_ff => 2,
            0xff_00_ff_ff => 1,
            0x00_ff_ff_ff => 0,
            _ => return Err(VncError::WrongPixelFormat),
        };
        if format.big_endian_flag == 0 {
            alpha_idx = 3 - alpha_idx;
        }
        for y in 0..usize::from(rect.height) {
            for x in 0..usize::from(rect.width) {
                let mask_idx = y * row_mask_bytes + (x / 8);
                let alpha = if (mask[mask_idx] << (x % 8)) & 0x80 > 0 {
                    255
                } else {
                    0
                };
                let pix_idx = (y * usize::from(rect.width) + x) * 4;
                image[pix_idx + alpha_idx] = alpha;
            }
        }

        output_func(VncEvent::SetCursor(*rect, image)).await?;
        Ok(())
    }
}
