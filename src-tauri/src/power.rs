//! Sleep / resume (plan §7): disarm on suspend, re-arm on resume if it was armed.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicBool, Ordering},
};

use tauri::{AppHandle, Manager};
use windows::Win32::{
    Foundation::HANDLE,
    System::Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification},
    UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND},
};

use crate::state::AppState;

/// Capture was on when the machine went to sleep.
static REARM: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_power(context: *const c_void, kind: u32, _setting: *const c_void) -> u32 {
    // SAFETY: `context` is the AppHandle leaked in `watch`, alive for the whole process.
    let app = unsafe { &*(context as *const AppHandle) };
    match kind {
        PBT_APMSUSPEND => {
            let state = app.state::<AppState>();
            let armed = state.is_armed();
            REARM.store(armed, Ordering::Release);
            if armed {
                // Synchronous: stop before the system sleeps (saves in progress finish).
                state.disarm(app);
            }
        }
        PBT_APMRESUMEAUTOMATIC if REARM.swap(false, Ordering::AcqRel) => {
            let h = app.clone();
            std::thread::spawn(move || h.state::<AppState>().arm(&h));
        }
        _ => {}
    }
    0
}

/// Subscribe to suspend/resume for the life of the process.
pub fn watch(app: &AppHandle) {
    let context = Box::into_raw(Box::new(app.clone())) as *mut c_void;
    let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power),
        Context: context,
    }));
    let mut registration = std::ptr::null_mut();
    // SAFETY: `params` and its context are leaked, so they outlive the registration,
    // which lasts until the process exits.
    let err = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut c_void),
            &mut registration,
        )
    };
    if err.0 != 0 {
        eprintln!("sleep/resume notifications unavailable: error {}", err.0);
    }
}
