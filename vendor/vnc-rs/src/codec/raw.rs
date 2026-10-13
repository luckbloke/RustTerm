use crate::{PixelFormat, Rect, VncError, VncEvent, VncLimits};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{checked_buffer_size, uninit_vec};

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
        let bpp = usize::from(format.bits_per_pixel / 8);
        let buffer_size = checked_buffer_size(rect, bpp, &self.limits)?;
        let mut pixels = uninit_vec(buffer_size);
        input.read_exact(&mut pixels).await?;
        output_func(VncEvent::RawImage(*rect, pixels)).await?;
        Ok(())
    }
}
