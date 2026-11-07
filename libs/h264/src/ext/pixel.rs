//! Sample storage. 8-bit streams keep `u8` planes, deeper ones `u16`, and the
//! code above this is generic over which, so that an interlaced 8-bit stream
//! costs no more memory than a progressive one.

use alloc::sync::Arc;
use alloc::vec::Vec;

/// One sample of a plane.
pub(crate) trait Pixel: Copy + Default + PartialEq + Send + Sync + 'static {
    fn get(self) -> i32;
    /// The value must already be in range; it isn't checked.
    fn new(value: i32) -> Self;
    /// The 8-bit plane behind `plane` when no conversion is needed to show it.
    fn share_u8(plane: &Arc<Vec<Self>>) -> Option<Arc<Vec<u8>>>;
    /// Wraps a finished plane for [`crate::decoder::Frame::native`].
    fn native(plane: &Arc<Vec<Self>>) -> crate::decoder::NativePlane;
}

impl Pixel for u8 {
    #[inline(always)]
    fn get(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn new(value: i32) -> Self {
        value as u8
    }
    fn share_u8(plane: &Arc<Vec<u8>>) -> Option<Arc<Vec<u8>>> {
        Some(plane.clone())
    }
    fn native(plane: &Arc<Vec<u8>>) -> crate::decoder::NativePlane {
        crate::decoder::NativePlane::U8(plane.clone())
    }
}

impl Pixel for u16 {
    #[inline(always)]
    fn get(self) -> i32 {
        self as i32
    }
    #[inline(always)]
    fn new(value: i32) -> Self {
        value as u16
    }
    fn share_u8(_: &Arc<Vec<u16>>) -> Option<Arc<Vec<u8>>> {
        None
    }
    fn native(plane: &Arc<Vec<u16>>) -> crate::decoder::NativePlane {
        crate::decoder::NativePlane::U16(plane.clone())
    }
}
