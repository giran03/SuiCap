//! Capture: D3D11 device creation, adapter/output discovery and backend support
//! checks. The WGC/DDA frame sources themselves land in P1 (plan §4.1).

use windows::{
    Foundation::Metadata::ApiInformation,
    Graphics::Capture::{GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureSession},
    Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus,
    Win32::{
        Foundation::{E_ACCESSDENIED, HMODULE},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D10::ID3D10Multithread,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
            },
            Dxgi::{
                CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
                DXGI_ERROR_NOT_FOUND, DXGI_ERROR_UNSUPPORTED, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
                IDXGIOutput1,
            },
            Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW},
        },
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
    },
    core::{HSTRING, Interface, PCWSTR},
};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// UAC prompt, mode switch, window closed: recreate the source only (plan §7).
    #[error("capture access lost")]
    AccessLost,
    /// Device removed/reset: rebuild device + encoder and flush the buffer.
    #[error("graphics device lost: {0}")]
    DeviceLost(windows::core::Error),
    #[error("capture backend unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Win(#[from] windows::core::Error),
}

impl CaptureError {
    pub fn is_recoverable(&self) -> bool {
        matches!(self, CaptureError::AccessLost | CaptureError::DeviceLost(_))
    }
}

type Result<T> = std::result::Result<T, CaptureError>;

/// Initialise COM (MTA) on the current thread. Call once on every worker thread that
/// touches DXGI/WGC/WASAPI (plan §7). `S_FALSE` (already initialised) is fine.
pub fn com_init() {
    // SAFETY: plain COM init; no pointers. An RPC_E_CHANGED_MODE result just means the
    // thread already chose STA, which we report via later API failures instead.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Register the current thread with MMCSS under `task` (e.g. "Capture", "Pro Audio") so
/// the scheduler favours it; failure only costs priority, so it is ignored.
pub fn mmcss(task: &str) {
    let name = windows::core::HSTRING::from(task);
    let mut index = 0u32;
    // SAFETY: registers this thread; the handle is released when the thread exits.
    let _ = unsafe { windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(&name, &mut index) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Other(u32),
}

impl Vendor {
    pub fn from_id(id: u32) -> Self {
        match id {
            0x10DE => Vendor::Nvidia,
            0x1002 | 0x1022 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            other => Vendor::Other(other),
        }
    }
}

pub struct Adapter {
    pub index: u32,
    pub name: String,
    pub vendor: Vendor,
    pub dedicated_vram_mb: u64,
    pub software: bool,
    /// Locally unique adapter id: the same GPU across enumerations.
    pub luid: (u32, i32),
    pub raw: IDXGIAdapter1,
}

pub struct Output {
    pub device_name: String,
    pub width: i32,
    pub height: i32,
    pub refresh_hz: u32,
    pub attached: bool,
    /// Desktop origin; the primary monitor sits at (0, 0).
    pub left: i32,
    pub top: i32,
    pub monitor: windows::Win32::Graphics::Gdi::HMONITOR,
    pub raw: IDXGIOutput,
}

impl Output {
    pub fn is_primary(&self) -> bool {
        self.attached && self.left == 0 && self.top == 0
    }
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

pub fn adapters() -> Result<Vec<Adapter>> {
    // SAFETY: COM factory creation and enumeration; all out-params are owned wrappers.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut list = Vec::new();
    for index in 0.. {
        let raw = match unsafe { factory.EnumAdapters1(index) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        // SAFETY: GetDesc1 fills a plain struct.
        let desc = unsafe { raw.GetDesc1()? };
        list.push(Adapter {
            index,
            name: wide_to_string(&desc.Description),
            vendor: Vendor::from_id(desc.VendorId),
            dedicated_vram_mb: (desc.DedicatedVideoMemory / (1024 * 1024)) as u64,
            software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            luid: (desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart),
            raw,
        });
    }
    Ok(list)
}

pub fn outputs(adapter: &Adapter) -> Result<Vec<Output>> {
    let mut list = Vec::new();
    for i in 0.. {
        // SAFETY: COM enumeration; owned wrapper returned.
        let raw = match unsafe { adapter.raw.EnumOutputs(i) } {
            Ok(o) => o,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        // SAFETY: GetDesc fills a plain struct.
        let desc = unsafe { raw.GetDesc()? };
        let r = desc.DesktopCoordinates;
        list.push(Output {
            device_name: wide_to_string(&desc.DeviceName),
            width: r.right - r.left,
            height: r.bottom - r.top,
            refresh_hz: refresh_rate(&desc.DeviceName),
            attached: desc.AttachedToDesktop.as_bool(),
            left: r.left,
            top: r.top,
            monitor: desc.Monitor,
            raw,
        });
    }
    Ok(list)
}

/// Current refresh rate of a GDI display (e.g. `\\.\DISPLAY1`); 0 if unknown.
fn refresh_rate(device_name: &[u16; 32]) -> u32 {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: device_name is NUL-terminated by DXGI; mode is a valid, sized DEVMODEW.
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(device_name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
    if ok.as_bool() { mode.dmDisplayFrequency } else { 0 }
}

/// D3D11 device on the adapter that owns the captured output (plan §4.1), shared with
/// FFmpeg, hence multithread-protected.
pub fn create_device(adapter: &Adapter) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut context = None;
    let dxgi: IDXGIAdapter = adapter.raw.cast()?;
    // SAFETY: out-params point to locals that live past the call.
    unsafe {
        D3D11CreateDevice(
            &dxgi,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let (device, context) = device
        .zip(context)
        .ok_or_else(|| CaptureError::Unavailable("D3D11CreateDevice returned no device".into()))?;
    let mt: ID3D10Multithread = device.cast()?;
    // SAFETY: simple COM call on a live interface.
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok((device, context))
}

/// Windows Graphics Capture support, probed at runtime (plan §4.1, §7).
#[derive(Debug, Clone)]
pub struct WgcSupport {
    pub supported: bool,
    pub cursor_toggle: bool,
    /// `IsBorderRequired` property exists on this OS build.
    pub border_toggle: bool,
    /// Result of asking for borderless capture access, if the API exists.
    pub borderless_access: Option<String>,
}

pub fn wgc_support() -> WgcSupport {
    let class = HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession");
    let has = |prop: &str| ApiInformation::IsPropertyPresent(&class, &HSTRING::from(prop)).unwrap_or(false);
    let supported = GraphicsCaptureSession::IsSupported().unwrap_or(false);
    let border_toggle = has("IsBorderRequired");
    let borderless_access = border_toggle.then(|| {
        match GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.join())
        {
            Ok(s) if s == AppCapabilityAccessStatus::Allowed => "allowed".to_string(),
            Ok(s) => format!("not allowed ({})", s.0),
            Err(e) => format!("error: {}", e.message()),
        }
    });
    WgcSupport {
        supported,
        cursor_toggle: has("IsCursorCaptureEnabled"),
        border_toggle,
        borderless_access,
    }
}

/// Try to start Desktop Duplication on `output` with `device`; dropped immediately.
pub fn dda_check(output: &Output, device: &ID3D11Device) -> Result<()> {
    let out1: IDXGIOutput1 = output.raw.cast()?;
    // SAFETY: COM call with live interfaces; the duplication is released on drop.
    match unsafe { out1.DuplicateOutput(device) } {
        Ok(_dup) => Ok(()),
        Err(e) if e.code() == E_ACCESSDENIED => {
            Err(CaptureError::Unavailable("access denied (secure desktop?)".into()))
        }
        Err(e) if e.code() == DXGI_ERROR_UNSUPPORTED => Err(CaptureError::Unavailable(
            "unsupported on this adapter (hybrid GPU?)".into(),
        )),
        Err(e) if e.code() == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => {
            Err(CaptureError::Unavailable("too many duplication clients".into()))
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Clock and pacing (plan §2: one clock, i64 microseconds on the QPC timeline).

use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::{
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Threading::{
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
            TIMER_ALL_ACCESS, WaitForSingleObject,
        },
    },
};

/// Current QPC time in microseconds.
pub fn qpc_now_us() -> i64 {
    let (mut c, mut f) = (0i64, 0i64);
    // SAFETY: both out-params are valid locals; these calls cannot fail on XP+.
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (i128::from(c) * 1_000_000 / i128::from(f.max(1))) as i64
}

/// Fixed-rate ticker on absolute deadlines (no drift), using a high-resolution
/// waitable timer (plan §4.1).
pub struct Pacer {
    timer: HANDLE,
    start_us: i64,
    fps: i64,
    next: i64,
}

/// One tick: its index and its deadline, which is the CFR PTS of that frame.
#[derive(Debug, Clone, Copy)]
pub struct Tick {
    pub index: i64,
    pub pts_us: i64,
    /// Ticks skipped because we woke up too late (count them as dropped frames).
    pub missed: i64,
}

impl Pacer {
    pub fn new(fps: u32, start_us: i64) -> Result<Self> {
        // SAFETY: creates an owned handle closed in Drop.
        let timer =
            unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)? };
        Ok(Self {
            timer,
            start_us,
            fps: i64::from(fps.max(1)),
            next: 0,
        })
    }

    pub fn deadline_us(&self, index: i64) -> i64 {
        self.start_us + index * 1_000_000 / self.fps
    }

    /// Block until the next deadline. If already past it, skip to the latest due tick.
    pub fn wait(&mut self) -> Result<Tick> {
        let now = qpc_now_us();
        let mut index = self.next;
        let mut missed = 0;
        while self.deadline_us(index + 1) <= now {
            index += 1;
            missed += 1;
        }
        let due = self.deadline_us(index);
        if due > now {
            let relative_100ns = -((due - now) * 10);
            // SAFETY: valid timer handle; due time is a local.
            unsafe {
                SetWaitableTimer(self.timer, &relative_100ns, 0, None, None, false)?;
                if WaitForSingleObject(self.timer, INFINITE) != WAIT_OBJECT_0 {
                    return Err(windows::core::Error::from_thread().into());
                }
            }
        }
        self.next = index + 1;
        Ok(Tick {
            index,
            pts_us: due,
            missed,
        })
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.timer);
        }
    }
}

// ---------------------------------------------------------------------------
// Frame sources (plan §4.1).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Graphics::{Direct3D11::ID3D11Texture2D, Dxgi::IDXGIDevice},
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
};

/// A captured GPU frame. Holding it keeps the source buffer checked out, so drop it
/// right after the blt.
pub struct Frame {
    pub tex: ID3D11Texture2D,
    pub qpc_us: i64,
    /// Valid content size (may be smaller than the texture during a resize).
    pub width: u32,
    pub height: u32,
    wgc: Option<Direct3D11CaptureFrame>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        if let Some(f) = self.wgc.take() {
            let _ = f.Close();
        }
    }
}

pub trait CaptureSource {
    /// Latest frame newer than the previous call, or `None` if nothing changed.
    /// `timeout_ms` bounds the wait for backends that block (DDA); WGC never blocks.
    fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>>;
}

/// A D3D11 device handed to a helper thread (WGC source rebuilds).
pub struct SendDevice(pub ID3D11Device);

// SAFETY: the device is created with ID3D10Multithread protection on (create_device),
// and D3D11 devices are free-threaded; only the immediate context is thread-bound, and
// it never leaves the video thread.
unsafe impl Send for SendDevice {}

// SAFETY: every member is either a free-threaded WinRT object (frame pool created with
// CreateFreeThreaded, session, item, WinRT device wrapper) or an Arc<AtomicBool>; the
// source is used by one thread at a time (built or dropped on a helper, then polled on
// the video thread).
unsafe impl Send for WgcSource {}

/// Windows Graphics Capture on a monitor, free-threaded pool (no dispatcher queue).
pub struct WgcSource {
    d3d: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pool_size: SizeInt32,
    closed: Arc<AtomicBool>,
    /// The yellow capture border was turned off (needs borderless access).
    pub border_removed: bool,
    _item: GraphicsCaptureItem,
}

const WGC_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
const WGC_BUFFERS: i32 = 2;

impl WgcSource {
    pub fn for_monitor(device: &ID3D11Device, output: &Output, cursor: bool) -> Result<Self> {
        let dxgi: IDXGIDevice = device.cast()?;
        // SAFETY: wraps our live DXGI device in a WinRT IDirect3DDevice.
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: HMONITOR comes from DXGI for an attached output.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(output.monitor)? };
        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        item.Closed(&TypedEventHandler::new(move |_, _| {
            flag.store(true, Ordering::Release);
            Ok(())
        }))?;
        let pool_size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, WGC_FORMAT, WGC_BUFFERS, pool_size)?;
        let session = pool.CreateCaptureSession(&item)?;
        // Cached: the access request is slow and must not stall a source recreate.
        static SUPPORT: std::sync::OnceLock<WgcSupport> = std::sync::OnceLock::new();
        let support = SUPPORT.get_or_init(wgc_support);
        // Fails harmlessly if borderless access was not granted; the border then shows.
        let border_removed = support.border_toggle
            && support.borderless_access.as_deref() == Some("allowed")
            && session.SetIsBorderRequired(false).is_ok();
        if support.cursor_toggle {
            session.SetIsCursorCaptureEnabled(cursor)?;
        }
        session.StartCapture()?;
        Ok(Self {
            d3d,
            pool,
            session,
            pool_size,
            closed,
            border_removed,
            _item: item,
        })
    }
}

impl CaptureSource for WgcSource {
    fn next_frame(&mut self, _timeout_ms: u32) -> Result<Option<Frame>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(CaptureError::AccessLost);
        }
        // Drain to the newest frame; older ones go straight back to the pool.
        let mut latest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if let Some(old) = latest.replace(f) {
                let _ = old.Close();
            }
        }
        let Some(f) = latest else { return Ok(None) };
        let content = f.ContentSize()?;
        if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
            // Source resized: next frames come at the new size; this one stays valid.
            self.pool.Recreate(&self.d3d, WGC_FORMAT, WGC_BUFFERS, content)?;
            self.pool_size = content;
        }
        let access: IDirect3DDxgiInterfaceAccess = f.Surface()?.cast()?;
        // SAFETY: the surface is a D3D11 texture on our device.
        let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let qpc_us = f.SystemRelativeTime()?.Duration / 10;
        Ok(Some(Frame {
            tex,
            qpc_us,
            width: content.Width.max(1) as u32,
            height: content.Height.max(1) as u32,
            wgc: Some(f),
        }))
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

// ---------------------------------------------------------------------------
// Process usage, for the §1 budget (CPU < 1 %, private RAM < 40 MB + buffer).

#[derive(Debug, Clone, Copy)]
pub struct ProcessUsage {
    /// Kernel + user CPU time of this process so far.
    pub cpu_us: i64,
    /// Private committed memory (Task Manager "Commit size"); includes GPU-driver
    /// reservations that are not necessarily resident.
    pub private_bytes: u64,
    /// Private working set (Task Manager "Memory" column): resident and not shared.
    pub private_ws_bytes: u64,
}

pub fn process_usage() -> ProcessUsage {
    use windows::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2},
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };
    let ft = |f: FILETIME| ((i64::from(f.dwHighDateTime) << 32) | i64::from(f.dwLowDateTime)) / 10;
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let mut mem = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    // SAFETY: pseudo-handle of this process; out-params are sized locals.
    unsafe {
        let p = GetCurrentProcess();
        let _ = GetProcessTimes(p, &mut c, &mut e, &mut k, &mut u);
        let _ = GetProcessMemoryInfo(p, (&raw mut mem).cast::<PROCESS_MEMORY_COUNTERS>(), mem.cb);
    }
    ProcessUsage {
        cpu_us: ft(k) + ft(u),
        private_bytes: mem.PrivateUsage as u64,
        private_ws_bytes: mem.PrivateWorkingSetSize as u64,
    }
}
