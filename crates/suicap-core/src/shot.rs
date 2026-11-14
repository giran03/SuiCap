//! One-off screenshot (plan §6.3): the sole place a video frame is read back to the CPU
//! (rule 1 exception). It copies the newest converted NV12 frame to a staging texture,
//! converts it to RGBA (BT.709 limited, as encoded) and writes a PNG on a worker thread.
//! FFI module.

use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};

#[derive(Debug, thiserror::Error)]
pub enum ShotError {
    #[error(transparent)]
    Win(#[from] windows::core::Error),
    #[error("png: {0}")]
    Png(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the frame is empty")]
    Empty,
}

/// Straight-alpha RGBA8, row-major.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Read `w`x`h` pixels of NV12 texture `tex` (array slice `slice`) back to the CPU.
pub fn read_nv12(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    slice: u32,
    w: u32,
    h: u32,
) -> Result<Image, ShotError> {
    if w == 0 || h == 0 {
        return Err(ShotError::Empty);
    }
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: GetDesc fills a local.
    unsafe { tex.GetDesc(&mut desc) };
    let (w, h) = (w.min(desc.Width), h.min(desc.Height));
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    // SAFETY: desc is a valid local; the out-param is a local.
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.ok_or(ShotError::Empty)?;
    let src_sub = slice * desc.MipLevels.max(1);
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: both textures are live and the same format/size; the mapping is released
    // before the staging texture is dropped, and every read stays inside the mapped
    // planes (Y: pitch*height, UV: pitch*height/2 right after it).
    let image = unsafe {
        ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, tex, src_sub, None);
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        let plane = std::slice::from_raw_parts(base, pitch * desc.Height as usize * 3 / 2);
        let image = nv12_to_rgba(plane, pitch, desc.Height as usize, w as usize, h as usize);
        ctx.Unmap(&staging, 0);
        image
    };
    Ok(Image {
        width: w,
        height: h,
        rgba: image,
    })
}

/// `plane`: Y rows (`pitch` bytes, `alloc_h` rows) followed by interleaved UV rows.
fn nv12_to_rgba(plane: &[u8], pitch: usize, alloc_h: usize, w: usize, h: usize) -> Vec<u8> {
    let (y_plane, uv_plane) = plane.split_at(pitch * alloc_h);
    let mut out = vec![255u8; w * h * 4];
    for row in 0..h {
        let y_row = &y_plane[row * pitch..row * pitch + w];
        let uv_row = &uv_plane[(row / 2) * pitch..(row / 2) * pitch + (w & !1).max(2).min(pitch)];
        let o = &mut out[row * w * 4..(row + 1) * w * 4];
        for x in 0..w {
            let u = uv_row.get(x & !1).copied().unwrap_or(128);
            let v = uv_row.get((x & !1) + 1).copied().unwrap_or(128);
            let [r, g, b] = yuv709_limited(y_row[x], u, v);
            o[x * 4..x * 4 + 3].copy_from_slice(&[r, g, b]);
        }
    }
    out
}

/// BT.709, limited range (16..235 luma, 16..240 chroma) to full-range RGB.
fn yuv709_limited(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = 1.164_383 * (f32::from(y) - 16.0);
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    let clamp = |x: f32| x.round().clamp(0.0, 255.0) as u8;
    [
        clamp(y + 1.792_741 * v),
        clamp(y - 0.213_249 * u - 0.532_909 * v),
        clamp(y + 2.112_402 * u),
    ]
}

/// Write `image` as `path` (via `<path>.tmp`, then renamed).
pub fn write_png(image: &Image, path: &Path) -> Result<PathBuf, ShotError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| -> Result<(), ShotError> {
        let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut enc = png::Encoder::new(file, image.width, image.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().map_err(|e| ShotError::Png(e.to_string()))?;
        w.write_image_data(&image.rgba)
            .map_err(|e| ShotError::Png(e.to_string()))?;
        w.finish().map_err(|e| ShotError::Png(e.to_string()))?;
        Ok(())
    })();
    match result.and_then(|()| crate::mux::replace_file(&tmp, path).map_err(Into::into)) {
        Ok(()) => Ok(path.to_path_buf()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_range_black_white_and_grey() {
        assert_eq!(yuv709_limited(16, 128, 128), [0, 0, 0]);
        assert_eq!(yuv709_limited(235, 128, 128), [255, 255, 255]);
        let [r, g, b] = yuv709_limited(126, 128, 128);
        assert!(r == g && g == b && (126..=130).contains(&r));
    }

    #[test]
    fn converts_a_padded_plane_and_crops() {
        // 4x2 visible, pitch 8, allocated height 2: grey luma, neutral chroma.
        let (pitch, alloc_h) = (8, 2);
        let mut plane = vec![126u8; pitch * alloc_h];
        plane.extend(std::iter::repeat_n(128u8, pitch));
        let out = nv12_to_rgba(&plane, pitch, alloc_h, 4, 2);
        assert_eq!(out.len(), 4 * 2 * 4);
        assert!(out.chunks(4).all(|p| p[3] == 255 && p[0] == p[1] && p[1] == p[2]));
    }

    #[test]
    fn png_roundtrip_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("suicap-shot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = Image {
            width: 2,
            height: 2,
            rgba: vec![255; 16],
        };
        let path = write_png(&img, &dir.join("a.png")).unwrap();
        assert_eq!(&std::fs::read(&path).unwrap()[1..4], b"PNG");
        assert!(!dir.join("a.png.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
