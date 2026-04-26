use std::sync::{Arc, RwLock};
use std::time::Instant;

#[cfg(feature = "codec-ffmpeg")]
use std::sync::atomic::{AtomicBool, Ordering};

use styx_core::prelude::*;

use crate::{
    Codec, CodecDescriptor, CodecKind, CodecPolicy, CodecStats, Preference, RegistryError,
};

struct RegistryInner {
    codecs: std::collections::HashMap<FourCc, Vec<Arc<dyn Codec>>>,
    preferences: std::collections::HashMap<FourCc, Preference>,
    impl_priority: std::collections::HashMap<(FourCc, String), i32>,
    default_prefer_hardware: bool,
    policies: std::collections::HashMap<FourCc, CodecPolicy>,
}

impl RegistryInner {
    fn new() -> Self {
        Self {
            codecs: std::collections::HashMap::new(),
            preferences: std::collections::HashMap::new(),
            impl_priority: std::collections::HashMap::new(),
            default_prefer_hardware: true,
            policies: std::collections::HashMap::new(),
        }
    }
}

fn sort_backends_for(
    priorities: &std::collections::HashMap<(FourCc, String), i32>,
    default_prefer_hardware: bool,
    fourcc: FourCc,
    list: &mut Vec<Arc<dyn Codec>>,
) {
    list.sort_by_key(|c| {
        let impl_name = c.descriptor().impl_name.to_ascii_lowercase();
        let prio = priorities
            .get(&(fourcc, impl_name.clone()))
            .copied()
            .unwrap_or(i32::MAX);
        let hw_bias = if default_prefer_hardware && is_hardware_impl(&impl_name) {
            0
        } else {
            1
        };
        (prio, hw_bias, impl_name)
    });
}

fn is_hardware_impl(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "vaapi",
        "nvenc",
        "nvdec",
        "cuvid",
        "qsv",
        "v4l2",
        "videotoolbox",
        "v4l2m2m",
    ]
    .iter()
    .any(|tok| n.contains(tok))
}

#[cfg(feature = "codec-ffmpeg")]
static V4L2M2M_PROBE_DISABLED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "codec-ffmpeg")]
fn v4l2m2m_probe_enabled() -> bool {
    !V4L2M2M_PROBE_DISABLED.load(Ordering::Relaxed)
}

#[cfg(feature = "codec-ffmpeg")]
fn disable_v4l2m2m_probe() {
    V4L2M2M_PROBE_DISABLED.store(true, Ordering::Relaxed);
}

#[derive(Clone)]
pub struct CodecRegistryHandle {
    inner: Arc<RwLock<RegistryInner>>,
    stats: CodecStats,
}

impl CodecRegistryHandle {
    pub fn lookup(&self, fourcc: FourCc) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        guard
            .codecs
            .get(&fourcc)
            .and_then(|v| v.first().cloned())
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_named(
        &self,
        fourcc: FourCc,
        impl_name: &str,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        guard
            .codecs
            .get(&fourcc)
            .and_then(|v| {
                v.iter()
                    .find(|c| c.descriptor().impl_name.eq_ignore_ascii_case(impl_name))
                    .cloned()
            })
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_named_kind(
        &self,
        fourcc: FourCc,
        kind: CodecKind,
        impl_name: &str,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        guard
            .codecs
            .get(&fourcc)
            .and_then(|v| {
                v.iter()
                    .find(|c| {
                        c.descriptor().kind == kind
                            && c.descriptor().impl_name.eq_ignore_ascii_case(impl_name)
                    })
                    .cloned()
            })
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_preferred(
        &self,
        fourcc: FourCc,
        preferred_impls: &[&str],
        prefer_hardware: bool,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        let list = guard
            .codecs
            .get(&fourcc)
            .ok_or(RegistryError::NotFound(fourcc))?;
        if !preferred_impls.is_empty() {
            for pref in preferred_impls {
                if let Some(c) = list
                    .iter()
                    .find(|c| c.descriptor().impl_name.eq_ignore_ascii_case(pref))
                {
                    return Ok(c.clone());
                }
            }
        }
        if prefer_hardware
            && let Some(c) = list
                .iter()
                .find(|c| is_hardware_impl(c.descriptor().impl_name))
        {
            return Ok(c.clone());
        }
        list.first().cloned().ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_by_impl(
        &self,
        kind: CodecKind,
        impl_name: &str,
    ) -> Result<(FourCc, Arc<dyn Codec>), RegistryError> {
        let guard = self.inner.read().unwrap();
        for (fcc, list) in guard.codecs.iter() {
            if let Some(c) = list.iter().find(|c| {
                c.descriptor().kind == kind
                    && c.descriptor().impl_name.eq_ignore_ascii_case(impl_name)
            }) {
                return Ok((*fcc, c.clone()));
            }
        }
        Err(RegistryError::NotFound(FourCc::new(*b"    ")))
    }
    pub fn process(&self, fourcc: FourCc, frame: FrameLease) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup(fourcc)?;
        self.run_codec(start, codec, frame)
    }
    pub fn process_named(
        &self,
        fourcc: FourCc,
        impl_name: &str,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup_named(fourcc, impl_name)?;
        self.run_codec(start, codec, frame)
    }
    pub fn process_preferred(
        &self,
        fourcc: FourCc,
        preferred_impls: &[&str],
        prefer_hardware: bool,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup_preferred(fourcc, preferred_impls, prefer_hardware)?;
        self.run_codec(start, codec, frame)
    }
    pub fn set_preference(&self, fourcc: FourCc, preference: Preference) {
        let mut guard = self.inner.write().unwrap();
        guard.preferences.insert(fourcc, preference);
    }
    pub fn disable_impl(&self, fourcc: FourCc, impl_name: &str) {
        let mut guard = self.inner.write().unwrap();
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            list.retain(|c| !c.descriptor().impl_name.eq_ignore_ascii_case(impl_name));
        }
    }
    pub fn enable_only(&self, fourcc: FourCc, impl_names: &[&str]) {
        let mut guard = self.inner.write().unwrap();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            let names: Vec<String> = impl_names.iter().map(|s| s.to_ascii_lowercase()).collect();
            list.retain(|c| {
                names
                    .iter()
                    .any(|n| c.descriptor().impl_name.eq_ignore_ascii_case(n))
            });
            sort_backends_for(&priorities, prefer_hw, fourcc, list);
        }
    }
    pub fn register_dynamic(&self, fourcc: FourCc, codec: Arc<dyn Codec>) {
        let mut guard = self.inner.write().unwrap();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        let list = guard.codecs.entry(fourcc).or_default();
        list.push(codec);
        sort_backends_for(&priorities, prefer_hw, fourcc, list);
    }
    pub fn set_impl_priority(&self, fourcc: FourCc, impl_name: &str, priority: i32) {
        let mut guard = self.inner.write().unwrap();
        guard
            .impl_priority
            .insert((fourcc, impl_name.to_ascii_lowercase()), priority);
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            sort_backends_for(&priorities, prefer_hw, fourcc, list);
        }
    }
    pub fn set_default_hardware_bias(&self, prefer: bool) {
        let mut guard = self.inner.write().unwrap();
        guard.default_prefer_hardware = prefer;
    }
    pub fn set_policy(&self, policy: CodecPolicy) {
        let mut guard = self.inner.write().unwrap();
        guard.default_prefer_hardware = policy.prefer_hardware;
        guard.impl_priority.extend(
            policy
                .priorities
                .clone()
                .into_iter()
                .map(|(k, v)| ((policy.fourcc, k), v)),
        );
        if !policy.ordered_impls.is_empty() {
            guard.preferences.insert(
                policy.fourcc,
                Preference {
                    impls: policy.ordered_impls.clone(),
                    prefer_hardware: policy.prefer_hardware,
                },
            );
        }
        guard.policies.insert(policy.fourcc, policy);
    }
    pub fn lookup_auto(&self, fourcc: FourCc) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        let list = guard
            .codecs
            .get(&fourcc)
            .ok_or(RegistryError::NotFound(fourcc))?;
        let policy = guard.policies.get(&fourcc);
        let prefer_hw = policy
            .map(|p| p.prefer_hardware)
            .unwrap_or(guard.default_prefer_hardware);
        if let Some(pref) = guard.preferences.get(&fourcc) {
            if !pref.impls.is_empty() {
                for name in &pref.impls {
                    if let Some(c) = list
                        .iter()
                        .find(|c| c.descriptor().impl_name.eq_ignore_ascii_case(name))
                    {
                        return Ok(c.clone());
                    }
                }
            }
            if pref.prefer_hardware
                && let Some(c) = list
                    .iter()
                    .find(|c| is_hardware_impl(c.descriptor().impl_name))
            {
                return Ok(c.clone());
            }
        }
        let impl_prio = &guard.impl_priority;
        list.iter()
            .min_by_key(|c| {
                let name = c.descriptor().impl_name.to_ascii_lowercase();
                let prio = impl_prio
                    .get(&(fourcc, name.clone()))
                    .copied()
                    .unwrap_or(i32::MAX);
                let hw_bias = if prefer_hw && is_hardware_impl(&name) {
                    0
                } else {
                    1
                };
                (prio, hw_bias, name)
            })
            .cloned()
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_auto_kind(
        &self,
        fourcc: FourCc,
        kind: CodecKind,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        let list_all = guard
            .codecs
            .get(&fourcc)
            .ok_or(RegistryError::NotFound(fourcc))?;
        let list: Vec<&Arc<dyn Codec>> = list_all
            .iter()
            .filter(|c| c.descriptor().kind == kind)
            .collect();
        if list.is_empty() {
            return Err(RegistryError::NotFound(fourcc));
        }
        let policy = guard.policies.get(&fourcc);
        let prefer_hw = policy
            .map(|p| p.prefer_hardware)
            .unwrap_or(guard.default_prefer_hardware);
        if let Some(pref) = guard.preferences.get(&fourcc) {
            if !pref.impls.is_empty() {
                for name in &pref.impls {
                    if let Some(c) = list
                        .iter()
                        .find(|c| c.descriptor().impl_name.eq_ignore_ascii_case(name))
                    {
                        return Ok((*c).clone());
                    }
                }
            }
            if pref.prefer_hardware
                && let Some(c) = list
                    .iter()
                    .find(|c| is_hardware_impl(c.descriptor().impl_name))
            {
                return Ok((*c).clone());
            }
        }
        let impl_prio = &guard.impl_priority;
        list.iter()
            .min_by_key(|c| {
                let name = c.descriptor().impl_name.to_ascii_lowercase();
                let prio = impl_prio
                    .get(&(fourcc, name.clone()))
                    .copied()
                    .unwrap_or(i32::MAX);
                let hw_bias = if prefer_hw && is_hardware_impl(&name) {
                    0
                } else {
                    1
                };
                (prio, hw_bias, name)
            })
            .cloned()
            .cloned()
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_auto_kind_by_name(
        &self,
        fourcc: FourCc,
        kind: CodecKind,
        codec_name: &str,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        let list_all = guard
            .codecs
            .get(&fourcc)
            .ok_or(RegistryError::NotFound(fourcc))?;
        let list: Vec<&Arc<dyn Codec>> = list_all
            .iter()
            .filter(|c| {
                c.descriptor().kind == kind && c.descriptor().name.eq_ignore_ascii_case(codec_name)
            })
            .collect();
        if list.is_empty() {
            return Err(RegistryError::NotFound(fourcc));
        }
        let policy = guard.policies.get(&fourcc);
        let prefer_hw = policy
            .map(|p| p.prefer_hardware)
            .unwrap_or(guard.default_prefer_hardware);
        if let Some(pref) = guard.preferences.get(&fourcc) {
            if !pref.impls.is_empty() {
                for name in &pref.impls {
                    if let Some(c) = list
                        .iter()
                        .find(|c| c.descriptor().impl_name.eq_ignore_ascii_case(name))
                    {
                        return Ok((*c).clone());
                    }
                }
            }
            if pref.prefer_hardware
                && let Some(c) = list
                    .iter()
                    .find(|c| is_hardware_impl(c.descriptor().impl_name))
            {
                return Ok((*c).clone());
            }
        }
        let impl_prio = &guard.impl_priority;
        list.iter()
            .min_by_key(|c| {
                let name = c.descriptor().impl_name.to_ascii_lowercase();
                let prio = impl_prio
                    .get(&(fourcc, name.clone()))
                    .copied()
                    .unwrap_or(i32::MAX);
                let hw_bias = if prefer_hw && is_hardware_impl(&name) {
                    0
                } else {
                    1
                };
                (prio, hw_bias, name)
            })
            .cloned()
            .cloned()
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn process_auto(
        &self,
        fourcc: FourCc,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup_auto(fourcc)?;
        self.run_codec(start, codec, frame)
    }
    pub fn process_auto_kind(
        &self,
        fourcc: FourCc,
        kind: CodecKind,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup_auto_kind(fourcc, kind)?;
        self.run_codec(start, codec, frame)
    }
    pub fn process_auto_kind_by_name(
        &self,
        fourcc: FourCc,
        kind: CodecKind,
        codec_name: &str,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup_auto_kind_by_name(fourcc, kind, codec_name)?;
        self.run_codec(start, codec, frame)
    }
    pub fn stats(&self) -> CodecStats {
        self.stats.clone()
    }
    pub fn list_registered(&self) -> Vec<(FourCc, Vec<CodecDescriptor>)> {
        let guard = self.inner.read().unwrap();
        guard
            .codecs
            .iter()
            .map(|(fourcc, list)| {
                (
                    *fourcc,
                    list.iter().map(|c| c.descriptor().clone()).collect(),
                )
            })
            .collect()
    }
    pub fn list_registered_by_kind(&self, kind: CodecKind) -> Vec<(FourCc, Vec<CodecDescriptor>)> {
        self.list_registered()
            .into_iter()
            .filter_map(|(fcc, descs)| {
                let filtered: Vec<_> = descs.into_iter().filter(|d| d.kind == kind).collect();
                if filtered.is_empty() {
                    None
                } else {
                    Some((fcc, filtered))
                }
            })
            .collect()
    }
    fn run_codec(
        &self,
        start: Instant,
        codec: Arc<dyn Codec>,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let expected = codec.descriptor().input;
        let actual = frame.meta().format.code;
        let frame = if actual != expected {
            if let Some(converter) = self.lookup_converter(actual, expected) {
                match converter.process(frame) {
                    Ok(converted) => converted,
                    Err(err) => {
                        self.stats.inc_errors();
                        return Err(RegistryError::Codec(err));
                    }
                }
            } else {
                frame
            }
        } else {
            frame
        };
        match codec.process(frame) {
            Ok(out) => {
                self.stats.inc_processed();
                self.stats.record_duration(start.elapsed());
                Ok(out)
            }
            Err(err) => {
                if matches!(err, crate::CodecError::Backpressure) {
                    self.stats.inc_backpressure();
                } else {
                    self.stats.inc_errors();
                }
                Err(RegistryError::Codec(err))
            }
        }
    }
    fn lookup_converter(&self, actual: FourCc, expected: FourCc) -> Option<Arc<dyn Codec>> {
        let guard = self.inner.read().unwrap();
        let list = guard.codecs.get(&actual)?;
        list.iter()
            .find(|c| c.descriptor().output == expected)
            .cloned()
    }
}

pub struct CodecRegistry {
    handle: CodecRegistryHandle,
}

const DEFAULT_CODEC_MAX_WIDTH: u32 = 1920;
const DEFAULT_CODEC_MAX_HEIGHT: u32 = 1080;

impl Default for CodecRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CodecRegistry {
    pub fn new() -> Self {
        let inner = RegistryInner::new();
        let handle = CodecRegistryHandle {
            inner: Arc::new(RwLock::new(inner)),
            stats: CodecStats::default(),
        };
        Self { handle }
    }
    pub fn handle(&self) -> CodecRegistryHandle {
        self.handle.clone()
    }
    pub fn register(&self, fourcc: FourCc, codec: Arc<dyn Codec>) {
        let mut guard = self.handle.inner.write().unwrap();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        let list = guard.codecs.entry(fourcc).or_default();
        list.push(codec);
        sort_backends_for(&priorities, prefer_hw, fourcc, list);
    }
    pub fn with_enabled_codecs() -> Result<Self, crate::CodecError> {
        Self::with_enabled_codecs_for_max(DEFAULT_CODEC_MAX_WIDTH, DEFAULT_CODEC_MAX_HEIGHT)
    }
    pub fn with_enabled_codecs_for_max(
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, crate::CodecError> {
        let registry = Self::new();
        registry.register_enabled_codecs(max_width, max_height)?;
        Ok(registry)
    }
    pub fn register_enabled_codecs_default(&self) -> Result<(), crate::CodecError> {
        self.register_enabled_codecs(DEFAULT_CODEC_MAX_WIDTH, DEFAULT_CODEC_MAX_HEIGHT)
    }
}

include!("registry_enabled.incl.rs");
