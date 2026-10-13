//! 位图缓存 + FastPath 解析扩展。
//!
//! 协议参考：MS-RDPBCGR 2.2.7 (Secondary Drawing Orders)，
//! MS-RDPEGDI 2.2.2.2.1.2.4 (CACHE_BITMAP_REV2_ORDER)。

use crate::model::error::{Error, RdpError, RdpErrorKind, RdpResult};
use std::collections::HashMap;

pub const CACHE_ID_16: u8 = 0;
pub const CACHE_ID_32: u8 = 1;
pub const CACHE_ID_64: u8 = 2;
pub const CACHE_ID_SCREEN: u8 = 0xFF;

#[derive(Default)]
pub struct BitmapCache {
    slots: Vec<Option<Vec<u8>>>,
    slot_dims: Vec<(u16, u16)>,
}

impl BitmapCache {
    pub fn new(count: usize) -> Self {
        Self {
            slots: vec![None; count],
            slot_dims: vec![(0, 0); count],
        }
    }

    pub fn store(&mut self, index: usize, w: u16, h: u16, data: Vec<u8>) {
        if index >= self.slots.len() {
            return;
        }
        self.slots[index] = Some(data);
        self.slot_dims[index] = (w, h);
    }

    pub fn lookup(&self, index: usize) -> Option<(&[u8], u16, u16)> {
        let slot = self.slots.get(index)?.as_ref()?;
        let (w, h) = *self.slot_dims.get(index)?;
        Some((slot.as_slice(), w, h))
    }
}

pub struct CacheTracker {
    caches: HashMap<u8, BitmapCache>,
    offscreen: HashMap<u32, Vec<u8>>,
    offscreen_dims: HashMap<u32, (u16, u16)>,
    announced: bool,
}

impl Default for CacheTracker {
    fn default() -> Self {
        let mut caches = HashMap::new();
        caches.insert(CACHE_ID_16, BitmapCache::new(120));
        caches.insert(CACHE_ID_32, BitmapCache::new(120));
        caches.insert(CACHE_ID_64, BitmapCache::new(337));
        Self {
            caches,
            offscreen: HashMap::new(),
            offscreen_dims: HashMap::new(),
            announced: false,
        }
    }
}

impl CacheTracker {
    pub fn capability_set_rev2() -> [u8; 20] {
        let mut buf = [0u8; 20];
        buf[0..2].copy_from_slice(&0x0013u16.to_le_bytes());
        buf[2..4].copy_from_slice(&20u16.to_le_bytes());
        buf[4] = 3;
        buf[5..9].copy_from_slice(&120u32.to_le_bytes());
        buf[9..13].copy_from_slice(&120u32.to_le_bytes());
        buf[13..17].copy_from_slice(&337u32.to_le_bytes());
        buf
    }

    pub fn mark_announced(&mut self) {
        self.announced = true;
    }

    pub fn is_announced(&self) -> bool {
        self.announced
    }

    pub fn store_bitmap(
        &mut self,
        cache_id: u8,
        index: usize,
        do_not_cache: bool,
        w: u16,
        h: u16,
        data: Vec<u8>,
    ) -> RdpResult<()> {
        let cache = self.caches.get_mut(&cache_id).ok_or_else(|| {
            Error::RdpError(RdpError::new(
                RdpErrorKind::InvalidData,
                &format!("unknown bitmap cache id: {cache_id}"),
            ))
        })?;
        let effective_index = if do_not_cache {
            cache.slots.len().saturating_sub(1)
        } else {
            index
        };
        cache.store(effective_index, w, h, data);
        Ok(())
    }

    pub fn blit_from_cache(
        &self,
        framebuffer: &mut [u8],
        fb_w: u16,
        fb_h: u16,
        cache_id: u8,
        index: usize,
        dst_x: u16,
        dst_y: u16,
    ) -> RdpResult<()> {
        let (src, src_w, src_h) = self
            .caches
            .get(&cache_id)
            .and_then(|c| c.lookup(index))
            .ok_or_else(|| {
                Error::RdpError(RdpError::new(
                    RdpErrorKind::InvalidData,
                    &format!("cache miss: cache_id={cache_id} index={index}"),
                ))
            })?;
        blit_bgra(framebuffer, fb_w, fb_h, src, src_w, src_h, dst_x, dst_y);
        Ok(())
    }

    pub fn store_offscreen(&mut self, key: u32, w: u16, h: u16, data: Vec<u8>) {
        self.offscreen.insert(key, data);
        self.offscreen_dims.insert(key, (w, h));
    }

    pub fn lookup_offscreen(&self, key: u32) -> Option<(&[u8], u16, u16)> {
        let data = self.offscreen.get(&key)?.as_slice();
        let (w, h) = *self.offscreen_dims.get(&key)?;
        Some((data, w, h))
    }
}

fn blit_bgra(
    fb: &mut [u8],
    fb_w: u16,
    fb_h: u16,
    src: &[u8],
    src_w: u16,
    src_h: u16,
    dst_x: u16,
    dst_y: u16,
) {
    let fb_w = fb_w as usize;
    let fb_h = fb_h as usize;
    let dst_x = dst_x as usize;
    let dst_y = dst_y as usize;
    let copy_w = (src_w as usize).min(fb_w.saturating_sub(dst_x));
    let copy_h = (src_h as usize).min(fb_h.saturating_sub(dst_y));
    if copy_w == 0 || copy_h == 0 {
        return;
    }
    let src_stride = src_w as usize * 4;
    let fb_stride = fb_w * 4;
    for row in 0..copy_h {
        let src_off = row * src_stride;
        let dst_off = (dst_y + row) * fb_stride + dst_x * 4;
        let len = copy_w * 4;
        if src_off + len <= src.len() && dst_off + len <= fb.len() {
            fb[dst_off..dst_off + len].copy_from_slice(&src[src_off..src_off + len]);
        }
    }
}

pub struct VarIntCursor<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> VarIntCursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn read_u8(&mut self) -> RdpResult<u8> {
        if self.pos >= self.buf.len() {
            return Err(Error::RdpError(RdpError::new(
                RdpErrorKind::InvalidData,
                "VarIntCursor: out of bounds u8",
            )));
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16_le(&mut self) -> RdpResult<u16> {
        if self.pos + 2 > self.buf.len() {
            return Err(Error::RdpError(RdpError::new(
                RdpErrorKind::InvalidData,
                "VarIntCursor: out of bounds u16",
            )));
        }
        let v = u16::from_le_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u32_le(&mut self) -> RdpResult<u32> {
        if self.pos + 4 > self.buf.len() {
            return Err(Error::RdpError(RdpError::new(
                RdpErrorKind::InvalidData,
                "VarIntCursor: out of bounds u32",
            )));
        }
        let v = u32::from_le_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_var(&mut self) -> RdpResult<u32> {
        let b0 = self.read_u8()?;
        let mask = b0 & 0xC0;
        let v0 = (b0 & 0x3F) as u32;
        match mask {
            0x00 => Ok(v0),
            0x40 => {
                let b1 = self.read_u8()? as u32;
                Ok(v0 | (b1 << 6))
            }
            0x80 => {
                let b1 = self.read_u8()? as u32;
                let b2 = self.read_u8()? as u32;
                Ok(v0 | (b1 << 6) | (b2 << 14))
            }
            0xC0 => {
                let b1 = self.read_u8()? as u32;
                let b2 = self.read_u8()? as u32;
                let b3 = self.read_u8()? as u32;
                Ok(v0 | (b1 << 6) | (b2 << 14) | (b3 << 22))
            }
            _ => unreachable!(),
        }
    }
}

pub struct CacheBitmapRev2 {
    pub cache_id: u8,
    pub bits_per_pixel: u8,
    pub flags: u16,
    pub width: u16,
    pub height: u16,
    pub bitmap_length: u32,
    pub cache_index: u32,
    pub data: Vec<u8>,
}

pub const CBR2_HEIGHT_SAME_AS_WIDTH: u16 = 0x0001;
pub const CBR2_DO_NOT_CACHE: u16 = 0x0002;

impl CacheBitmapRev2 {
    pub fn parse(cursor: &mut VarIntCursor) -> RdpResult<Self> {
        let b = cursor.read_u8()?;
        let cache_id = (b >> 5) & 0x07;
        let bpp = b & 0x1F;
        let flags = cursor.read_u16_le()?;
        let width = cursor.read_var()? as u16;
        let height = if flags & CBR2_HEIGHT_SAME_AS_WIDTH != 0 {
            width
        } else {
            cursor.read_var()? as u16
        };
        let bitmap_length = cursor.read_var()?;
        let cache_index = cursor.read_var()?;
        if cursor.pos + bitmap_length as usize > cursor.buf.len() {
            return Err(Error::RdpError(RdpError::new(
                RdpErrorKind::InvalidData,
                "CACHE_BITMAP_REV2: data stream out of bounds",
            )));
        }
        let data = cursor.buf[cursor.pos..cursor.pos + bitmap_length as usize].to_vec();
        cursor.pos += bitmap_length as usize;
        Ok(Self {
            cache_id,
            bits_per_pixel: bpp,
            flags,
            width,
            height,
            bitmap_length,
            cache_index,
            data,
        })
    }

    pub fn is_do_not_cache(&self) -> bool {
        self.flags & CBR2_DO_NOT_CACHE != 0
    }
}

pub mod fastpath {
    pub const FASTPATH_UPDATETYPE_ORDERS: u8 = 0x0;
    pub const FASTPATH_UPDATETYPE_BITMAP: u8 = 0x1;
    pub const FASTPATH_UPDATETYPE_PALETTE: u8 = 0x2;
    pub const FASTPATH_UPDATETYPE_SYNCHRONIZE: u8 = 0x3;
    pub const FASTPATH_UPDATETYPE_SURFCMDS: u8 = 0x4;
    pub const FASTPATH_UPDATETYPE_PTR_NULL: u8 = 0x5;
    pub const FASTPATH_UPDATETYPE_PTR_DEFAULT: u8 = 0x6;
    pub const FASTPATH_UPDATETYPE_PTR_POSITION: u8 = 0x8;
    pub const FASTPATH_UPDATETYPE_COLOR: u8 = 0x9;
    pub const FASTPATH_UPDATETYPE_CACHED: u8 = 0xA;
    pub const FASTPATH_UPDATETYPE_POINTER: u8 = 0xB;
}

pub fn parse_fastpath_header(b: u8) -> (bool, u8) {
    let output_update = (b & 0x02) != 0;
    let update_code = b & 0x0F;
    (output_update, update_code)
}

pub fn parse_fastpath_updates(buf: &[u8]) -> RdpResult<Vec<(u8, usize, usize)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let b = buf[pos];
        pos += 1;
        let update_code = b & 0x0F;
        if pos >= buf.len() {
            break;
        }
        let size_byte = buf[pos];
        pos += 1;
        let len = if size_byte < 0x80 {
            size_byte as usize
        } else {
            if pos >= buf.len() {
                break;
            }
            let hi = (size_byte & 0x7F) as usize;
            let lo = buf[pos] as usize;
            pos += 1;
            (hi << 8) | lo
        };
        if pos + len > buf.len() {
            return Err(Error::RdpError(RdpError::new(
                RdpErrorKind::InvalidData,
                &format!("FastPath update out of bounds: code={update_code} len={len}"),
            )));
        }
        out.push((update_code, pos, pos + len));
        pos += len;
    }
    Ok(out)
}