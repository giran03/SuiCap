//! Colour convert + scale in one `VideoProcessorBlt`, written straight into the FFmpeg
//! hwframe texture (plan §4.2). FFI module.
//!
//! SDR only for now: BGRA sRGB full range -> NV12 BT.709 limited. Output size is fixed
//! by the preset; the source is letterboxed so resolution changes never reach the encoder.

use std::collections::HashMap;

use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct3D11::{
                D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
            },
            Dxgi::{
                Common::{
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_RATIONAL,
                },
                DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
            },
        },
    },
    core::Interface,
};

use crate::capture::Frame;

#[derive(Debug, thiserror::Error)]
pub enum PreprocessError {
    #[error(transparent)]
    Win(#[from] windows::core::Error),
}

impl PreprocessError {
    /// Device removed/reset is recoverable by rebuilding the device (plan §7).
    pub fn is_recoverable(&self) -> bool {
        let PreprocessError::Win(e) = self;
        e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET
    }
}

type Result<T> = std::result::Result<T, PreprocessError>;

/// Views are tied to a texture and the enumerator; cache a few, rebuild on size change.
const VIEW_CACHE_MAX: usize = 16;

struct Processor {
    in_w: u32,
    in_h: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
}

pub struct Preprocessor {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    out_w: u32,
    out_h: u32,
    fps: u32,
    proc: Option<Processor>,
    in_views: HashMap<usize, ID3D11VideoProcessorInputView>,
    out_views: HashMap<usize, ID3D11VideoProcessorOutputView>,
}

impl Preprocessor {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        Ok(Self {
            vdev: device.cast()?,
            vctx: context.cast()?,
            out_w,
            out_h,
            fps,
            proc: None,
            in_views: HashMap::new(),
            out_views: HashMap::new(),
        })
    }

    fn processor(&mut self, in_w: u32, in_h: u32) -> Result<&Processor> {
        if self.proc.as_ref().is_none_or(|p| p.in_w != in_w || p.in_h != in_h) {
            self.in_views.clear();
            self.out_views.clear();
            let rate = DXGI_RATIONAL {
                Numerator: self.fps,
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: self.out_w,
                OutputHeight: self.out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            // SAFETY: desc is a valid local; returned interfaces are owned.
            let (enumerator, vp) = unsafe {
                let e = self.vdev.CreateVideoProcessorEnumerator(&desc)?;
                let vp = self.vdev.CreateVideoProcessor(&e, 0)?;
                (e, vp)
            };
            let dst = letterbox(in_w, in_h, self.out_w, self.out_h);
            let src = RECT {
                left: 0,
                top: 0,
                right: in_w as i32,
                bottom: in_h as i32,
            };
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            // SAFETY: plain state setters on a live processor; rect/colour are locals.
            unsafe {
                let c = &self.vctx;
                c.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
                c.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
                c.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                c.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
                c.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&src));
                c.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dst));
                c.VideoProcessorSetOutputBackgroundColor(&vp, false, &black);
            }
            self.proc = Some(Processor {
                in_w,
                in_h,
                enumerator,
                vp,
            });
        }
        Ok(self.proc.as_ref().expect("set above"))
    }

    /// Convert `src` into `dst` (an NV12 hwframe texture, array slice `dst_index`).
    pub fn blt(&mut self, src: &Frame, dst: &ID3D11Texture2D, dst_index: u32) -> Result<()> {
        let (enumerator, vp) = {
            let p = self.processor(src.width, src.height)?;
            (p.enumerator.clone(), p.vp.clone())
        };
        let in_view = cached(&mut self.in_views, src.tex.as_raw() as usize, || {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut view = None;
            // SAFETY: texture and enumerator are live; out-param is a local.
            unsafe {
                self.vdev
                    .CreateVideoProcessorInputView(&src.tex, &enumerator, &desc, Some(&mut view))?
            };
            Ok(view.expect("created on success"))
        })?;
        let out_key = dst.as_raw() as usize ^ ((dst_index as usize) << 48);
        let out_view = cached(&mut self.out_views, out_key, || {
            // Pool textures are single (initial_pool_size = 0), so slice is always 0.
            debug_assert_eq!(dst_index, 0);
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view = None;
            // SAFETY: as above.
            unsafe {
                self.vdev
                    .CreateVideoProcessorOutputView(dst, &enumerator, &desc, Some(&mut view))?
            };
            Ok(view.expect("created on success"))
        })?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
            ..Default::default()
        };
        let mut streams = [stream];
        // SAFETY: all views/processor are live for the call; the stream's ManuallyDrop
        // input view is released right after.
        let r = unsafe { self.vctx.VideoProcessorBlt(&vp, &out_view, 0, &streams) };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        Ok(r?)
    }
}

fn cached<V: Clone>(
    map: &mut HashMap<usize, V>,
    key: usize,
    make: impl FnOnce() -> std::result::Result<V, windows::core::Error>,
) -> Result<V> {
    if let Some(v) = map.get(&key) {
        return Ok(v.clone());
    }
    if map.len() >= VIEW_CACHE_MAX {
        map.clear();
    }
    let v = make()?;
    map.insert(key, v.clone());
    Ok(v)
}

/// Largest rect with the source aspect centred in the output, on even pixels (NV12).
pub fn letterbox(in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> RECT {
    let (iw, ih, ow, oh) = (
        u64::from(in_w.max(1)),
        u64::from(in_h.max(1)),
        u64::from(out_w),
        u64::from(out_h),
    );
    let (w, h) = if iw * oh > ih * ow {
        (ow, ih * ow / iw)
    } else {
        (iw * oh / ih, oh)
    };
    let (w, h) = (w & !1, h & !1);
    let left = ((ow - w) / 2) & !1;
    let top = ((oh - h) / 2) & !1;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + w) as i32,
        bottom: (top + h) as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rr: i32, b: i32) -> RECT {
        RECT {
            left: l,
            top: t,
            right: rr,
            bottom: b,
        }
    }

    #[test]
    fn same_aspect_fills() {
        assert_eq!(letterbox(2560, 1440, 1920, 1080), r(0, 0, 1920, 1080));
    }

    #[test]
    fn portrait_is_pillarboxed() {
        // 1080x2560 into 1920x1080 -> 454 wide (even), centred.
        let d = letterbox(1080, 2560, 1920, 1080);
        assert_eq!((d.top, d.bottom), (0, 1080));
        assert_eq!((d.right - d.left) % 2, 0);
        assert!((d.left - (1920 - d.right)).abs() <= 2);
    }

    #[test]
    fn ultrawide_is_letterboxed() {
        let d = letterbox(3440, 1440, 1920, 1080);
        assert_eq!((d.left, d.right), (0, 1920));
        assert_eq!(d.bottom - d.top, 802);
    }
}
