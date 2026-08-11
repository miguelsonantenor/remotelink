//! Thin video frame types compatible with `remotelink-media` capture traits.
//!
//! Kept local (no `media` crate dependency) to avoid merge coupling with PR 9.
//! Field names and PTS semantics match media's `VideoFrame` / `PixelFormat`
//! so a later thin adapter is mechanical. DXGI natively produces [`PixelFormat::Bgra8`].

use std::time::Duration;

/// Pixel layout of a video frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit BGRA, tightly packed (4 bytes/pixel). DXGI Desktop Duplication native.
    Bgra8,
    /// 8-bit RGBA (4 bytes/pixel).
    Rgba8,
    /// 8-bit RGB, tightly packed (3 bytes/pixel).
    Rgb24,
}

impl PixelFormat {
    /// Bytes per pixel for tightly packed formats.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => 4,
            PixelFormat::Rgb24 => 3,
        }
    }
}

/// A captured (or mock) video frame with host-monotonic PTS.
///
/// `pts_host_mono` is measured with [`host_mono_now`] at capture time and is
/// suitable for RTP mapping via a shared session epoch `t0` (see DESIGN A/V
/// timing contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Host monotonic time of capture.
    pub pts_host_mono: Duration,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes (may exceed `width * bpp` for DXGI staging pitch).
    pub stride: u32,
    /// Pixel layout of [`Self::data`].
    pub format: PixelFormat,
    /// Pixel data, row-major. Each row is `stride` bytes; active width is
    /// `width * format.bytes_per_pixel()`.
    pub data: Vec<u8>,
}

impl VideoFrame {
    /// Expected tightly packed byte length (ignoring stride padding).
    pub fn packed_len(&self) -> usize {
        (self.width as usize)
            .saturating_mul(self.height as usize)
            .saturating_mul(self.format.bytes_per_pixel())
    }

    /// Expected buffer length given stride (one full row pitch per scanline).
    pub fn expected_len(&self) -> usize {
        (self.stride as usize).saturating_mul(self.height as usize)
    }

    /// Returns true if dimensions, stride, and buffer length are consistent.
    pub fn is_well_formed(&self) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let min_stride = self.width as usize * self.format.bytes_per_pixel();
        if (self.stride as usize) < min_stride {
            return false;
        }
        self.data.len() == self.expected_len()
    }

    /// Create a tightly packed frame (`stride = width * bpp`).
    pub fn packed(
        pts_host_mono: Duration,
        width: u32,
        height: u32,
        format: PixelFormat,
        data: Vec<u8>,
    ) -> Self {
        let stride = width.saturating_mul(format.bytes_per_pixel() as u32);
        Self {
            pts_host_mono,
            width,
            height,
            stride,
            format,
            data,
        }
    }
}

/// Host-monotonic clock used for capture timestamps (`host_mono`).
///
/// - **Windows:** `QueryPerformanceCounter` / `QueryPerformanceFrequency`
/// - **Other:** process-relative `Instant` (tests / non-Windows CI)
pub fn host_mono_now() -> Duration {
    #[cfg(windows)]
    {
        windows_host_mono()
    }
    #[cfg(not(windows))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }
}

#[cfg(windows)]
fn windows_host_mono() -> Duration {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    // Frequency is constant for the process lifetime.
    use std::sync::OnceLock;
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: QPF with a valid out-pointer; fails only on ancient systems.
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        if f <= 0 {
            1
        } else {
            f
        }
    });

    let mut counter = 0i64;
    // SAFETY: QPC with a valid out-pointer.
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
    }
    if counter < 0 {
        return Duration::ZERO;
    }
    let counter = counter as u64;
    let freq = freq as u64;
    let secs = counter / freq;
    let rem = counter % freq;
    let nanos = rem.saturating_mul(1_000_000_000) / freq;
    Duration::new(secs, nanos as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_format_bpp() {
        assert_eq!(PixelFormat::Bgra8.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgb24.bytes_per_pixel(), 3);
    }

    #[test]
    fn packed_frame_well_formed() {
        let f = VideoFrame::packed(
            Duration::from_millis(1),
            2,
            2,
            PixelFormat::Bgra8,
            vec![0; 2 * 2 * 4],
        );
        assert!(f.is_well_formed());
        assert_eq!(f.stride, 8);
        assert_eq!(f.packed_len(), 16);
        assert_eq!(f.expected_len(), 16);
    }

    #[test]
    fn strided_frame_well_formed() {
        // width 2 BGRA needs 8 bytes; stride 16 with padding.
        let f = VideoFrame {
            pts_host_mono: Duration::ZERO,
            width: 2,
            height: 1,
            stride: 16,
            format: PixelFormat::Bgra8,
            data: vec![0; 16],
        };
        assert!(f.is_well_formed());
        assert_eq!(f.packed_len(), 8);
        assert_eq!(f.expected_len(), 16);
    }

    #[test]
    fn host_mono_is_monotonic() {
        let a = host_mono_now();
        let b = host_mono_now();
        assert!(b >= a);
    }
}
