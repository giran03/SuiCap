//! Copies the FFmpeg runtime DLLs next to the binaries Cargo produces (including
//! test binaries in `deps/`), so `cargo run` and `cargo test` work without PATH edits.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let Ok(ffmpeg_dir) = env::var("FFMPEG_DIR") else {
        println!("cargo:warning=FFMPEG_DIR is not set; run .scripts\\SuiCap Fetch FFmpeg.bat");
        return;
    };
    let bin = PathBuf::from(ffmpeg_dir).join("bin");
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let Some(profile_dir) = out.ancestors().nth(3) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&bin) else {
        println!("cargo:warning=no FFmpeg DLLs in {}", bin.display());
        return;
    };
    for entry in entries.flatten() {
        let src = entry.path();
        if src.extension().is_none_or(|e| !e.eq_ignore_ascii_case("dll")) {
            continue;
        }
        for dir in [profile_dir.to_path_buf(), profile_dir.join("deps")] {
            let dst = dir.join(entry.file_name());
            let same = fs::metadata(&dst)
                .ok()
                .zip(entry.metadata().ok())
                .is_some_and(|(a, b)| a.len() == b.len());
            if !same {
                let _ = fs::create_dir_all(&dir);
                if let Err(e) = fs::copy(&src, &dst) {
                    println!("cargo:warning=copy {} failed: {e}", dst.display());
                }
            }
        }
    }
}
