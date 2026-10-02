use std::sync::Arc;
use std::time::Instant;

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;
use styx_core::prelude::*;

use crate::{
    Codec, CodecDescriptor, CodecImplementationId, CodecKind, CodecPolicy, CodecStats, Preference,
    RegistryError,
};

/// Registers codecs for one input format when that format is first looked up.
pub type DeferredCodecs = Box<dyn FnOnce() -> Vec<Arc<dyn Codec>> + Send + Sync>;

struct RegistryInner {
    codecs: std::collections::HashMap<FourCc, Vec<Arc<dyn Codec>>>,
    /// Registrations that are costly to make up front (e.g. probing hardware through FFmpeg),
    /// run on the first lookup of their input format.
    deferred: std::collections::HashMap<FourCc, Vec<DeferredCodecs>>,
    preferences: std::collections::HashMap<FourCc, Preference>,
    impl_priority: std::collections::HashMap<(FourCc, CodecImplementationId), i32>,
    default_prefer_hardware: bool,
    policies: std::collections::HashMap<FourCc, CodecPolicy>,
}

impl RegistryInner {
    fn new() -> Self {
        Self {
            codecs: std::collections::HashMap::new(),
            deferred: std::collections::HashMap::new(),
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
                && let Some(codec) = preferred_kind(&candidates)
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
                (kind_rank(codec.as_ref()), prio, hw_bias, id)
            })
            .ok_or(RegistryError::NotFound(fourcc))
    }
}

/// Decoders (and converters) before encoders: a format can have both (YUYV has a YUYV → RGB
/// decoder and YUYV → MJPEG/H.264 encoders), and a lookup that names no kind turns a frame of
/// that format into something usable, it does not compress it.
fn kind_rank(codec: &dyn Codec) -> u8 {
    match codec.descriptor().kind {
        CodecKind::Decoder => 0,
        CodecKind::Encoder => 1,
    }
}

/// The candidates of the kind a lookup without one picks ([`kind_rank`]).
fn preferred_kind(candidates: &[Arc<dyn Codec>]) -> Vec<Arc<dyn Codec>> {
    let best = candidates.iter().map(|c| kind_rank(c.as_ref())).min();
    candidates
        .iter()
        .filter(|c| Some(kind_rank(c.as_ref())) == best)
        .cloned()
        .collect()
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
        (kind_rank(c.as_ref()), prio, hw_bias, id)
    });
}

fn impl_name_matches(codec: &dyn Codec, id: &CodecImplementationId) -> bool {
    codec.descriptor().implementation_id() == *id
}

#[derive(Clone)]
pub struct CodecRegistryHandle {
    inner: Arc<RwLock<RegistryInner>>,
    stats: CodecStats,
    /// Whether any deferred registrations are pending (checked without locking).
    has_deferred: Arc<AtomicBool>,
}

impl CodecRegistryHandle {
    /// Register codecs for `fourcc` the first time `fourcc` is looked up.
    pub fn register_deferred(&self, fourcc: FourCc, codecs: DeferredCodecs) {
        self.inner
            .write()
            .deferred
            .entry(fourcc)
            .or_default()
            .push(codecs);
        self.has_deferred.store(true, Ordering::Release);
    }

    fn insert(&self, fourcc: FourCc, codec: Arc<dyn Codec>) {
        let mut guard = self.inner.write();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        let list = guard.codecs.entry(fourcc).or_default();
        list.push(codec);
        sort_backends_for(&priorities, prefer_hw, fourcc, list);
    }

    /// Run pending deferred registrations for `fourcc` (all formats when `None`).
    fn materialize(&self, fourcc: Option<FourCc>) {
        if !self.has_deferred.load(Ordering::Acquire) {
            return;
        }
        let pending: Vec<(FourCc, Vec<DeferredCodecs>)> = {
            let mut guard = self.inner.write();
            let pending = match fourcc {
                Some(fourcc) => guard
                    .deferred
                    .remove(&fourcc)
                    .map(|list| vec![(fourcc, list)])
                    .unwrap_or_default(),
                None => guard.deferred.drain().collect(),
            };
            self.has_deferred
                .store(!guard.deferred.is_empty(), Ordering::Release);
            pending
        };
        for (fourcc, providers) in pending {
            for provider in providers {
                for codec in provider() {
                    self.insert(fourcc, codec);
                }
            }
        }
    }

    /// The preferred codec that turns `input` into `output`, e.g. MJPG → GREY. Uses the same
    /// ordering as [`CodecRegistryHandle::lookup`] (priorities, then hardware when preferred).
    pub fn lookup_for_output(
        &self,
        input: FourCc,
        output: FourCc,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        self.materialize(Some(input));
        let guard = self.inner.read();
        guard
            .codecs
            .get(&input)
            .and_then(|list| {
                list.iter()
                    .find(|c| c.descriptor().output == output)
                    .cloned()
            })
            .ok_or(RegistryError::NotFound(input))
    }

    /// Like [`CodecRegistryHandle::lookup_for_output`], restricted to codecs whose descriptor
    /// satisfies `accept` (e.g. a hardware policy or a forbid list).
    pub fn lookup_for_output_where(
        &self,
        input: FourCc,
        output: FourCc,
        accept: impl Fn(&CodecDescriptor) -> bool,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        self.materialize(Some(input));
        let guard = self.inner.read();
        guard
            .codecs
            .get(&input)
            .and_then(|list| {
                list.iter()
                    .find(|c| c.descriptor().output == output && accept(c.descriptor()))
                    .cloned()
            })
            .ok_or(RegistryError::NotFound(input))
    }

    /// The first codec for input `fourcc` in the registry's order: a decoder (or converter)
    /// when `fourcc` has one, else an encoder. Ask by kind
    /// ([`CodecRegistryHandle::lookup_preferred_kind`]) or by output
    /// ([`CodecRegistryHandle::lookup_for_output`]) when that matters.
    pub fn lookup(&self, fourcc: FourCc) -> Result<Arc<dyn Codec>, RegistryError> {
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
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
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
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
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
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
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
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
            && let Some(c) = preferred_kind(list)
                .iter()
                .find(|c| c.descriptor().is_hardware_accelerated())
        {
            return Ok(c.clone());
        }
        list.first().cloned().ok_or(RegistryError::NotFound(fourcc))
    }

    /// The preferred codec of `kind` for `fourcc`: a hardware one first when `prefer_hardware`,
    /// else the first in the registry's order.
    pub fn lookup_preferred_kind(
        &self,
        fourcc: FourCc,
        kind: CodecKind,
        prefer_hardware: bool,
    ) -> Result<Arc<dyn Codec>, RegistryError> {
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
        let of_kind: Vec<&Arc<dyn Codec>> = guard
            .codecs
            .get(&fourcc)
            .into_iter()
            .flatten()
            .filter(|c| c.descriptor().kind == kind)
            .collect();
        let hardware = of_kind
            .iter()
            .find(|c| prefer_hardware && c.descriptor().is_hardware_accelerated());
        hardware
            .or(of_kind.first())
            .map(|c| Arc::clone(c))
            .ok_or(RegistryError::NotFound(fourcc))
    }
    pub fn lookup_by_impl(
        &self,
        kind: CodecKind,
        impl_name: impl Into<CodecImplementationId>,
    ) -> Result<(FourCc, Arc<dyn Codec>), RegistryError> {
        self.materialize(None);
        let guard = self.inner.read();
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
    /// Runs `frame` through [`CodecRegistryHandle::lookup`]'s codec: decodes or converts it
    /// when `fourcc` has a decoder.
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
        let mut guard = self.inner.write();
        guard.preferences.insert(fourcc, preference);
    }
    pub fn disable_impl(&self, fourcc: FourCc, impl_name: impl Into<CodecImplementationId>) {
        let mut guard = self.inner.write();
        let impl_id = impl_name.into();
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            list.retain(|c| !impl_name_matches(c.as_ref(), &impl_id));
        }
    }
    pub fn enable_only(&self, fourcc: FourCc, impl_names: &[&str]) {
        let mut guard = self.inner.write();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            let ids: Vec<_> = impl_names.iter().map(CodecImplementationId::new).collect();
            list.retain(|c| ids.iter().any(|id| impl_name_matches(c.as_ref(), id)));
            sort_backends_for(&priorities, prefer_hw, fourcc, list);
        }
    }
    pub fn register_dynamic(&self, fourcc: FourCc, codec: Arc<dyn Codec>) {
        let mut guard = self.inner.write();
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
        let mut guard = self.inner.write();
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
        let mut guard = self.inner.write();
        guard.default_prefer_hardware = prefer;
    }
    pub fn set_policy(&self, policy: CodecPolicy) {
        let mut guard = self.inner.write();
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
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
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
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
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
        self.materialize(Some(fourcc));
        let guard = self.inner.read();
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
        self.materialize(None);
        let guard = self.inner.read();
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
        self.materialize(Some(actual));
        let guard = self.inner.read();
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
            has_deferred: Arc::new(AtomicBool::new(false)),
        };
        Self { handle }
    }
    pub fn handle(&self) -> CodecRegistryHandle {
        self.handle.clone()
    }
    pub fn register(&self, fourcc: FourCc, codec: Arc<dyn Codec>) {
        self.handle.insert(fourcc, codec);
    }

    /// Register codecs for `fourcc` the first time `fourcc` is looked up.
    pub fn register_deferred(&self, fourcc: FourCc, codecs: DeferredCodecs) {
        self.handle.register_deferred(fourcc, codecs);
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
#[path = "registry_tests.rs"]
mod tests;
