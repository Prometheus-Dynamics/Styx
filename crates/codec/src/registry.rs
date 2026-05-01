use std::sync::{Arc, RwLock};
use std::time::Instant;

#[cfg(feature = "codec-ffmpeg")]
use std::sync::atomic::{AtomicBool, Ordering};

use styx_core::prelude::*;

use crate::{
    Codec, CodecDescriptor, CodecImplementationId, CodecKind, CodecPolicy, CodecStats, Preference,
    RegistryError,
};

struct RegistryInner {
    codecs: std::collections::HashMap<FourCc, Vec<Arc<dyn Codec>>>,
    preferences: std::collections::HashMap<FourCc, Preference>,
    impl_priority: std::collections::HashMap<(FourCc, CodecImplementationId), i32>,
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

    fn select_auto(
        &self,
        fourcc: FourCc,
        candidates: Vec<Arc<dyn Codec>>,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        if candidates.is_empty() {
            return Err(RegistryError::NotFound(fourcc));
        }

        let policy = self.policies.get(&fourcc);
        let prefer_hw = policy
            .map(|p| p.prefer_hardware)
            .unwrap_or(self.default_prefer_hardware);
        if let Some(pref) = self.preferences.get(&fourcc) {
            if !pref.impls.is_empty() {
                for id in &pref.impls {
                    if let Some(codec) = candidates
                        .iter()
                        .find(|codec| impl_name_matches(codec.as_ref(), id))
                    {
                        return Ok(codec.clone());
                    }
                }
            }
            if pref.prefer_hardware
                && let Some(codec) = candidates
                    .iter()
                    .find(|codec| codec.descriptor().is_hardware_accelerated())
            {
                return Ok(codec.clone());
            }
        }

        candidates
            .into_iter()
            .min_by_key(|codec| {
                let id = codec.descriptor().implementation_id();
                let prio = self
                    .impl_priority
                    .get(&(fourcc, id.clone()))
                    .copied()
                    .unwrap_or(i32::MAX);
                let hw_bias = if prefer_hw && codec.descriptor().is_hardware_accelerated() {
                    0
                } else {
                    1
                };
                (prio, hw_bias, id)
            })
            .ok_or(RegistryError::NotFound(fourcc))
    }
}

fn sort_backends_for(
    priorities: &std::collections::HashMap<(FourCc, CodecImplementationId), i32>,
    default_prefer_hardware: bool,
    fourcc: FourCc,
    list: &mut Vec<Arc<dyn Codec>>,
) {
    list.sort_by_key(|c| {
        let id = c.descriptor().implementation_id();
        let prio = priorities
            .get(&(fourcc, id.clone()))
            .copied()
            .unwrap_or(i32::MAX);
        let hw_bias = if default_prefer_hardware && c.descriptor().is_hardware_accelerated() {
            0
        } else {
            1
        };
        (prio, hw_bias, id)
    });
}

fn impl_name_matches(codec: &dyn Codec, id: &CodecImplementationId) -> bool {
    codec.descriptor().implementation_id() == *id
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
        impl_name: impl Into<CodecImplementationId>,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        let impl_id = impl_name.into();
        guard
            .codecs
            .get(&fourcc)
            .and_then(|v| {
                v.iter()
                    .find(|c| impl_name_matches(c.as_ref(), &impl_id))
                    .cloned()
            })
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_named_kind(
        &self,
        fourcc: FourCc,
        kind: CodecKind,
        impl_name: impl Into<CodecImplementationId>,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        let impl_id = impl_name.into();
        guard
            .codecs
            .get(&fourcc)
            .and_then(|v| {
                v.iter()
                    .find(|c| {
                        c.descriptor().kind == kind && impl_name_matches(c.as_ref(), &impl_id)
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
        let preferred_ids: Vec<_> = preferred_impls
            .iter()
            .map(CodecImplementationId::new)
            .collect();
        self.lookup_preferred_ids(fourcc, &preferred_ids, prefer_hardware)
    }

    pub fn lookup_preferred_ids(
        &self,
        fourcc: FourCc,
        preferred_impls: &[CodecImplementationId],
        prefer_hardware: bool,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        let list = guard
            .codecs
            .get(&fourcc)
            .ok_or(RegistryError::NotFound(fourcc))?;
        if !preferred_impls.is_empty() {
            for pref in preferred_impls {
                if let Some(c) = list.iter().find(|c| impl_name_matches(c.as_ref(), pref)) {
                    return Ok(c.clone());
                }
            }
        }
        if prefer_hardware
            && let Some(c) = list
                .iter()
                .find(|c| c.descriptor().is_hardware_accelerated())
        {
            return Ok(c.clone());
        }
        list.first().cloned().ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_by_impl(
        &self,
        kind: CodecKind,
        impl_name: impl Into<CodecImplementationId>,
    ) -> Result<(FourCc, Arc<dyn Codec>), RegistryError> {
        let guard = self.inner.read().unwrap();
        let impl_id = impl_name.into();
        for (fcc, list) in guard.codecs.iter() {
            if let Some(c) = list
                .iter()
                .find(|c| c.descriptor().kind == kind && impl_name_matches(c.as_ref(), &impl_id))
            {
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
        impl_name: impl Into<CodecImplementationId>,
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
        let preferred_ids: Vec<_> = preferred_impls
            .iter()
            .map(CodecImplementationId::new)
            .collect();
        self.process_preferred_ids(fourcc, &preferred_ids, prefer_hardware, frame)
    }

    pub fn process_preferred_ids(
        &self,
        fourcc: FourCc,
        preferred_impls: &[CodecImplementationId],
        prefer_hardware: bool,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup_preferred_ids(fourcc, preferred_impls, prefer_hardware)?;
        self.run_codec(start, codec, frame)
    }
    pub fn set_preference(&self, fourcc: FourCc, preference: Preference) {
        let mut guard = self.inner.write().unwrap();
        guard.preferences.insert(fourcc, preference);
    }
    pub fn disable_impl(&self, fourcc: FourCc, impl_name: impl Into<CodecImplementationId>) {
        let mut guard = self.inner.write().unwrap();
        let impl_id = impl_name.into();
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            list.retain(|c| !impl_name_matches(c.as_ref(), &impl_id));
        }
    }
    pub fn enable_only(&self, fourcc: FourCc, impl_names: &[&str]) {
        let mut guard = self.inner.write().unwrap();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            let ids: Vec<_> = impl_names.iter().map(CodecImplementationId::new).collect();
            list.retain(|c| ids.iter().any(|id| impl_name_matches(c.as_ref(), id)));
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
    pub fn set_impl_priority(
        &self,
        fourcc: FourCc,
        impl_name: impl Into<CodecImplementationId>,
        priority: i32,
    ) {
        let mut guard = self.inner.write().unwrap();
        guard
            .impl_priority
            .insert((fourcc, impl_name.into()), priority);
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
        let candidates = guard
            .codecs
            .get(&fourcc)
            .ok_or(RegistryError::NotFound(fourcc))?
            .to_vec();
        guard.select_auto(fourcc, candidates)
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
        let candidates: Vec<_> = list_all
            .iter()
            .filter(|c| c.descriptor().kind == kind)
            .cloned()
            .collect();
        guard.select_auto(fourcc, candidates)
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
        let candidates: Vec<_> = list_all
            .iter()
            .filter(|c| c.descriptor().kind == kind)
            .filter(|c| c.descriptor().name.eq_ignore_ascii_case(codec_name))
            .cloned()
            .collect();
        guard.select_auto(fourcc, candidates)
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

pub const DEFAULT_CODEC_MAX_WIDTH: u32 = 1920;
pub const DEFAULT_CODEC_MAX_HEIGHT: u32 = 1080;

/// Runtime limits used when registering built-in codec implementations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecRegistryConfig {
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for CodecRegistryConfig {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_CODEC_MAX_WIDTH,
            max_height: DEFAULT_CODEC_MAX_HEIGHT,
        }
    }
}

impl CodecRegistryConfig {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        Self {
            max_width: max_width.max(1),
            max_height: max_height.max(1),
        }
    }
}

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
        Self::with_enabled_codecs_with_config(CodecRegistryConfig::default())
    }
    pub fn with_enabled_codecs_for_max(
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, crate::CodecError> {
        Self::with_enabled_codecs_with_config(CodecRegistryConfig::new(max_width, max_height))
    }
    pub fn with_enabled_codecs_with_config(
        config: CodecRegistryConfig,
    ) -> Result<Self, crate::CodecError> {
        let registry = Self::new();
        registry.register_enabled_codecs_with_config(config)?;
        Ok(registry)
    }
    pub fn register_enabled_codecs_default(&self) -> Result<(), crate::CodecError> {
        self.register_enabled_codecs_with_config(CodecRegistryConfig::default())
    }
    pub fn register_enabled_codecs_with_config(
        &self,
        config: CodecRegistryConfig,
    ) -> Result<(), crate::CodecError> {
        self.register_enabled_codecs(config.max_width, config.max_height)
    }
}

include!("registry_enabled.incl.rs");

#[cfg(test)]
mod tests {
    use super::*;

    struct TestCodec {
        descriptor: CodecDescriptor,
    }

    impl TestCodec {
        fn decoder(impl_name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                descriptor: CodecDescriptor {
                    kind: CodecKind::Decoder,
                    input: FourCc::MJPG,
                    output: FourCc::RG24,
                    name: "mjpeg",
                    impl_name,
                },
            })
        }
    }

    impl Codec for TestCodec {
        fn descriptor(&self) -> &CodecDescriptor {
            &self.descriptor
        }

        fn process(&self, input: FrameLease) -> Result<FrameLease, crate::CodecError> {
            Ok(input)
        }
    }

    #[test]
    fn policy_ordered_impls_normalize_to_typed_ids() {
        let policy = CodecPolicy::builder(FourCc::MJPG)
            .ordered_impls([" SOFT-CPU "])
            .build();

        assert_eq!(
            policy.ordered_impls[0],
            CodecImplementationId::new("soft-cpu")
        );
    }

    #[test]
    fn auto_lookup_uses_policy_priority_before_hardware_bias() {
        let registry = CodecRegistry::new();
        registry.register(FourCc::MJPG, TestCodec::decoder("h264-v4l2m2m"));
        registry.register(FourCc::MJPG, TestCodec::decoder("soft-cpu"));
        let handle = registry.handle();

        assert_eq!(
            handle
                .lookup_auto(FourCc::MJPG)
                .unwrap()
                .descriptor()
                .impl_name,
            "h264-v4l2m2m"
        );

        handle.set_policy(
            CodecPolicy::builder(FourCc::MJPG)
                .prefer_hardware(false)
                .priority(" SOFT-CPU ", 0)
                .build(),
        );

        assert_eq!(
            handle
                .lookup_auto(FourCc::MJPG)
                .unwrap()
                .descriptor()
                .impl_name,
            "soft-cpu"
        );
    }

    #[test]
    fn preference_accepts_ergonomic_strings_but_stores_typed_ids() {
        let preference = Preference::hardware_biased([" SOFT-CPU ", "h264-v4l2m2m"]);

        assert_eq!(
            preference.impls,
            vec![
                CodecImplementationId::new("soft-cpu"),
                CodecImplementationId::new("h264-v4l2m2m"),
            ]
        );
        assert!(preference.prefer_hardware);
    }

    #[test]
    fn preferred_lookup_accepts_typed_impl_ids() {
        let registry = CodecRegistry::new();
        registry.register(FourCc::MJPG, TestCodec::decoder("h264-v4l2m2m"));
        registry.register(FourCc::MJPG, TestCodec::decoder("soft-cpu"));
        let handle = registry.handle();

        let codec = handle
            .lookup_preferred_ids(
                FourCc::MJPG,
                &[CodecImplementationId::new(" SOFT-CPU ")],
                true,
            )
            .unwrap();

        assert_eq!(codec.descriptor().impl_name, "soft-cpu");
    }

    #[test]
    fn codec_registry_config_sanitizes_dimensions() {
        assert_eq!(
            CodecRegistryConfig::new(0, 720),
            CodecRegistryConfig {
                max_width: 1,
                max_height: 720,
            }
        );
    }
}
