//! Save feedback (plan §6.3): a short chime and a Windows toast.

use std::sync::OnceLock;

use suicap_core::config::Settings;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use windows::{
    Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
    core::PCWSTR,
};

/// A two-note chime (A5 then E6, 70 ms each with a soft decay) as a 16-bit mono WAV.
fn chime() -> &'static [u8] {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    WAV.get_or_init(|| {
        const RATE: u32 = 44_100;
        let note = |hz: f64, n: usize| {
            (0..n).map(move |i| {
                let t = i as f64 / f64::from(RATE);
                let env = (1.0 - i as f64 / n as f64).powi(2) * (i as f64 / 200.0).min(1.0);
                (env * 0.25 * (std::f64::consts::TAU * hz * t).sin() * f64::from(i16::MAX)) as i16
            })
        };
        let len = (RATE / 1000 * 70) as usize;
        let samples: Vec<i16> = note(880.0, len).chain(note(1318.5, len * 2)).collect();
        let data_len = (samples.len() * 2) as u32;
        let mut w = Vec::with_capacity(44 + data_len as usize);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&1u16.to_le_bytes()); // mono
        w.extend_from_slice(&RATE.to_le_bytes());
        w.extend_from_slice(&(RATE * 2).to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        w
    })
}

pub fn play_chime() {
    let wav = chime();
    // SAFETY: SND_MEMORY reads the WAV from our pointer; the buffer is 'static, so it
    // outlives the asynchronous playback.
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(wav.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

pub fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

pub fn clip_saved(app: &AppHandle, settings: &Settings, name: &str, secs: f64) {
    if settings.save_sound {
        play_chime();
    }
    if settings.save_toast {
        notify(app, &format!("Clip saved ({secs:.0} s)"), name);
    }
}
