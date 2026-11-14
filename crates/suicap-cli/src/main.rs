//! Dev harness: `probe | record <s> | replay <buffer_s> <save_s>` (plan §3).

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    slice::Iter,
};

use suicap_core::{
    audio::{AudioOptions, AudioStats, Target, TrackKind},
    config::Codec,
    encode::AudioCodec,
    pipeline::{self, RecordOptions, ReplayOptions, VideoOptions, VideoStats},
};

const USAGE: &str = "usage:
  suicap-cli probe
  suicap-cli record <secs> [out.mkv|out.mp4] [video options]
  suicap-cli replay <buffer_secs> <save_secs> [out.mp4|out.mkv] [video options]
  suicap-cli session <minutes> [clips_dir] [--buffer S] [--save-every M] [options]
video options: --codec av1|hevc|h264  --fps N  --display N  --no-cursor  --window EXE [--title TEXT]  --auto-window
audio options: --no-audio  --no-mic  --mute game|voice|media  --mute-after S mix|mic|game|voice|media  --game <exe|pid>  --voice <exe|pid>  --media <exe|pid>
  (--game/--voice/--media repeat and replace that track's defaults)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("probe") if args.len() == 1 => probe(),
        Some("record") => record(&args[1..]),
        Some("replay") => replay(&args[1..]),
        Some("session") => session(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn probe() -> Result<(), String> {
    let report = suicap_core::probe::run().map_err(|e| format!("probe failed: {e}"))?;
    print!("{report}");
    Ok(())
}

fn positive(arg: Option<&String>) -> Result<u32, String> {
    arg.and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .ok_or_else(|| USAGE.to_string())
}

#[derive(Default)]
struct AudioArgs {
    off: bool,
    no_mic: bool,
    /// Tracks switched off with `--mute`: their apps are not recorded at all.
    mute: Vec<String>,
    /// `--mute-after S track`: mute a track S seconds into the run, live.
    mute_after: Option<(u64, TrackKind)>,
    game: Option<Vec<Target>>,
    voice: Option<Vec<Target>>,
    media: Option<Vec<Target>>,
}

impl AudioArgs {
    /// Plan §5 defaults, with any track the user named replaced; codec from extension.
    fn build(self, out: &Path) -> Option<AudioOptions> {
        if self.off {
            return None;
        }
        let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let mut o = AudioOptions::defaults(AudioCodec::for_extension(ext));
        o.mic = !self.no_mic;
        for m in &self.mute {
            match m.as_str() {
                "game" => o.track_game = false,
                "voice" => o.track_voice = false,
                _ => o.track_media = false,
            }
            o.muting = true;
        }
        if let Some(t) = self.game {
            o.game = t;
        }
        if let Some(t) = self.voice {
            o.voice = t;
        }
        if let Some(t) = self.media {
            o.media = t;
        }
        Some(o)
    }
}

fn target(arg: Option<&String>) -> Result<Target, String> {
    let a = arg.ok_or_else(|| USAGE.to_string())?;
    Ok(a.parse().map(Target::Pid).unwrap_or_else(|_| Target::Exe(a.clone())))
}

/// Parses `[out] [video options] [audio options]`; returns the output path if given.
fn video_options(
    it: &mut Iter<'_, String>,
    video: &mut VideoOptions,
    audio: &mut AudioArgs,
) -> Result<Option<PathBuf>, String> {
    let mut out = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-audio" => audio.off = true,
            "--no-mic" => audio.no_mic = true,
            "--mute-after" => {
                let secs = positive(it.next())?;
                let kind = match it.next().map(String::as_str) {
                    Some("mix") => TrackKind::Mix,
                    Some("mic") => TrackKind::Mic,
                    Some("game") => TrackKind::Game,
                    Some("voice") => TrackKind::Voice,
                    Some("media") => TrackKind::Media,
                    _ => return Err(USAGE.to_string()),
                };
                audio.mute_after = Some((u64::from(secs), kind));
            }
            "--mute" => match it.next().map(String::as_str) {
                Some(t @ ("game" | "voice" | "media")) => audio.mute.push(t.to_string()),
                _ => return Err(USAGE.to_string()),
            },
            "--game" => audio.game.get_or_insert_with(Vec::new).push(target(it.next())?),
            "--voice" => audio.voice.get_or_insert_with(Vec::new).push(target(it.next())?),
            "--media" => audio.media.get_or_insert_with(Vec::new).push(target(it.next())?),
            "--codec" => {
                video.codec = Some(match it.next().map(String::as_str) {
                    Some("av1") => Codec::Av1,
                    Some("hevc") => Codec::Hevc,
                    Some("h264") => Codec::H264,
                    _ => return Err(USAGE.to_string()),
                })
            }
            "--fps" => video.preset.fps = positive(it.next())?,
            "--display" => video.display = Some(positive(it.next())?),
            "--window" => {
                video.source.kind = suicap_core::config::SourceKind::App;
                video.source.exe = it.next().ok_or("--window needs a program name like game.exe")?.clone();
            }
            "--title" => video.source.title = it.next().ok_or("--title needs text")?.clone(),
            "--auto-window" => video.source.kind = suicap_core::config::SourceKind::Auto,
            "--no-cursor" => video.cursor = false,
            path if !path.starts_with("--") && out.is_none() => out = Some(PathBuf::from(path)),
            _ => return Err(USAGE.to_string()),
        }
    }
    Ok(out)
}

fn cwd() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| e.to_string())
}

fn print_video(s: &VideoStats, secs: f64) {
    println!("source   {}", s.source);
    println!("encoder  {} {}x{} @ {} fps (CFR)", s.encoder, s.width, s.height, s.fps);
    println!(
        "frames   {} encoded ({} fresh, {} repeats), {} dropped",
        s.frames,
        s.fresh,
        s.frames.saturating_sub(s.fresh),
        s.dropped
    );
    println!(
        "packets  {} ({:.1} MB, {:.1} Mbps)",
        s.packets,
        s.bytes as f64 / 1e6,
        s.bytes as f64 * 8.0 / 1e6 / secs
    );
}

fn record(args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    let secs = positive(it.next())?;
    let mut video = VideoOptions::default();
    let mut audio = AudioArgs::default();
    let out = match video_options(&mut it, &mut video, &mut audio)? {
        Some(p) => p,
        None => pipeline::default_output_path(&cwd()?, "mkv"),
    };
    let mute_after = audio.mute_after;
    let audio = audio.build(&out);
    if let (Some((after, kind)), Some(a)) = (mute_after, audio.as_ref()) {
        // Flip the live switch part-way through, as the settings page does.
        let live = a.live.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(after));
            live.set(kind, false);
            eprintln!("muted {} at {after} s", kind.title());
        });
    }
    eprintln!("recording {secs} s to {} ...", out.display());
    let s = pipeline::record(&RecordOptions {
        secs,
        out,
        video,
        audio,
    })
    .map_err(|e| format!("record failed: {e}"))?;
    println!("file     {}", s.path.display());
    print_video(&s.video, f64::from(secs));
    print_audio(s.audio.as_ref());
    Ok(())
}

fn replay(args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    let buffer_secs = positive(it.next())?;
    let save_secs = positive(it.next())?;
    let mut video = VideoOptions::default();
    let mut audio = AudioArgs::default();
    let out = match video_options(&mut it, &mut video, &mut audio)? {
        Some(p) => p,
        None => pipeline::default_output_path(&cwd()?, "mp4"),
    };
    let audio = audio.build(&out);
    eprintln!(
        "buffer armed for {buffer_secs} s, then saving the last {save_secs} s to {} ...",
        out.display()
    );
    let s = pipeline::replay(&ReplayOptions {
        buffer_secs,
        save_secs,
        out,
        video,
        audio,
    })
    .map_err(|e| format!("replay failed: {e}"))?;
    let c = &s.clip;
    println!("file     {}", c.path.display());
    println!(
        "clip     {} packets, first pts {} us, last pts {:.3} s (requested {save_secs} s)",
        c.packets,
        c.first_pts_us,
        c.last_pts_us as f64 / 1e6
    );
    print_video(&s.video, s.armed_secs);
    print_audio(s.audio.as_ref());
    println!(
        "buffer   {:.1} MB over {:.2} s ({} packets); max-bitrate estimate {} MB",
        s.buffer.bytes as f64 / 1e6,
        s.buffer.span_us as f64 / 1e6,
        s.buffer.packets,
        s.buffer_estimate_mb
    );
    println!(
        "save     capture kept running for {} ticks during the save",
        s.ticks_during_save
    );
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    println!(
        "budget   CPU {:.2} % of one core = {:.3} % total ({cores} logical cores) over {:.1} s armed",
        s.armed_cpu_pct_one_core,
        s.armed_cpu_pct_one_core / cores,
        s.armed_secs
    );
    let buffer = s.buffer.bytes as u64;
    println!(
        "budget   private working set {:.1} MB ({:.1} MB excl. buffer); commit {:.1} MB ({:.1} MB excl. buffer)",
        s.private_ws_armed as f64 / 1e6,
        s.private_ws_armed.saturating_sub(buffer) as f64 / 1e6,
        s.private_bytes_armed as f64 / 1e6,
        s.private_bytes_armed.saturating_sub(buffer) as f64 / 1e6
    );
    Ok(())
}

fn print_audio(a: Option<&AudioStats>) {
    let Some(a) = a else {
        println!("audio    off");
        return;
    };
    let srcs: Vec<String> = a
        .sources
        .iter()
        .map(|(t, pid)| format!("{} pid {pid}", t.title()))
        .collect();
    println!(
        "audio    tracks [{}], process loopback {}, mic {}",
        a.tracks.join(", "),
        if a.process_loopback {
            "yes"
        } else {
            "no (endpoint fallback)"
        },
        if a.mic { "yes" } else { "no" }
    );
    println!(
        "audio    loopback sources: {}",
        if srcs.is_empty() {
            "none".into()
        } else {
            srcs.join(", ")
        }
    );
    println!(
        "audio    {} frames per track, {} resyncs, {} late samples dropped",
        a.frames_per_track, a.resyncs, a.late_samples
    );
}

/// `suicap-cli session <minutes> [clips_dir] [--buffer S] [--save-every M] [options]`:
/// the app's armed `Session` with events on the console, for resilience tests and the
/// soak (plan P5). Saves `--save-every` minutes and the last 60 s at the end.
fn session(args: &[String]) -> Result<(), String> {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use suicap_core::{
        capture,
        config::Container,
        pipeline::{Session, SessionEvent, SessionOptions},
    };

    let mut it = args.iter();
    let minutes = positive(it.next())?;
    let mut rest = Vec::new();
    let (mut buffer_secs, mut save_every) = (60u32, 0u32);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--buffer" => buffer_secs = positive(it.next())?,
            "--save-every" => save_every = positive(it.next())?,
            _ => rest.push(a.clone()),
        }
    }
    let mut video = VideoOptions::default();
    let mut audio = AudioArgs::default();
    let dir = match video_options(&mut rest.iter(), &mut video, &mut audio)? {
        Some(p) => p,
        None => cwd()?,
    };
    let audio = audio.build(Path::new("x.mp4"));

    let start = std::time::Instant::now();
    let buffered = Arc::new(AtomicU64::new(0));
    let seen = buffered.clone();
    let (saved_tx, saved_rx) = std::sync::mpsc::channel::<()>();
    let mem = move |buffered: u64| {
        let u = capture::process_usage();
        format!(
            "private WS {:.1} MB ({:.1} excl. buffer), commit {:.1} MB, cpu {:.1} s",
            u.private_ws_bytes as f64 / 1e6,
            u.private_ws_bytes.saturating_sub(buffered) as f64 / 1e6,
            u.private_bytes as f64 / 1e6,
            u.cpu_us as f64 / 1e6
        )
    };
    let ticks = AtomicU64::new(0);
    let on_event = move |e: SessionEvent| {
        let t = start.elapsed().as_secs_f64();
        match e {
            SessionEvent::Armed(v) => {
                println!(
                    "[{t:8.1}] armed: {} {}x{}@{} ({})",
                    v.encoder, v.width, v.height, v.fps, v.source
                );
                for n in &v.notes {
                    println!("[{t:8.1}]   note: {n}");
                }
            }
            SessionEvent::Status(s) => {
                seen.store(s.buffered_bytes, Ordering::Relaxed);
                if ticks.fetch_add(1, Ordering::Relaxed).is_multiple_of(60) {
                    println!(
                        "[{t:8.1}] buffer {:.1} s / {:.1} MB, frames {}, dropped {} | {}",
                        s.buffered_secs,
                        s.buffered_bytes as f64 / 1e6,
                        s.frames,
                        s.dropped,
                        mem(s.buffered_bytes)
                    );
                }
            }
            SessionEvent::ClipSaved(c) => {
                println!(
                    "[{t:8.1}] saved {} ({} packets, {:.1} s)",
                    c.path.display(),
                    c.packets,
                    (c.last_pts_us - c.first_pts_us) as f64 / 1e6
                );
                let _ = saved_tx.send(());
            }
            SessionEvent::SaveFailed(m) => println!("[{t:8.1}] save failed: {m}"),
            SessionEvent::RecordingStarted => println!("[{t:8.1}] recording started"),
            SessionEvent::RecordingSaved(c) => println!(
                "[{t:8.1}] recording saved {} ({} packets, {:.1} s)",
                c.path.display(),
                c.packets,
                (c.last_pts_us - c.first_pts_us) as f64 / 1e6
            ),
            SessionEvent::RecordingFailed(m) => println!("[{t:8.1}] recording failed: {m}"),
            SessionEvent::ScreenshotSaved(p) => println!("[{t:8.1}] screenshot {}", p.display()),
            SessionEvent::ScreenshotFailed(m) => println!("[{t:8.1}] screenshot failed: {m}"),
            SessionEvent::Recovering(m) => println!("[{t:8.1}] RECOVERING: {m}"),
            SessionEvent::Stopped(err) => println!("[{t:8.1}] stopped: {}", err.as_deref().unwrap_or("ok")),
        }
    };
    let session = Session::start(
        SessionOptions {
            buffer_secs,
            video,
            audio,
            clips_dir: dir,
            container: Container::Mp4,
        },
        on_event,
    )
    .map_err(|e| e.to_string())?;

    let end = start + std::time::Duration::from_secs(u64::from(minutes) * 60);
    let mut next_save = (save_every > 0).then(|| start + std::time::Duration::from_secs(u64::from(save_every) * 60));
    while std::time::Instant::now() < end {
        if !session.is_running() {
            return Err("session stopped early".into());
        }
        if let Some(at) = next_save
            && std::time::Instant::now() >= at
        {
            session.save(Some(30));
            next_save = Some(at + std::time::Duration::from_secs(u64::from(save_every) * 60));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    while saved_rx.try_recv().is_ok() {}
    session.save(Some(60));
    let _ = saved_rx.recv_timeout(std::time::Duration::from_secs(30));
    session.stop();
    println!("final    {}", {
        let u = capture::process_usage();
        format!(
            "private WS {:.1} MB, commit {:.1} MB (session stopped, buffer freed; last buffer {:.1} MB)",
            u.private_ws_bytes as f64 / 1e6,
            u.private_bytes as f64 / 1e6,
            buffered.load(Ordering::Relaxed) as f64 / 1e6
        )
    });
    Ok(())
}
