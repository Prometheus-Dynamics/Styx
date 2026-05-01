use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// Default libcamera probe cache time-to-live (milliseconds).
pub const DEFAULT_LIBCAMERA_PROBE_CACHE_MS: u64 = 1_000;

/// Runtime configuration for the shared libcamera manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LibcameraManagerConfig {
    /// How long probe results remain cached. Set to `0` to effectively bypass the cache.
    pub probe_cache_ttl_ms: u64,
}

impl Default for LibcameraManagerConfig {
    fn default() -> Self {
        Self {
            probe_cache_ttl_ms: DEFAULT_LIBCAMERA_PROBE_CACHE_MS,
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
    }
}

/// Set typed runtime configuration for the shared libcamera manager.
///
/// `STYX_LIBCAMERA_PROBE_CACHE_MS` remains a process-level override for debugging and deployment
/// environments that cannot pass typed configuration.
pub fn set_manager_config(config: LibcameraManagerConfig) {
    PROBE_CACHE_TTL_MS.store(config.probe_cache_ttl_ms, Ordering::Relaxed);
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
    manager: UnsafeCell<CameraManager>,
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
    let mgr = unsafe { &mut *shared.manager.get() };
    if !mgr.is_started() {
        mgr.start().map_err(|e| e.to_string())?;
    }
    Ok(())
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
    MANAGER
        .set(SharedManager {
            manager: UnsafeCell::new(mgr),
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
pub(crate) fn with_manager_mut<R>(f: impl FnOnce(&mut CameraManager) -> R) -> Result<R, String> {
    let shared = shared_manager()?;
    reject_active_camera_uses()?;
    let _guard = shared.lock.lock().map_err(|e| e.to_string())?;
    reject_active_camera_uses()?;
    // SAFETY: mutable access is protected by `shared.lock`; active-use guards are rejected both
    // before and after taking the mutex so camera/request values cannot outlive a manager mutation.
    let mgr = unsafe { &mut *shared.manager.get() };
    if !mgr.is_started() {
        mgr.start().map_err(|e| e.to_string())?;
    }
    Ok(f(mgr))
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
    let manager = unsafe { &*shared.manager.get() };
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

/// Subscribe to libcamera hotplug events through the shared camera manager.
pub fn subscribe_hotplug_events() -> Result<mpsc::Receiver<HotplugEvent>, String> {
    with_manager_mut(|manager| manager.subscribe_hotplug_events())
}

/// Best-effort attempt to stop libcamera when no camera handles are alive.
///
/// This releases large PiSP/IPA allocations (seen as `/memfd:pisp_*`) so idle memory stays low.
pub fn try_stop_if_idle() -> Result<(), String> {
    let Some(shared) = MANAGER.get() else {
        return Ok(());
    };
    if ACTIVE_CAMERA_USES.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    let _guard = shared.lock.lock().map_err(|e| e.to_string())?;
    if ACTIVE_CAMERA_USES.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    // SAFETY: idle-stop mutation is protected by `shared.lock`, and the active-use count is checked
    // again after taking the mutex to avoid racing a new camera lookup.
    let mgr = unsafe { &mut *shared.manager.get() };
    if !mgr.is_started() {
        return Ok(());
    }
    mgr.try_stop().map_err(|e| e.to_string())
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
        });

        assert_eq!(manager_config().probe_cache_ttl_ms, 250);
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
