//! DXGI Desktop Duplication capture (Windows interactive session only).
//!
//! # Secure desktop / UAC
//!
//! Desktop Duplication does **not** capture the Winlogon / UAC secure desktop.
//! When the host enters a secure desktop, [`AcquireNextFrame`](DxgiDesktopDuplication::next_frame)
//! typically fails with access-lost; the capturer must be re-created after the
//! user returns to a normal desktop. Remote interaction with UAC prompts is
//! out of scope for v1 (DESIGN: secure desktop known gap).
//!
//! # Pixel format (v1)
//!
//! Only **`DXGI_FORMAT_B8G8R8A8_UNORM`** (BGRA8) desktop images are supported.
//! HDR / 10-bit outputs that duplicate as another format return a clear
//! [`CaptureError::Device`] rather than failing later at `CopyResource`.

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};

use super::frame::{host_mono_now, PixelFormat, VideoFrame};
use super::source::{CaptureConfig, CaptureError, VideoSource};

/// DXGI Desktop Duplication capturer for a single output.
pub struct DxgiDesktopDuplication {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    timeout_ms: u32,
}

impl std::fmt::Debug for DxgiDesktopDuplication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiDesktopDuplication")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("timeout_ms", &self.timeout_ms)
            .finish_non_exhaustive()
    }
}

impl DxgiDesktopDuplication {
    /// Open Desktop Duplication for `config.display_index` (0 = primary adapter/output).
    pub fn open(config: &CaptureConfig) -> Result<Self, CaptureError> {
        // SAFETY: COM DXGI/D3D11 APIs; pointers are stack locals owned by this function.
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| CaptureError::Device(e.to_string()))?;

            let adapter = factory
                .EnumAdapters1(0)
                .map_err(|e| CaptureError::Device(format!("enum adapter: {e}")))?;

            let output = adapter
                .EnumOutputs(config.display_index)
                .map_err(|_| CaptureError::DisplayNotFound(config.display_index))?;

            let mut feature_level = D3D_FEATURE_LEVEL_11_0;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            // When passing a concrete adapter, DriverType must be UNKNOWN (MSDN).
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
            .map_err(|e| CaptureError::Device(format!("D3D11CreateDevice: {e}")))?;

            let device = device.ok_or_else(|| CaptureError::Device("null D3D device".into()))?;
            let context = context.ok_or_else(|| CaptureError::Device("null D3D context".into()))?;

            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| CaptureError::Device(format!("IDXGIOutput1: {e}")))?;

            let duplication = output1.DuplicateOutput(&device).map_err(|e| {
                // ACCESS_DENIED often means secure desktop / no interactive session.
                let code = e.code();
                if code == windows::Win32::Foundation::E_ACCESSDENIED {
                    CaptureError::AccessLost
                } else {
                    CaptureError::Device(format!("DuplicateOutput: {e}"))
                }
            })?;

            let desc: DXGI_OUTPUT_DESC = output
                .GetDesc()
                .map_err(|e| CaptureError::Device(format!("GetDesc: {e}")))?;

            let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
            let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
            if width == 0 || height == 0 {
                return Err(CaptureError::Device(
                    "output desktop rect has zero size".into(),
                ));
            }

            Ok(Self {
                _device: device,
                context,
                duplication,
                staging: None,
                width,
                height,
                timeout_ms: config.timeout_ms,
            })
        }
    }

    fn ensure_staging(&mut self, src: &ID3D11Texture2D) -> Result<(), CaptureError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: texture from DXGI frame; GetDesc writes into stack desc.
        unsafe {
            src.GetDesc(&mut desc);
        }

        // v1: BGRA8 only. CopyResource requires matching formats; reject early.
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::Device(format!(
                "unsupported desktop format 0x{:x} (v1 requires DXGI_FORMAT_B8G8R8A8_UNORM / BGRA8)",
                desc.Format.0
            )));
        }
        if desc.SampleDesc.Count != 1 {
            return Err(CaptureError::Device(format!(
                "unsupported desktop sample count {} (v1 requires 1)",
                desc.SampleDesc.Count
            )));
        }

        let need_new = match &self.staging {
            None => true,
            Some(_) => {
                // Recreate if desktop mode changed size.
                desc.Width != self.width || desc.Height != self.height
            }
        };
        if !need_new {
            // Still refresh cached size from the desktop texture.
            self.width = desc.Width;
            self.height = desc.Height;
            return Ok(());
        }
        self.width = desc.Width;
        self.height = desc.Height;

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            // Match desktop image format (validated BGRA8 above).
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };

        // SAFETY: CreateTexture2D with valid device and desc.
        let staging = unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self._device
                .CreateTexture2D(&staging_desc, None, Some(&mut tex))
                .map_err(|e| CaptureError::Device(format!("CreateTexture2D staging: {e}")))?;
            tex.ok_or_else(|| CaptureError::Device("null staging texture".into()))?
        };
        self.staging = Some(staging);
        Ok(())
    }

    fn copy_frame(&mut self, src: &ID3D11Texture2D) -> Result<VideoFrame, CaptureError> {
        self.ensure_staging(src)?;
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| CaptureError::Device("staging missing".into()))?;

        let pts = host_mono_now();

        // SAFETY: both textures are D3D11 resources on this device/context;
        // formats match (BGRA8 validated in ensure_staging).
        unsafe {
            self.context.CopyResource(staging, src);
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: Map staging for CPU read; UnmapOnDrop releases on all paths
        // including unwind.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Device(format!("Map staging: {e}")))?;
        }
        let _unmap = UnmapOnDrop {
            context: &self.context,
            resource: staging,
        };

        let stride = mapped.RowPitch;
        let height = self.height;
        let byte_len = (stride as usize).saturating_mul(height as usize);
        if mapped.pData.is_null() || byte_len == 0 {
            return Err(CaptureError::Device("mapped frame is empty".into()));
        }
        // SAFETY: pData points at `byte_len` readable bytes for the mapped subresource.
        let slice = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, byte_len) };
        let data = slice.to_vec();
        let frame = VideoFrame {
            pts_host_mono: pts,
            width: self.width,
            height: self.height,
            stride,
            format: PixelFormat::Bgra8,
            data,
        };
        if !frame.is_well_formed() {
            return Err(CaptureError::Device(
                "captured frame failed well-formed check".into(),
            ));
        }
        Ok(frame)
        // `_unmap` drops here → Unmap
    }
}

/// RAII guard so a mapped staging texture is always unmapped, including on unwind.
struct UnmapOnDrop<'a> {
    context: &'a ID3D11DeviceContext,
    resource: &'a ID3D11Texture2D,
}

impl Drop for UnmapOnDrop<'_> {
    fn drop(&mut self) {
        // SAFETY: paired with a successful Map on the same resource/subresource.
        unsafe {
            self.context.Unmap(self.resource, 0);
        }
    }
}

impl VideoSource for DxgiDesktopDuplication {
    type Error = CaptureError;

    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // SAFETY: AcquireNextFrame; must ReleaseFrame after success.
        let acquire = unsafe {
            self.duplication
                .AcquireNextFrame(self.timeout_ms, &mut frame_info, &mut resource)
        };

        match acquire {
            Ok(()) => {}
            Err(e) => {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                if code == DXGI_ERROR_ACCESS_LOST {
                    return Err(CaptureError::AccessLost);
                }
                return Err(CaptureError::Device(format!("AcquireNextFrame: {e}")));
            }
        }

        let result = (|| {
            let resource =
                resource.ok_or_else(|| CaptureError::Device("null frame resource".into()))?;
            // SAFETY: QI to texture2D for the desktop image.
            let texture: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| CaptureError::Device(format!("frame as Texture2D: {e}")))?;
            // Only copy when there is desktop image data (or first frame).
            if frame_info.LastPresentTime == 0 && frame_info.AccumulatedFrames == 0 {
                // No new pixels; still release.
                return Ok(None);
            }
            self.copy_frame(&texture).map(Some)
        })();

        // SAFETY: every successful AcquireNextFrame requires ReleaseFrame.
        let release = unsafe { self.duplication.ReleaseFrame() };
        if let Err(e) = release {
            let code = e.code();
            if code == DXGI_ERROR_ACCESS_LOST {
                return Err(CaptureError::AccessLost);
            }
            // Prefer reporting copy/acquire errors first.
            if result.is_ok() {
                return Err(CaptureError::Device(format!("ReleaseFrame: {e}")));
            }
        }
        result
    }
}
