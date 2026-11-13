//! `suicap-cli probe`: what this machine can actually do (plan §8 P0).
//! Encoders are reported only if `avcodec_open2` succeeds on our own D3D11 device.

use std::fmt;

use crate::{
    capture::{self, Adapter, Vendor, WgcSupport},
    config::{Codec, VideoPreset},
    encode::{self, Encoder, EncoderSettings, HwDevice, HwFrames, SwFormat},
};

pub struct Report {
    pub os_build: Option<u32>,
    pub wgc: WgcSupport,
    pub adapters: Vec<AdapterReport>,
}

pub struct AdapterReport {
    pub index: u32,
    pub name: String,
    pub vendor: Vendor,
    pub vram_mb: u64,
    pub software: bool,
    pub device: Result<(), String>,
    pub outputs: Vec<OutputReport>,
    pub encoders: Vec<EncoderReport>,
}

pub struct OutputReport {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh_hz: u32,
    pub attached: bool,
    pub dda: Result<(), String>,
}

pub struct EncoderReport {
    pub codec: Codec,
    pub name: String,
    pub result: Result<EncoderOk, String>,
}

pub struct EncoderOk {
    pub extradata_len: usize,
    pub unused_options: Vec<String>,
}

pub fn run() -> crate::Result<Report> {
    capture::com_init();
    // SUICAP_FFMPEG_LOG=1 shows FFmpeg's own diagnostics while probing.
    encode::set_log_quiet(std::env::var_os("SUICAP_FFMPEG_LOG").is_none());
    let adapters = capture::adapters()?
        .iter()
        .map(probe_adapter)
        .collect::<crate::Result<Vec<_>>>()?;
    encode::set_log_quiet(false);
    Ok(Report {
        os_build: os_build(),
        wgc: capture::wgc_support(),
        adapters,
    })
}

fn probe_adapter(a: &Adapter) -> crate::Result<AdapterReport> {
    let device = if a.software {
        Err("software adapter skipped".to_string())
    } else {
        capture::create_device(a).map_err(|e| e.to_string())
    };
    let outputs = capture::outputs(a)?
        .iter()
        .map(|o| OutputReport {
            name: o.device_name.clone(),
            width: o.width,
            height: o.height,
            refresh_hz: o.refresh_hz,
            attached: o.attached,
            dda: match &device {
                Ok((dev, _)) => capture::dda_check(o, dev).map_err(|e| e.to_string()),
                Err(e) => Err(e.clone()),
            },
        })
        .collect();
    let encoders = match &device {
        Ok((dev, _)) => probe_encoders(a.vendor, dev),
        Err(_) => Vec::new(),
    };
    Ok(AdapterReport {
        index: a.index,
        name: a.name.clone(),
        vendor: a.vendor,
        vram_mb: a.dedicated_vram_mb,
        software: a.software,
        device: device.map(|_| ()),
        outputs,
        encoders,
    })
}

fn probe_encoders(vendor: Vendor, device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) -> Vec<EncoderReport> {
    let preset = VideoPreset::P1080_60;
    let frames = HwDevice::wrap(device)
        .and_then(|d| HwFrames::new(&d, SwFormat::Nv12, preset.width, preset.height).map(|f| (d, f)));
    Codec::PREFERENCE
        .iter()
        .filter_map(|&codec| {
            let name = encode::encoder_name(vendor, codec)?;
            let result = match &frames {
                Err(e) => Err(format!("hw frames: {e}")),
                Ok((_dev, frames)) => {
                    let s = EncoderSettings {
                        vendor,
                        codec,
                        preset,
                        max_bitrate_mbps: preset.max_bitrate_mbps(codec),
                        quality: 23,
                    };
                    Encoder::open(&s, frames)
                        .map(|e| EncoderOk {
                            extradata_len: e.extradata_len(),
                            unused_options: e.unused_options.clone(),
                        })
                        .map_err(|e| e.to_string())
                }
            };
            Some(EncoderReport { codec, name, result })
        })
        .collect()
}

fn os_build() -> Option<u32> {
    use windows::{
        Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
        core::w,
    };
    let mut buf = [0u16; 16];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buf/len describe a valid writable buffer; key and value are static strings.
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if ok.is_err() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).parse().ok()
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.os_build {
            Some(b) => writeln!(f, "Windows build {b}")?,
            None => writeln!(f, "Windows build unknown")?,
        }
        let w = &self.wgc;
        writeln!(f, "\nCapture backends")?;
        writeln!(
            f,
            "  WGC: supported {}, cursor toggle {}, border toggle {}",
            yes(w.supported),
            yes(w.cursor_toggle),
            yes(w.border_toggle)
        )?;
        if let Some(access) = &w.borderless_access {
            writeln!(f, "       borderless access: {access}")?;
        }
        let default = if w.supported && w.border_toggle {
            "WGC"
        } else {
            "DDA (WGC missing or border not removable)"
        };
        writeln!(f, "  default backend: {default}")?;
        for a in &self.adapters {
            writeln!(
                f,
                "\nAdapter {}: {} ({:?}, {} MB VRAM{})",
                a.index,
                a.name,
                a.vendor,
                a.vram_mb,
                if a.software { ", software" } else { "" }
            )?;
            if let Err(e) = &a.device {
                writeln!(f, "  D3D11 device: {e}")?;
            }
            for o in &a.outputs {
                let dda = match &o.dda {
                    Ok(()) => "ok".to_string(),
                    Err(e) => e.clone(),
                };
                writeln!(
                    f,
                    "  output {} {}x{} @ {} Hz{} | DDA: {dda}",
                    o.name,
                    o.width,
                    o.height,
                    o.refresh_hz,
                    if o.attached { "" } else { " (detached)" }
                )?;
            }
            if a.outputs.is_empty() {
                writeln!(f, "  no outputs")?;
            }
            for e in &a.encoders {
                match &e.result {
                    Ok(ok) => {
                        write!(
                            f,
                            "  encoder {:<12} opens (1080p60 NV12 D3D11, extradata {} B)",
                            e.name, ok.extradata_len
                        )?;
                        if !ok.unused_options.is_empty() {
                            write!(f, ", ignored options: {}", ok.unused_options.join(" "))?;
                        }
                        writeln!(f)?;
                    }
                    Err(err) => writeln!(f, "  encoder {:<12} FAILED: {err}", e.name)?,
                }
            }
        }
        Ok(())
    }
}
