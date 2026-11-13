//! Fault injection for resilience tests (plan P5). With `SUICAP_FAULT` unset (normal
//! use) every check is a cheap `false`.
//!
//! `SUICAP_FAULT="access_lost:20,device_lost:60"` fires each listed fault once, that
//! many seconds after the first check in the process.

use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// As if WGC closed the capture item (UAC, mode switch, monitor gone).
    AccessLost,
    /// As if the GPU reported DXGI_ERROR_DEVICE_REMOVED.
    DeviceLost,
}

struct Plan {
    start: Instant,
    /// (fault, delay, fired)
    faults: Vec<(Kind, Duration, bool)>,
}

static PLAN: OnceLock<Option<Mutex<Plan>>> = OnceLock::new();

/// Parse `kind:seconds` pairs; unknown entries are ignored.
pub fn parse(spec: &str) -> Vec<(Kind, Duration)> {
    spec.split(',')
        .filter_map(|item| {
            let (kind, secs) = item.trim().split_once(':')?;
            let kind = match kind.trim() {
                "access_lost" => Kind::AccessLost,
                "device_lost" => Kind::DeviceLost,
                _ => return None,
            };
            let secs: f64 = secs.trim().parse().ok().filter(|s: &f64| s.is_finite() && *s >= 0.0)?;
            Some((kind, Duration::from_secs_f64(secs)))
        })
        .collect()
}

/// True once per configured fault of `kind`, when its time has come.
pub fn fire(kind: Kind) -> bool {
    let plan = PLAN.get_or_init(|| {
        let spec = std::env::var("SUICAP_FAULT").ok()?;
        let faults: Vec<_> = parse(&spec).into_iter().map(|(k, d)| (k, d, false)).collect();
        (!faults.is_empty()).then(|| {
            Mutex::new(Plan {
                start: Instant::now(),
                faults,
            })
        })
    });
    let Some(plan) = plan else { return false };
    let mut p = plan.lock().unwrap_or_else(|e| e.into_inner());
    let elapsed = p.start.elapsed();
    for f in p.faults.iter_mut() {
        if f.0 == kind && !f.2 && elapsed >= f.1 {
            f.2 = true;
            eprintln!("[fault] injecting {kind:?} at {:.1} s", elapsed.as_secs_f64());
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_faults_only() {
        let f = parse("access_lost:20, device_lost:1.5,bogus:3,device_lost:-1,access_lost:x");
        assert_eq!(
            f,
            vec![
                (Kind::AccessLost, Duration::from_secs(20)),
                (Kind::DeviceLost, Duration::from_millis(1500)),
            ]
        );
    }
}
