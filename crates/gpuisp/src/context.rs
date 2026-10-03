//! The Vulkan instance, device and queue: loaded at run time, chosen once, shared by every
//! [`GpuIsp`](crate::GpuIsp) made from it.

use std::ffi::CStr;
use std::sync::{Arc, Mutex};

use ash::vk;

use crate::error::{GpuError, VkContext};

/// The kind of a Vulkan device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Discrete,
    Integrated,
    Virtual,
    /// A software rasteriser (llvmpipe, SwiftShader): correct, slower than `styx-softisp`.
    Cpu,
    Other,
}

impl DeviceKind {
    fn of(t: vk::PhysicalDeviceType) -> Self {
        match t {
            vk::PhysicalDeviceType::DISCRETE_GPU => Self::Discrete,
            vk::PhysicalDeviceType::INTEGRATED_GPU => Self::Integrated,
            vk::PhysicalDeviceType::VIRTUAL_GPU => Self::Virtual,
            vk::PhysicalDeviceType::CPU => Self::Cpu,
            _ => Self::Other,
        }
    }

    /// Order of preference when nothing is asked for (lower first).
    fn rank(self) -> u8 {
        match self {
            Self::Discrete => 0,
            Self::Integrated => 1,
            Self::Virtual => 2,
            Self::Other => 3,
            Self::Cpu => 4,
        }
    }
}

/// A device the GPU ISP can run on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Position among the suitable devices ([`devices`]).
    pub index: usize,
    pub name: String,
    pub kind: DeviceKind,
    /// The driver's name and version (`radv`, `Mesa 26.2.3`).
    pub driver: String,
    /// Vulkan version (major, minor, patch).
    pub api: (u32, u32, u32),
    /// `VK_EXT_external_memory_dma_buf`: capture buffers import, outputs export, as dma-bufs.
    pub dmabuf: bool,
    /// Whether device-local memory is separate from what the CPU maps (a discrete GPU): frames
    /// are then copied in and out by the GPU's copy engine within the same submission.
    pub separate_memory: bool,
}

/// Which device [`GpuContext::open`] takes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceSelect {
    /// `STYX_GPUISP_DEVICE` (an index into [`devices`] or part of a device name) when set,
    /// else the first hardware device: discrete, integrated, virtual. Never a software
    /// rasteriser unless the variable names it.
    #[default]
    Auto,
    /// An index into [`devices`].
    Index(usize),
    /// The first device whose name contains this (ignoring case).
    Name(String),
    /// The first device of any kind, software rasterisers included (tests).
    Any,
}

pub(crate) struct Inner {
    _entry: ash::Entry,
    pub instance: ash::Instance,
    pub device: ash::Device,
    pub queue: Mutex<vk::Queue>,
    pub queue_family: u32,
    pub memory: vk::PhysicalDeviceMemoryProperties,
    pub limits: vk::PhysicalDeviceLimits,
    pub external_fd: Option<ash::khr::external_memory_fd::Device>,
    pub timestamps: bool,
    pub info: DeviceInfo,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: every object made from the device belongs to a `GpuIsp`, which holds an
        // `Arc` of this, so all are gone.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// A Vulkan device and queue for GPU ISPs. Cheap to clone (shared).
#[derive(Clone)]
pub struct GpuContext {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("GpuContext").field(&self.inner.info).finish()
    }
}

struct Candidate {
    physical: vk::PhysicalDevice,
    family: u32,
    info: DeviceInfo,
    timestamps: bool,
}

fn load() -> Result<(ash::Entry, ash::Instance), GpuError> {
    // SAFETY: loading the system's Vulkan loader; its initialisation runs no Rust code.
    let entry = unsafe { ash::Entry::load() }.map_err(|e| GpuError::NoVulkan(e.to_string()))?;
    let app = vk::ApplicationInfo::default()
        .application_name(c"styx-gpuisp")
        .api_version(vk::API_VERSION_1_2);
    let create = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: valid create info; the instance is destroyed by `Inner::drop` or below.
    let instance = unsafe { entry.create_instance(&create, None) }
        .map_err(|e| GpuError::NoVulkan(format!("vkCreateInstance: {e}")))?;
    Ok((entry, instance))
}

fn has_extension(props: &[vk::ExtensionProperties], name: &CStr) -> bool {
    props
        .iter()
        .any(|p| p.extension_name_as_c_str().is_ok_and(|n| n == name))
}

/// The suitable devices of `instance`, in enumeration order.
fn candidates(instance: &ash::Instance) -> Result<Vec<Candidate>, GpuError> {
    // SAFETY: plain queries of a live instance.
    let physicals =
        unsafe { instance.enumerate_physical_devices() }.ctx("vkEnumeratePhysicalDevices")?;
    let mut out = Vec::new();
    for physical in physicals {
        // SAFETY: as above.
        let props = unsafe { instance.get_physical_device_properties(physical) };
        if props.api_version < vk::API_VERSION_1_2 {
            continue;
        }
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut v12);
        // SAFETY: as above.
        unsafe { instance.get_physical_device_features2(physical, &mut features) };
        if v12.storage_buffer8_bit_access == vk::FALSE {
            continue;
        }
        // SAFETY: as above.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let Some((family, fam)) = families
            .iter()
            .enumerate()
            .find(|(_, f)| f.queue_flags.contains(vk::QueueFlags::COMPUTE))
        else {
            continue;
        };
        // SAFETY: as above.
        let exts =
            unsafe { instance.enumerate_device_extension_properties(physical) }.unwrap_or_default();
        let dmabuf = has_extension(&exts, ash::khr::external_memory_fd::NAME)
            && has_extension(&exts, ash::ext::external_memory_dma_buf::NAME);
        let mut driver = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
        // SAFETY: as above.
        unsafe { instance.get_physical_device_properties2(physical, &mut props2) };
        // SAFETY: as above.
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        let types = &memory.memory_types[..memory.memory_type_count as usize];
        let separate_memory = types.iter().any(|t| {
            t.property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                && !t
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
        });
        let text = |s: Result<&CStr, _>| {
            s.map_or(String::new(), |c: &CStr| c.to_string_lossy().into_owned())
        };
        let v = props.api_version;
        out.push(Candidate {
            physical,
            family: family as u32,
            timestamps: fam.timestamp_valid_bits > 0 && props.limits.timestamp_period > 0.0,
            info: DeviceInfo {
                index: out.len(),
                name: text(props.device_name_as_c_str()),
                kind: DeviceKind::of(props.device_type),
                driver: format!(
                    "{} {}",
                    text(driver.driver_name_as_c_str()),
                    text(driver.driver_info_as_c_str())
                ),
                api: (
                    vk::api_version_major(v),
                    vk::api_version_minor(v),
                    vk::api_version_patch(v),
                ),
                dmabuf,
                separate_memory,
            },
        });
    }
    Ok(out)
}

/// The devices the GPU ISP can run on (empty without Vulkan or a suitable device).
pub fn devices() -> Vec<DeviceInfo> {
    let Ok((_entry, instance)) = load() else {
        return Vec::new();
    };
    let list = candidates(&instance)
        .map(|c| c.into_iter().map(|c| c.info).collect())
        .unwrap_or_default();
    // SAFETY: nothing was made from the instance.
    unsafe { instance.destroy_instance(None) };
    list
}

fn pick(list: Vec<Candidate>, select: &DeviceSelect) -> Result<Candidate, GpuError> {
    let names = || {
        list.iter()
            .map(|c| format!("{}: {} ({:?})", c.info.index, c.info.name, c.info.kind))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let select = match select {
        DeviceSelect::Auto => match std::env::var("STYX_GPUISP_DEVICE") {
            Ok(v) if !v.is_empty() => match v.parse::<usize>() {
                Ok(i) => DeviceSelect::Index(i),
                Err(_) => DeviceSelect::Name(v),
            },
            _ => DeviceSelect::Auto,
        },
        s => s.clone(),
    };
    let found = match &select {
        DeviceSelect::Auto => list
            .iter()
            .filter(|c| c.info.kind != DeviceKind::Cpu)
            .min_by_key(|c| c.info.kind.rank())
            .map(|c| c.info.index),
        DeviceSelect::Any => list.first().map(|c| c.info.index),
        DeviceSelect::Index(i) => list.get(*i).map(|c| c.info.index),
        DeviceSelect::Name(n) => {
            let n = n.to_lowercase();
            list.iter()
                .find(|c| c.info.name.to_lowercase().contains(&n))
                .map(|c| c.info.index)
        }
    };
    let Some(i) = found else {
        return Err(GpuError::NoDevice(format!(
            "{select:?} among [{}]",
            names()
        )));
    };
    Ok(list.into_iter().nth(i).expect("index from the list"))
}

impl GpuContext {
    /// The device `select` names (see [`DeviceSelect::Auto`]).
    pub fn open(select: DeviceSelect) -> Result<Self, GpuError> {
        let (entry, instance) = load()?;
        let chosen = candidates(&instance).and_then(|list| pick(list, &select));
        let chosen = match chosen {
            Ok(c) => c,
            Err(e) => {
                // SAFETY: nothing was made from the instance.
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };
        match Self::create_device(&instance, &chosen) {
            Ok((device, queue)) => {
                // SAFETY: plain queries of a live instance.
                let (memory, limits) = unsafe {
                    (
                        instance.get_physical_device_memory_properties(chosen.physical),
                        instance
                            .get_physical_device_properties(chosen.physical)
                            .limits,
                    )
                };
                let external_fd = chosen
                    .info
                    .dmabuf
                    .then(|| ash::khr::external_memory_fd::Device::new(&instance, &device));
                Ok(Self {
                    inner: Arc::new(Inner {
                        _entry: entry,
                        queue: Mutex::new(queue),
                        queue_family: chosen.family,
                        memory,
                        limits,
                        external_fd,
                        timestamps: chosen.timestamps,
                        info: chosen.info,
                        device,
                        instance,
                    }),
                })
            }
            Err(e) => {
                // SAFETY: the device was not made.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn create_device(
        instance: &ash::Instance,
        c: &Candidate,
    ) -> Result<(ash::Device, vk::Queue), GpuError> {
        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(c.family)
            .queue_priorities(&priorities)];
        let mut exts = Vec::new();
        if c.info.dmabuf {
            exts.push(ash::khr::external_memory_fd::NAME.as_ptr());
            exts.push(ash::ext::external_memory_dma_buf::NAME.as_ptr());
        }
        let mut v12 =
            vk::PhysicalDeviceVulkan12Features::default().storage_buffer8_bit_access(true);
        let create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&exts)
            .push_next(&mut v12);
        // SAFETY: a valid create info for a device of `instance`.
        let device =
            unsafe { instance.create_device(c.physical, &create, None) }.ctx("vkCreateDevice")?;
        // SAFETY: queue 0 of a family the device was made with.
        let queue = unsafe { device.get_device_queue(c.family, 0) };
        Ok((device, queue))
    }

    /// The device in use.
    pub fn info(&self) -> &DeviceInfo {
        &self.inner.info
    }
}
