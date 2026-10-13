use crate::{PixelFormat, Rect, VncError, VncEvent, VncLimits};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::error;

use super::{
    checked_buffer_size, checked_pixel_count, ensure_payload_limit, uninit_vec, zlib::ZlibReader,
};

fn read_run_length(reader: &mut ZlibReader) -> Result<usize, VncError> {
    let mut run_length_part;
    let mut run_length = 1_usize;
    loop {
        run_length_part = reader.read_u8()?;
        run_length = run_length
            .checked_add(usize::from(run_length_part))
            .ok_or(VncError::IntegerOverflow("ZRLE run length"))?;
        if 255 != run_length_part {
            break;
        }
    }
    Ok(run_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    use std::{
        future::ready,
        io::Write,
        sync::{Arc, Mutex},
    };

    fn rect(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).expect("write zlib input");
        encoder.finish().expect("finish zlib")
    }

    fn zrle_payload(decoded: &[u8]) -> Vec<u8> {
        let compressed = compress(decoded);
        let mut payload = Vec::new();
        payload.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
        payload.extend_from_slice(&compressed);
        payload
    }

    async fn decode(payload: Vec<u8>, rect: Rect) -> Result<Vec<VncEvent>, VncError> {
        let mut decoder = Decoder::new(VncLimits::default());
        let mut input = std::io::Cursor::new(payload);
        let events = Arc::new(Mutex::new(Vec::new()));
        decoder
            .decode(&PixelFormat::rgba(), &rect, &mut input, &|event| {
                events.lock().expect("events").push(event);
                ready(Ok(()))
            })
            .await?;
        let events = events.lock().expect("events").clone();
        Ok(events)
    }

    fn raw_payload(events: &[VncEvent]) -> &[u8] {
        let [VncEvent::RawImage(_, payload)] = events else {
            panic!("expected exactly one raw image event");
        };
        payload
    }

    #[tokio::test]
    async fn decodes_true_color_tile() {
        let decoded = [0, 10, 20, 30, 40, 50, 60];
        let events = decode(zrle_payload(&decoded), rect(2, 1)).await.unwrap();
        assert_eq!(raw_payload(&events), &[10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[tokio::test]
    async fn decodes_palette_fill_and_packed_palette() {
        let decoded = [
            1,
            1,
            2,
            3, // palette fill
            2,
            10,
            20,
            30,
            40,
            50,
            60,
            0b1000_0000, // packed palette
        ];
        let events = decode(zrle_payload(&decoded), rect(66, 1)).await.unwrap();
        let payload = raw_payload(&events);
        assert_eq!(payload.len(), 66 * 4);
        assert_eq!(&payload[0..4], &[1, 2, 3, 255]);
        assert_eq!(&payload[63 * 4..64 * 4], &[1, 2, 3, 255]);
        assert_eq!(&payload[64 * 4..65 * 4], &[40, 50, 60, 255]);
        assert_eq!(&payload[65 * 4..66 * 4], &[10, 20, 30, 255]);
    }

    #[tokio::test]
    async fn decodes_true_color_and_indexed_rle() {
        let decoded = [
            128, 7, 8, 9, 63, // true-color RLE, full 64-pixel tile
            130, 10, 20, 30, 40, 50, 60, 0x81, 1, // indexed RLE, two pixels of index 1
        ];
        let events = decode(zrle_payload(&decoded), rect(66, 1)).await.unwrap();
        let payload = raw_payload(&events);
        assert_eq!(payload.len(), 66 * 4);
        assert!(payload[..64 * 4]
            .chunks_exact(4)
            .all(|pixel| pixel == [7, 8, 9, 255]));
        assert_eq!(&payload[64 * 4..66 * 4], &[40, 50, 60, 255, 40, 50, 60, 255]);
    }

    #[tokio::test]
    async fn rejects_malformed_zrle_payloads() {
        let too_long_rle = [128, 1, 2, 3, 2];
        assert!(matches!(
            decode(zrle_payload(&too_long_rle), rect(2, 1)).await,
            Err(VncError::InvalidImageData)
        ));

        let bad_index = [130, 1, 2, 3, 4, 5, 6, 0x02];
        assert!(matches!(
            decode(zrle_payload(&bad_index), rect(2, 1)).await,
            Err(VncError::InvalidImageData)
        ));

        let mut compressed_with_leftover = compress(&[0, 1, 2, 3]);
        compressed_with_leftover.push(0);
        let mut leftover = Vec::new();
        leftover.extend_from_slice(&(compressed_with_leftover.len() as u32).to_be_bytes());
        leftover.extend_from_slice(&compressed_with_leftover);
        assert!(matches!(
            decode(leftover, rect(1, 1)).await,
            Err(VncError::IoError(_))
        ));

        let mut truncated = zrle_payload(&[0, 1, 2]);
        truncated.pop();
        assert!(matches!(
            decode(truncated, rect(1, 1)).await,
            Err(VncError::IoError(_))
        ));
    }

    #[tokio::test]
    async fn rejects_zrle_encoded_and_decoded_bombs() {
        let mut limits = VncLimits::default();
        limits.max_encoded_payload_bytes = 1;
        let mut decoder = Decoder::new(limits);
        let payload = zrle_payload(&[0, 1, 2, 3]);
        let mut input = std::io::Cursor::new(payload);
        assert!(matches!(
            decoder
                .decode(&PixelFormat::rgba(), &rect(1, 1), &mut input, &|_| ready(
                    Ok(())
                ))
                .await,
            Err(VncError::LimitExceeded {
                field: "ZRLE encoded payload",
                ..
            })
        ));

        let mut limits = VncLimits::default();
        limits.max_decoded_payload_bytes = 3;
        let mut decoder = Decoder::new(limits);
        let payload = zrle_payload(&[0, 1, 2, 3]);
        let mut input = std::io::Cursor::new(payload);
        assert!(matches!(
            decoder
                .decode(&PixelFormat::rgba(), &rect(1, 1), &mut input, &|_| ready(
                    Ok(())
                ))
                .await,
            Err(VncError::LimitExceeded { .. })
        ));
    }
}

fn copy_true_color(
    reader: &mut ZlibReader,
    pixels: &mut Vec<u8>,
    pad: bool,
    compressed_bpp: usize,
    bpp: usize,
) -> Result<(), VncError> {
    let mut buf = [255; 4];
    std::io::Read::read_exact(
        reader,
        &mut buf[pad as usize..pad as usize + compressed_bpp],
    )?;
    pixels.extend_from_slice(&buf[..bpp]);
    Ok(())
}

fn copy_indexed(
    palette: &[u8],
    pixels: &mut Vec<u8>,
    bpp: usize,
    index: u8,
) -> Result<(), VncError> {
    let start = usize::from(index)
        .checked_mul(bpp)
        .ok_or(VncError::IntegerOverflow("ZRLE palette offset"))?;
    let end = start
        .checked_add(bpp)
        .ok_or(VncError::IntegerOverflow("ZRLE palette end"))?;
    let color = palette.get(start..end).ok_or(VncError::InvalidImageData)?;
    pixels.extend_from_slice(color);
    Ok(())
}

pub struct Decoder {
    decompressor: Option<flate2::Decompress>,
    limits: VncLimits,
}

impl Decoder {
    pub fn new(limits: VncLimits) -> Self {
        Self {
            decompressor: Some(flate2::Decompress::new(true)),
            limits,
        }
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
        let data_len = input.read_u32().await? as usize;
        ensure_payload_limit(
            "ZRLE encoded payload",
            data_len,
            self.limits.max_encoded_payload_bytes,
        )?;
        let mut zlib_data = uninit_vec(data_len);
        input.read_exact(&mut zlib_data).await?;
        let decompressor = self.decompressor.take().ok_or(VncError::InvalidImageData)?;
        let mut reader = ZlibReader::new(decompressor, &zlib_data);

        let bpp = format.bits_per_pixel as usize / 8;
        let rect_bytes = checked_buffer_size(rect, bpp, &self.limits)?;
        let pixel_mask = ((format.red_max as u32) << format.red_shift)
            | ((format.green_max as u32) << format.green_shift)
            | ((format.blue_max as u32) << format.blue_shift);

        let (compressed_bpp, alpha_at_first) =
            if format.bits_per_pixel == 32 && format.true_color_flag > 0 && format.depth <= 24 {
                if pixel_mask & 0x000000ff == 0 {
                    // rgb at the most significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data is excepted to be [rgb.0, rgb.1, rgb.2, alpha]
                    // otherwise the decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    (3, format.big_endian_flag == 0)
                } else if pixel_mask & 0xff000000 == 0 {
                    // rgb at the least significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    // otherwise the decompressed data should be [rgb.0, rgb.1, rgb.2, alpha]
                    (3, format.big_endian_flag > 0)
                } else {
                    (4, false)
                }
            } else {
                (bpp, false)
            };
        let mut palette = Vec::with_capacity(128 * bpp);
        let mut rect_pixels = vec![0_u8; rect_bytes];

        let mut y = 0;
        while y < rect.height {
            let height = if y + 64 > rect.height {
                rect.height - y
            } else {
                64
            };
            let mut x = 0;
            while x < rect.width {
                let width = if x + 64 > rect.width {
                    rect.width - x
                } else {
                    64
                };
                let tile_rect = Rect {
                    x: rect.x + x,
                    y: rect.y + y,
                    width,
                    height,
                };
                let pixel_count = checked_pixel_count(&tile_rect)?;
                let tile_bytes = pixel_count
                    .checked_mul(bpp)
                    .ok_or(VncError::IntegerOverflow("ZRLE tile bytes"))?;
                ensure_payload_limit(
                    "ZRLE tile payload",
                    tile_bytes,
                    self.limits.max_decoded_payload_bytes,
                )?;

                let control = reader.read_u8()?;
                let is_rle = control & 0x80 > 0;
                let palette_size = control & 0x7f;
                palette.truncate(0);

                for _ in 0..palette_size {
                    copy_true_color(
                        &mut reader,
                        &mut palette,
                        alpha_at_first,
                        compressed_bpp,
                        bpp,
                    )?
                }

                let mut pixels = Vec::with_capacity(tile_bytes);
                match (is_rle, palette_size) {
                    (false, 0) => {
                        // True Color pixels
                        for _ in 0..pixel_count {
                            copy_true_color(
                                &mut reader,
                                &mut pixels,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            )?
                        }
                    }
                    (false, 1) => {
                        // Color fill
                        for _ in 0..pixel_count {
                            copy_indexed(&palette, &mut pixels, bpp, 0)?
                        }
                    }
                    (false, 2..=16) => {
                        // Indexed pixels
                        let bits_per_index = match palette_size {
                            2 => 1,
                            3..=4 => 2,
                            5..=16 => 4,
                            _ => return Err(VncError::InvalidImageData),
                        };
                        let mut encoded = reader.read_u8()?;
                        let mask = (1 << bits_per_index) - 1;

                        for y in 0..height {
                            let mut shift = 8 - bits_per_index;
                            for _ in 0..width {
                                if shift < 0 {
                                    shift = 8 - bits_per_index;
                                    encoded = reader.read_u8()?;
                                }
                                let idx = (encoded >> shift) & mask;

                                copy_indexed(&palette, &mut pixels, bpp, idx)?;
                                shift -= bits_per_index;
                            }
                            if shift < 8 - bits_per_index && y < height - 1 {
                                encoded = reader.read_u8()?;
                            }
                        }
                    }
                    (true, 0) => {
                        // True Color RLE
                        let mut count = 0;
                        let mut pixel = Vec::new();
                        while count < pixel_count {
                            pixel.truncate(0);
                            copy_true_color(
                                &mut reader,
                                &mut pixel,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            )?;
                            let run_length = read_run_length(&mut reader)?;
                            if run_length > pixel_count - count {
                                return Err(VncError::InvalidImageData);
                            }
                            for _ in 0..run_length {
                                pixels.extend(&pixel)
                            }
                            count += run_length;
                        }
                    }
                    (true, 2..=127) => {
                        // Indexed RLE
                        let mut count = 0;
                        while count < pixel_count {
                            let control = reader.read_u8()?;
                            let longer_than_one = control & 0x80 > 0;
                            let index = control & 0x7f;
                            let run_length = if longer_than_one {
                                read_run_length(&mut reader)?
                            } else {
                                1
                            };
                            if usize::from(index) >= usize::from(palette_size)
                                || run_length > pixel_count - count
                            {
                                return Err(VncError::InvalidImageData);
                            }
                            for _ in 0..run_length {
                                copy_indexed(&palette, &mut pixels, bpp, index)?;
                            }
                            count += run_length;
                        }
                    }
                    (x, y) => {
                        error!("ZRLE subencoding error {:?}", (x, y));
                        return Err(VncError::InvalidImageData);
                    }
                }
                if pixels.len() != tile_bytes {
                    return Err(VncError::InvalidImageData);
                }
                copy_tile_to_rect(&mut rect_pixels, rect, &tile_rect, &pixels, bpp)?;
                x += width;
            }
            y += height;
        }

        self.decompressor = Some(reader.into_inner()?);
        output_func(VncEvent::RawImage(*rect, rect_pixels)).await?;

        Ok(())
    }
}

fn copy_tile_to_rect(
    rect_pixels: &mut [u8],
    rect: &Rect,
    tile_rect: &Rect,
    tile_pixels: &[u8],
    bpp: usize,
) -> Result<(), VncError> {
    let tile_row_bytes = usize::from(tile_rect.width)
        .checked_mul(bpp)
        .ok_or(VncError::IntegerOverflow("ZRLE tile row bytes"))?;
    let rect_row_bytes = usize::from(rect.width)
        .checked_mul(bpp)
        .ok_or(VncError::IntegerOverflow("ZRLE rect row bytes"))?;
    let x_offset = usize::from(
        tile_rect
            .x
            .checked_sub(rect.x)
            .ok_or(VncError::InvalidImageData)?,
    )
    .checked_mul(bpp)
    .ok_or(VncError::IntegerOverflow("ZRLE tile x offset"))?;
    let y_offset = usize::from(
        tile_rect
            .y
            .checked_sub(rect.y)
            .ok_or(VncError::InvalidImageData)?,
    );

    for row in 0..usize::from(tile_rect.height) {
        let src_start = row
            .checked_mul(tile_row_bytes)
            .ok_or(VncError::IntegerOverflow("ZRLE tile source row"))?;
        let src_end = src_start
            .checked_add(tile_row_bytes)
            .ok_or(VncError::IntegerOverflow("ZRLE tile source end"))?;
        let dst_start = y_offset
            .checked_add(row)
            .and_then(|y| y.checked_mul(rect_row_bytes))
            .and_then(|start| start.checked_add(x_offset))
            .ok_or(VncError::IntegerOverflow("ZRLE rect destination row"))?;
        let dst_end = dst_start
            .checked_add(tile_row_bytes)
            .ok_or(VncError::IntegerOverflow("ZRLE rect destination end"))?;
        let src = tile_pixels
            .get(src_start..src_end)
            .ok_or(VncError::InvalidImageData)?;
        let dst = rect_pixels
            .get_mut(dst_start..dst_end)
            .ok_or(VncError::InvalidImageData)?;
        dst.copy_from_slice(src);
    }

    Ok(())
}
