use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use libcamera::camera::Camera;
use libcamera::camera_manager::{CameraManager, HotplugEvent};

use crate::LibcameraDeviceInfo;

static MANAGER: OnceLock<SharedManager> = OnceLock::new();
static INIT_GUARD: Mutex<()> = Mutex::new(());
static PROBE_CACHE: OnceLock<Mutex<ProbeCache>> = OnceLock::new();
static PROBE_CACHE_TTL_MS: AtomicU64 = AtomicU64::new(DEFAULT_LIBCAMERA_PROBE_CACHE_MS);
static ACTIVE_CAMERA_USES: AtomicUsize = AtomicUsize::new(0);
static HOTPLUG_SUBSCRIBERS: AtomicUsize = AtomicUsize::new(0);
static STOP_WHEN_IDLE: AtomicBool = AtomicBool::new(true);

/// Default libcamera probe cache time-to-live (milliseconds).
pub const DEFAULT_LIBCAMERA_PROBE_CACHE_MS: u64 = 1_000;

/// Runtime configuration for the shared libcamera manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LibcameraManagerConfig {
    /// How long probe results remain cached. Set to `0` to effectively bypass the cache.
    pub probe_cache_ttl_ms: u64,
    /// Stop the manager whenever nothing needs it: after a probe, and when the last camera use
    /// or hotplug subscription ends. A running manager keeps an IPA process (and its buffers)
    /// alive for every camera it found, whether or not it is capturing.
    pub stop_when_idle: bool,
}

impl Default for LibcameraManagerConfig {
    fn default() -> Self {
        Self {
            probe_cache_ttl_ms: DEFAULT_LIBCAMERA_PROBE_CACHE_MS,
            stop_when_idle: true,
        }
    }
}

#[derive(Default)]
struct ProbeCache {
    last_probe_at: Option<Instant>,
    cached_devices: Vec<LibcameraDeviceInfo>,
}

/// Return the current typed manager configuration.
pub fn manager_config() -> LibcameraManagerConfig {
    LibcameraManagerConfig {
        probe_cache_ttl_ms: PROBE_CACHE_TTL_MS.load(Ordering::Relaxed),
        stop_when_idle: STOP_WHEN_IDLE.load(Ordering::Relaxed),
    }
}

/// Set typed runtime configuration for the shared libcamera manager.
///
/// `STYX_LIBCAMERA_PROBE_CACHE_MS` remains a process-level override for debugging and deployment
/// environments that cannot pass typed configuration.
pub fn set_manager_config(config: LibcameraManagerConfig) {
    PROBE_CACHE_TTL_MS.store(config.probe_cache_ttl_ms, Ordering::Relaxed);
    STOP_WHEN_IDLE.store(config.stop_when_idle, Ordering::Relaxed);
}

/// Stop the manager after a probe if idle-stop is configured and nothing needs it.
pub(crate) fn stop_after_probe() {
    if STOP_WHEN_IDLE.load(Ordering::Relaxed) {
        let _ = try_stop_if_idle();
    }
}

fn probe_cache_ttl() -> Duration {
    let ms = std::env::var("STYX_LIBCAMERA_PROBE_CACHE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(|| PROBE_CACHE_TTL_MS.load(Ordering::Relaxed));
    Duration::from_millis(ms)
}

pub(crate) fn read_probe_cache() -> Option<Vec<LibcameraDeviceInfo>> {
    let cache = PROBE_CACHE.get_or_init(|| Mutex::new(ProbeCache::default()));
    let ttl = probe_cache_ttl();
    let guard = cache.lock().ok()?;
    let last = guard.last_probe_at?;
    if last.elapsed() <= ttl {
        return Some(guard.cached_devices.clone());
    }
    None
}

pub(crate) fn write_probe_cache(devices: &[LibcameraDeviceInfo]) {
    let cache = PROBE_CACHE.get_or_init(|| Mutex::new(ProbeCache::default()));
    if let Ok(mut guard) = cache.lock() {
        guard.last_probe_at = Some(Instant::now());
        guard.cached_devices = devices.to_vec();
    }
}

struct SharedManager {
    /// `None` while stopped. A stopped manager is dropped rather than restarted: a restarted
    /// libcamera manager returns from `start()` before its cameras are registered, while a new
    /// one blocks until enumeration is complete.
    manager: UnsafeCell<Option<CameraManager>>,
    lock: Mutex<()>,
}

// SAFETY: mutable access to the non-thread-safe `CameraManager` is serialized by `lock` and is
// rejected while `ActiveCameraUse` guards exist. Moving the wrapper between threads does not expose
// the inner manager without taking that mutex.
unsafe impl Send for SharedManager {}

// SAFETY: direct mutable manager access goes through `with_manager_mut`, which double-checks that no
// active camera lease exists before touching `UnsafeCell<CameraManager>`. Camera lookup requires an
// `ActiveCameraUse` guard, and manager stop/mutation paths are blocked until those guards drop.
unsafe impl Sync for SharedManager {}

/// Guard that keeps the shared libcamera manager from being stopped while a camera is active.
#[derive(Debug)]
pub struct ActiveCameraUse {
    active: bool,
}

impl Drop for ActiveCameraUse {
    fn drop(&mut self) {
        if self.active {
            ACTIVE_CAMERA_USES.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl ActiveCameraUse {
    fn is_active(&self) -> bool {
        self.active
    }
}

/// Mark a camera lookup/capture session as active.
///
/// Hold this guard for the whole lifetime of any camera or request derived from the shared manager.
pub fn begin_camera_use() -> Result<ActiveCameraUse, String> {
    ACTIVE_CAMERA_USES.fetch_add(1, Ordering::AcqRel);
    if let Err(err) = shared_manager() {
        ACTIVE_CAMERA_USES.fetch_sub(1, Ordering::AcqRel);
        return Err(err);
    }
    Ok(ActiveCameraUse { active: true })
}

fn reject_active_camera_uses() -> Result<(), String> {
    if ACTIVE_CAMERA_USES.load(Ordering::Acquire) == 0 {
        Ok(())
    } else {
        Err("libcamera manager mutation blocked by active camera use".to_string())
    }
}

fn ensure_started(shared: &SharedManager) -> Result<(), String> {
    let _guard = shared.lock.lock().map_err(|e| e.to_string())?;
    // SAFETY: `shared.lock` serializes access to the `UnsafeCell`, and this function only borrows
    // the manager for the duration of the mutex guard.
    let slot = unsafe { &mut *shared.manager.get() };
    started(slot)?;
    Ok(())
}

/// The running manager in `slot`, creating one if it was stopped.
fn started(slot: &mut Option<CameraManager>) -> Result<&mut CameraManager, String> {
    if slot.is_none() {
        reap_ipa_helpers();
        *slot = Some(CameraManager::new().map_err(|e| e.to_string())?);
    }
    let mgr = slot.as_mut().expect("manager just created");
    if !mgr.is_started() {
        mgr.start().map_err(|e| e.to_string())?;
    }
    Ok(mgr)
}

/// Stop the manager in `slot` by dropping it, then reap the IPA helpers it ended.
fn stop(slot: &mut Option<CameraManager>) -> Result<(), String> {
    let Some(mgr) = slot.as_mut() else {
        return Ok(());
    };
    // Refuses while camera handles are alive.
    mgr.try_stop().map_err(|e| e.to_string())?;
    *slot = None;
    for _ in 0..10 {
        if reap_ipa_helpers() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        } else {
            break;
        }
    }
    Ok(())
}

/// Reap exited libcamera IPA helper processes (`*_ipa_proxy`) of this process. libcamera ends
/// them when its manager stops but does not wait for them, so each stop would otherwise leave a
/// zombie. Only zombie children whose name contains `ipa` are touched.
fn reap_ipa_helpers() -> usize {
    let own_pid = std::process::id() as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut reaped = 0;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "<pid> (<comm>) <state> <ppid> ..."; comm may contain spaces and parentheses.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let comm = &stat[open + 1..close];
        let mut rest = stat[close + 1..].split_whitespace();
        let (Some(state), Some(ppid)) = (rest.next(), rest.next()) else {
            continue;
        };
        if state == "Z" && ppid.parse::<i32>() == Ok(own_pid) && comm.contains("ipa") {
            // SAFETY: waitpid on our own zombie child with WNOHANG does not block.
            if unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == pid {
                reaped += 1;
            }
        }
    }
    reaped
}

fn shared_manager() -> Result<&'static SharedManager, String> {
    if let Some(shared) = MANAGER.get() {
        ensure_started(shared)?;
        return Ok(shared);
    }

    let _guard = INIT_GUARD.lock().map_err(|e| e.to_string())?;
    if let Some(shared) = MANAGER.get() {
        ensure_started(shared)?;
        return Ok(shared);
    }

    let mgr = CameraManager::new().map_err(|e| e.to_string())?;
    register_exit_stop();
    MANAGER
        .set(SharedManager {
            manager: UnsafeCell::new(Some(mgr)),
            lock: Mutex::new(()),
        })
        .map_err(|_| "failed to set libcamera manager".to_string())?;
    if let Some(shared) = MANAGER.get() {
        ensure_started(shared)?;
    }
    let shared = MANAGER
        .get()
        .ok_or_else(|| "failed to init libcamera manager".to_string())?;
    ensure_started(shared)?;
    Ok(shared)
}

/// Run a closure with exclusive mutable access to the shared `CameraManager`.
///
/// This is required for lifecycle/probe operations. It refuses mutable access while any
/// `ActiveCameraUse` guard is alive because camera/request values returned from the manager may
/// outlive the manager mutex guard.
/// Stop the shared manager when the process exits normally.
///
/// The manager lives in a static that is never dropped, and libcamera only terminates its
/// out-of-process IPA helpers (e.g. `raspberrypi_ipa_proxy`) when the manager stops. Without this
/// every process that used libcamera leaves an orphaned helper behind after `main` returns.
fn register_exit_stop() {
    extern "C" fn stop_manager_at_exit() {
        let Some(shared) = MANAGER.get() else {
            return;
        };
        // A capture thread may still own a camera; stopping the manager under it is unsafe.
        if ACTIVE_CAMERA_USES.load(Ordering::Acquire) != 0 {
            return;
        }
        // Never block process exit on a lock held by another thread.
        let Ok(_guard) = shared.lock.try_lock() else {
            return;
        };
        // SAFETY: the manager lock is held and no camera use is active.
        let slot = unsafe { &mut *shared.manager.get() };
        let _ = stop(slot);
    }
    // SAFETY: registering a plain `extern "C"` function with no captured state.
    unsafe {
        libc::atexit(stop_manager_at_exit);
    }
}

pub(crate) fn with_manager_mut<R>(f: impl FnOnce(&mut CameraManager) -> R) -> Result<R, String> {
    let shared = shared_manager()?;
    reject_active_camera_uses()?;
    let _guard = shared.lock.lock().map_err(|e| e.to_string())?;
    reject_active_camera_uses()?;
    // SAFETY: mutable access is protected by `shared.lock`; active-use guards are rejected both
    // before and after taking the mutex so camera/request values cannot outlive a manager mutation.
    let slot = unsafe { &mut *shared.manager.get() };
    Ok(f(started(slot)?))
}

/// Run a closure with shared access to the running manager (starting it if needed).
///
/// Unlike [`with_manager_mut`] this is allowed while cameras are in use: listing cameras and
/// reading their properties does not change the manager, so probes keep working while other
/// cameras capture.
pub(crate) fn with_manager<R>(f: impl FnOnce(&CameraManager) -> R) -> Result<R, String> {
    let shared = shared_manager()?;
    let _guard = shared.lock.lock().map_err(|e| e.to_string())?;
    // SAFETY: shared access under `shared.lock`; the slot is only replaced (stop) under the same
    // lock, and never while camera uses exist.
    let manager = unsafe { &*shared.manager.get() }
        .as_ref()
        .ok_or_else(|| "libcamera manager stopped".to_string())?;
    Ok(f(manager))
}

/// Find a camera by id while holding the shared manager lock.
///
/// The active-use guard must be held until all camera/request objects returned from this lookup are
/// dropped, which prevents idle-stop from racing the camera lifetime.
pub fn find_camera(
    active_use: &ActiveCameraUse,
    id: &str,
) -> Result<(Option<Camera<'static>>, Vec<String>), String> {
    if !active_use.is_active() {
        return Err("libcamera active camera guard is not active".to_string());
    }
    let shared = shared_manager()?;
    let _guard = shared.lock.lock().map_err(|e| e.to_string())?;
    // SAFETY: camera lookup only needs shared access while `shared.lock` is held. The caller's
    // active-use guard prevents manager stop/mutation while returned camera handles remain alive.
    let manager = unsafe { &*shared.manager.get() }
        .as_ref()
        .ok_or_else(|| "libcamera manager stopped".to_string())?;
    let cameras = manager.cameras();
    let seen = (0..cameras.len())
        .filter_map(|idx| cameras.get(idx).map(|camera| camera.id().to_string()))
        .collect();
    let camera = (0..cameras.len()).find_map(|idx| {
        let camera = cameras.get(idx)?;
        if camera.id() == id {
            Some(camera)
        } else {
            None
        }
    });
    Ok((camera, seen))
}

/// Keeps the shared manager running (it delivers hotplug events) until dropped.
#[derive(Debug)]
pub struct HotplugSubscription {
    _private: (),
}

impl Drop for HotplugSubscription {
    fn drop(&mut self) {
        HOTPLUG_SUBSCRIBERS.fetch_sub(1, Ordering::AcqRel);
        if STOP_WHEN_IDLE.load(Ordering::Relaxed) {
            let _ = try_stop_if_idle();
        }
    }
}

/// Subscribe to libcamera hotplug events through the shared camera manager. The manager keeps
/// running while the returned subscription is alive.
pub fn subscribe_hotplug_events()
-> Result<(mpsc::Receiver<HotplugEvent>, HotplugSubscription), String> {
    HOTPLUG_SUBSCRIBERS.fetch_add(1, Ordering::AcqRel);
    let subscription = HotplugSubscription { _private: () };
    let receiver = with_manager_mut(|manager| manager.subscribe_hotplug_events())?;
    Ok((receiver, subscription))
}

/// Best-effort attempt to stop libcamera when no camera handles are alive.
///
/// This releases large PiSP/IPA allocations (seen as `/memfd:pisp_*`) so idle memory stays low.
pub fn try_stop_if_idle() -> Result<(), String> {
    let Some(shared) = MANAGER.get() else {
        return Ok(());
    };
    let in_use = || {
        ACTIVE_CAMERA_USES.load(Ordering::Acquire) != 0
            || HOTPLUG_SUBSCRIBERS.load(Ordering::Acquire) != 0
    };
    if in_use() {
        return Ok(());
    }
    let _guard = shared.lock.lock().map_err(|e| e.to_string())?;
    if in_use() {
        return Ok(());
    }
    // SAFETY: idle-stop mutation is protected by `shared.lock`, and the active-use count is checked
    // again after taking the mutex to avoid racing a new camera lookup.
    let slot = unsafe { &mut *shared.manager.get() };
    stop(slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{MutexGuard, OnceLock};

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("test lock")
    }

    #[test]
    fn typed_manager_config_sets_probe_cache_ttl() {
        let original = manager_config();
        set_manager_config(LibcameraManagerConfig {
            probe_cache_ttl_ms: 250,
            stop_when_idle: false,
        });

        assert_eq!(manager_config().probe_cache_ttl_ms, 250);
        assert!(!manager_config().stop_when_idle);
        assert_eq!(probe_cache_ttl(), Duration::from_millis(250));

        set_manager_config(original);
    }

    #[test]
    fn manager_mutation_is_rejected_while_camera_use_is_active() {
        let _guard = test_lock();
        ACTIVE_CAMERA_USES.fetch_add(1, Ordering::AcqRel);
        let result = reject_active_camera_uses();
        ACTIVE_CAMERA_USES.fetch_sub(1, Ordering::AcqRel);

        assert_eq!(
            result,
            Err("libcamera manager mutation blocked by active camera use".to_string())
        );
    }

    #[test]
    fn active_camera_uses_block_idle_stop_without_initializing_manager() {
        let _guard = test_lock();
        ACTIVE_CAMERA_USES.fetch_add(1, Ordering::AcqRel);
        let result = try_stop_if_idle();
        ACTIVE_CAMERA_USES.fetch_sub(1, Ordering::AcqRel);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn active_camera_use_counter_is_stable_under_concurrency() {
        let _guard = test_lock();
        let before = ACTIVE_CAMERA_USES.load(Ordering::Acquire);
        let mut threads = Vec::new();
        for _ in 0..8 {
            threads.push(std::thread::spawn(|| {
                for _ in 0..100 {
                    ACTIVE_CAMERA_USES.fetch_add(1, Ordering::AcqRel);
                    assert!(reject_active_camera_uses().is_err());
                    ACTIVE_CAMERA_USES.fetch_sub(1, Ordering::AcqRel);
                }
            }));
        }

        for thread in threads {
            thread.join().expect("stress thread");
        }

        assert_eq!(ACTIVE_CAMERA_USES.load(Ordering::Acquire), before);
    }
}
