#![doc = include_str!("../README.md")]

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use styx_core::prelude::*;
/// Encoders/decoders share the same entry-point; the kind distinguishes behavior.
///
/// # Example
/// ```rust
/// use styx_codec::CodecKind;
///
/// let kind = CodecKind::Decoder;
/// assert_eq!(kind, CodecKind::Decoder);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum CodecKind {
    /// Encodes raw frames into compressed payloads.
    Encoder,
    /// Decodes compressed payloads into raw frames.
    Decoder,
}

/// Descriptor for a codec implementation.
///
/// # Example
/// ```rust
/// use styx_codec::{CodecDescriptor, CodecKind};
/// use styx_core::prelude::FourCc;
///
/// let desc = CodecDescriptor {
///     kind: CodecKind::Decoder,
///     input: FourCc::new(*b"MJPG"),
///     output: FourCc::new(*b"RG24"),
///     name: "mjpeg",
///     impl_name: "jpeg-decoder",
/// };
/// assert_eq!(desc.name, "mjpeg");
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct CodecDescriptor {
    /// Encoder or decoder.
    pub kind: CodecKind,
    /// Expected input FourCc.
    pub input: FourCc,
    /// Output FourCc produced.
    pub output: FourCc,
    /// Algorithm family (e.g. "mjpeg", "h264").
    pub name: &'static str,
    /// Implementation/backend identifier (e.g. "jpeg-decoder", "vaapi", "ffmpeg").
    pub impl_name: &'static str,
}

/// Unified codec trait for zero-copy processing.
///
/// # Example
/// ```rust,ignore
/// use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind};
/// use styx_core::prelude::{FourCc, FrameLease};
///
/// struct Passthrough {
///     desc: CodecDescriptor,
/// }
///
/// impl Codec for Passthrough {
///     fn descriptor(&self) -> &CodecDescriptor { &self.desc }
///     fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
///         Ok(input)
///     }
/// }
/// ```
pub trait Codec: Send + Sync + 'static {
    /// Describes what this codec expects and produces.
    fn descriptor(&self) -> &CodecDescriptor;

    /// Process a frame and return the transformed frame.
    ///
    /// Implementations should preserve plane references when possible to avoid copies.
    fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError>;
}

/// Errors emitted by codecs.
///
/// # Example
/// ```rust
/// use styx_codec::CodecError;
/// use styx_core::prelude::FourCc;
///
/// let err = CodecError::FormatMismatch {
///     expected: FourCc::new(*b"MJPG"),
///     actual: FourCc::new(*b"RG24"),
/// };
/// assert!(matches!(err, CodecError::FormatMismatch { .. }));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Input did not match the expected FourCc.
    #[error("format mismatch: expected {expected}, got {actual}")]
    FormatMismatch {
        /// Expected input FourCc.
        expected: FourCc,
        /// Actual FourCc encountered.
        actual: FourCc,
    },
    /// Codec-specific failure detail.
    #[error("codec error: {0}")]
    Codec(String),
    /// The codec accepted input but did not (yet) produce output (e.g. encoder pipeline delay).
    #[error("codec backpressure")]
    Backpressure,
}

/// Errors surfaced by the registry.
///
/// # Example
/// ```rust
/// use styx_codec::RegistryError;
/// use styx_core::prelude::FourCc;
///
/// let err = RegistryError::NotFound(FourCc::new(*b"MJPG"));
/// assert!(matches!(err, RegistryError::NotFound(_)));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// No codec registered for the requested FourCc.
    #[error("codec not registered for {0}")]
    NotFound(FourCc),
    /// Codec failed while processing.
    #[error(transparent)]
    Codec(#[from] CodecError),
}

/// Basic stats for codec processing.
///
/// # Example
/// ```rust
/// use styx_codec::CodecStats;
///
/// let stats = CodecStats::default();
/// stats.inc_processed();
/// assert_eq!(stats.processed(), 1);
/// ```
#[derive(Debug, Clone, Default)]
pub struct CodecStats {
    processed: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    backpressure: Arc<AtomicU64>,
    last_nanos: Arc<AtomicU64>,
    window: Arc<Mutex<WindowState>>,
}

#[derive(Debug, Clone)]
struct WindowState {
    samples: VecDeque<(Instant, u64)>,
    max: usize,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            max: DEFAULT_WINDOW,
        }
    }
}

const DEFAULT_WINDOW: usize = 120;

impl CodecStats {
    /// Increment processed count.
    pub fn inc_processed(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment error count.
    pub fn inc_errors(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment backpressure count.
    pub fn inc_backpressure(&self) {
        self.backpressure.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of processed frames.
    pub fn processed(&self) -> u64 {
        self.processed.load(Ordering::Relaxed)
    }

    /// Snapshot of errors.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Snapshot of backpressure events.
    pub fn backpressure(&self) -> u64 {
        self.backpressure.load(Ordering::Relaxed)
    }

    /// Samples within the current window.
    pub fn samples(&self) -> u64 {
        self.window
            .lock()
            .map(|w| w.samples.len() as u64)
            .unwrap_or(0)
    }

    /// Record a successful processing duration in nanoseconds.
    pub fn record_duration(&self, dur: Duration) {
        let nanos = dur.as_nanos().min(u64::MAX as u128) as u64;
        self.last_nanos.store(nanos, Ordering::Relaxed);
        if let Ok(mut win) = self.window.lock() {
            if win.max == 0 {
                win.max = DEFAULT_WINDOW;
            }
            win.samples.push_back((Instant::now(), nanos));
            while win.samples.len() > win.max {
                win.samples.pop_front();
            }
        }
    }

    /// Configure the rolling window size (minimum 1).
    pub fn set_window_size(&self, window: usize) {
        let window = window.max(1);
        if let Ok(mut win) = self.window.lock() {
            win.max = window;
            while win.samples.len() > win.max {
                win.samples.pop_front();
            }
        }
    }

    /// Average processing time in milliseconds, if any samples were recorded.
    pub fn avg_millis(&self) -> Option<f64> {
        self.window.lock().ok().and_then(|w| {
            let count = w.samples.len();
            if count == 0 {
                return None;
            }
            let total: u128 = w.samples.iter().map(|(_, n)| *n as u128).sum();
            Some(total as f64 / 1_000_000.0 / count as f64)
        })
    }

    /// Last processing duration in milliseconds, if any samples were recorded.
    pub fn last_millis(&self) -> Option<f64> {
        let last = self.last_nanos.load(Ordering::Relaxed);
        if last == 0 {
            None
        } else {
            Some(last as f64 / 1_000_000.0)
        }
    }

    /// Approximate frames per second based on processed frames since first sample.
    pub fn fps(&self) -> Option<f64> {
        self.window.lock().ok().and_then(|w| {
            if w.samples.len() < 2 {
                return None;
            }
            let first = w.samples.front()?.0;
            let last = w.samples.back()?.0;
            let span = last.saturating_duration_since(first).as_secs_f64();
            if span > 0.0 {
                Some(w.samples.len() as f64 / span)
            } else {
                None
            }
        })
    }
}

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

/// Thread-safe handle for codec registration/lookups.
///
/// # Example
/// ```rust,ignore
/// use std::sync::Arc;
/// use styx_codec::{Codec, CodecDescriptor, CodecError, CodecKind, CodecRegistry};
/// use styx_core::prelude::{FourCc, FrameLease};
///
/// struct Passthrough {
///     desc: CodecDescriptor,
/// }
///
/// impl Codec for Passthrough {
///     fn descriptor(&self) -> &CodecDescriptor { &self.desc }
///     fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> { Ok(input) }
/// }
///
/// let registry = CodecRegistry::new();
/// registry.register(
///     FourCc::new(*b"RG24"),
///     Arc::new(Passthrough {
///         desc: CodecDescriptor {
///             kind: CodecKind::Decoder,
///             input: FourCc::new(*b"RG24"),
///             output: FourCc::new(*b"RG24"),
///             name: "passthrough",
///             impl_name: "passthrough",
///         },
///     }),
/// );
/// let handle = registry.handle();
/// let _ = handle.lookup(FourCc::new(*b"RG24"))?;
/// # Ok::<(), styx_codec::RegistryError>(())
/// ```
#[derive(Clone)]
pub struct CodecRegistryHandle {
    inner: Arc<RwLock<RegistryInner>>,
    stats: CodecStats,
}

impl CodecRegistryHandle {
    /// Lookup a codec by FourCc.
    pub fn lookup(&self, fourcc: FourCc) -> Result<Arc<dyn Codec>, RegistryError> {
        let guard = self.inner.read().unwrap();
        guard
            .codecs
            .get(&fourcc)
            .and_then(|v| v.first().cloned())
            .ok_or(RegistryError::NotFound(fourcc))
    }

    /// Lookup a codec by FourCc and implementation name.
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

    /// Lookup a codec by FourCc, kind, and implementation name.
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

    /// Lookup a codec by FourCc honoring an ordered list of preferred impl names and hardware preference.
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

    /// Find a codec by implementation name and kind across all FourCc entries.
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

    /// Process a frame with the registered codec.
    pub fn process(&self, fourcc: FourCc, frame: FrameLease) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup(fourcc)?;
        self.run_codec(start, codec, frame)
    }

    /// Process with a specific implementation name.
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

    /// Process with an ordered implementation preference and optional hardware bias.
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

    /// Configure preferences for a FourCc (impl order + hardware bias).
    pub fn set_preference(&self, fourcc: FourCc, preference: Preference) {
        let mut guard = self.inner.write().unwrap();
        guard.preferences.insert(fourcc, preference);
    }

    /// Disable a codec implementation by impl_name for the given FourCc.
    pub fn disable_impl(&self, fourcc: FourCc, impl_name: &str) {
        let mut guard = self.inner.write().unwrap();
        if let Some(list) = guard.codecs.get_mut(&fourcc) {
            list.retain(|c| !c.descriptor().impl_name.eq_ignore_ascii_case(impl_name));
        }
    }

    /// Enable only the listed impl_names for a FourCc (removes others).
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

    /// Dynamically register a new codec impl at runtime.
    pub fn register_dynamic(&self, fourcc: FourCc, codec: Arc<dyn Codec>) {
        let mut guard = self.inner.write().unwrap();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        let list = guard.codecs.entry(fourcc).or_default();
        list.push(codec);
        sort_backends_for(&priorities, prefer_hw, fourcc, list);
    }

    /// Assign an explicit priority for an impl name (lower wins).
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

    /// Toggle the default bias toward hardware implementations.
    pub fn set_default_hardware_bias(&self, prefer: bool) {
        let mut guard = self.inner.write().unwrap();
        guard.default_prefer_hardware = prefer;
    }

    /// Install a full policy for a FourCc.
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

    /// Lookup honoring stored preferences when present.
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

        // Apply explicit preference first.
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

        // Fall back to priority + default hardware bias.
        let impl_prio = &guard.impl_priority;
        let best = list
            .iter()
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
            .cloned();

        best.ok_or(RegistryError::NotFound(fourcc))
    }

    /// Lookup honoring stored preferences when present, constrained to a codec kind.
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
        let best = list
            .iter()
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
            .cloned();

        best.cloned().ok_or(RegistryError::NotFound(fourcc))
    }

    /// Lookup honoring stored preferences, constrained to a codec kind and algorithm family name.
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
            .filter(|c| c.descriptor().kind == kind && c.descriptor().name.eq_ignore_ascii_case(codec_name))
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
        let best = list
            .iter()
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
            .cloned();

        best.cloned().ok_or(RegistryError::NotFound(fourcc))
    }

    /// Process honoring stored preferences when present.
    pub fn process_auto(
        &self,
        fourcc: FourCc,
        frame: FrameLease,
    ) -> Result<FrameLease, RegistryError> {
        let start = Instant::now();
        let codec = self.lookup_auto(fourcc)?;
        self.run_codec(start, codec, frame)
    }

    /// Process honoring stored preferences when present, constrained to a codec kind.
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

    /// Process honoring stored preferences, constrained to a codec kind and algorithm family name.
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

    /// Stats snapshot.
    pub fn stats(&self) -> CodecStats {
        self.stats.clone()
    }

    /// Snapshot of all registered codecs grouped by FourCc (descriptor-only).
    pub fn list_registered(&self) -> Vec<(FourCc, Vec<CodecDescriptor>)> {
        let guard = self.inner.read().unwrap();
        guard
            .codecs
            .iter()
            .map(|(fourcc, list)| {
                let descs = list.iter().map(|c| c.descriptor().clone()).collect();
                (*fourcc, descs)
            })
            .collect()
    }

    /// List registered codecs filtered by kind.
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
}

impl CodecRegistryHandle {
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
                if matches!(err, CodecError::Backpressure) {
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

/// Registry used to install codecs.
///
/// # Example
/// ```rust,ignore
/// use styx_codec::CodecRegistry;
///
/// let registry = CodecRegistry::new();
/// let handle = registry.handle();
/// let _ = handle.list_registered();
/// ```
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
    /// Create an empty registry.
    pub fn new() -> Self {
        let inner = RegistryInner::new();
        let handle = CodecRegistryHandle {
            inner: Arc::new(RwLock::new(inner)),
            stats: CodecStats::default(),
        };
        Self { handle }
    }

    /// Obtain a clonable handle.
    pub fn handle(&self) -> CodecRegistryHandle {
        self.handle.clone()
    }

    /// Register a codec implementation.
    pub fn register(&self, fourcc: FourCc, codec: Arc<dyn Codec>) {
        let mut guard = self.handle.inner.write().unwrap();
        let priorities = guard.impl_priority.clone();
        let prefer_hw = guard.default_prefer_hardware;
        let list = guard.codecs.entry(fourcc).or_default();
        list.push(codec);
        sort_backends_for(&priorities, prefer_hw, fourcc, list);
    }

    /// Create a registry pre-populated with codecs enabled for the current build.
    pub fn with_enabled_codecs() -> Result<Self, CodecError> {
        Self::with_enabled_codecs_for_max(DEFAULT_CODEC_MAX_WIDTH, DEFAULT_CODEC_MAX_HEIGHT)
    }

    /// Create a registry and register built-ins using a suggested max frame size (for buffer pools).
    pub fn with_enabled_codecs_for_max(
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, CodecError> {
        let registry = Self::new();
        registry.register_enabled_codecs(max_width, max_height)?;
        Ok(registry)
    }

    /// Register codecs that are available under the current feature set using default pool sizing.
    pub fn register_enabled_codecs_default(&self) -> Result<(), CodecError> {
        self.register_enabled_codecs(DEFAULT_CODEC_MAX_WIDTH, DEFAULT_CODEC_MAX_HEIGHT)
    }

    /// Register codecs that are available under the current feature set.
    pub fn register_enabled_codecs(
        &self,
        max_width: u32,
        max_height: u32,
    ) -> Result<(), CodecError> {
        let max_width = max_width.max(1);
        let max_height = max_height.max(1);

        // Core decoders.
        self.register(
            FourCc::new(*b"MJPG"),
            Arc::new(mjpeg::MjpegDecoder::new(FourCc::new(*b"RG24"))),
        );
        // Some V4L2 stacks report JPEG-coded streams as `JPEG` instead of `MJPG`.
        self.register(
            FourCc::new(*b"JPEG"),
            Arc::new(mjpeg::MjpegDecoder::new_for_input(
                FourCc::new(*b"JPEG"),
                FourCc::new(*b"RG24"),
            )),
        );
        self.register(
            FourCc::new(*b"BGR3"),
            Arc::new(decoder::raw::BgrToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"BGRA"),
            Arc::new(decoder::raw::BgraToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"RGBA"),
            Arc::new(decoder::raw::RgbaToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"YUYV"),
            Arc::new(decoder::raw::YuyvToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"YUYV"),
            Arc::new(decoder::raw::YuyvToLumaDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"NV12"),
            Arc::new(decoder::raw::Nv12ToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"I420"),
            Arc::new(decoder::raw::I420ToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"YU12"),
            Arc::new(decoder::raw::Yuv420pToRgbDecoder::new(
                FourCc::new(*b"YU12"),
                "yu12-cpu",
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YV12"),
            Arc::new(decoder::raw::Yuv420pToRgbDecoder::new(
                FourCc::new(*b"YV12"),
                "yv12-cpu",
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"R8  "),
            Arc::new(decoder::raw::Mono8ToRgbDecoder::new(max_width, max_height)),
        );
        self.register(
            FourCc::new(*b"R16 "),
            Arc::new(decoder::raw::Mono16ToRgbDecoder::new(max_width, max_height)),
        );

        // Additional YUV/NV decoders.
        self.register(
            FourCc::new(*b"NV21"),
            Arc::new(decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV21"),
                "nv21-cpu",
                2,
                2,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV16"),
            Arc::new(decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV16"),
                "nv16-cpu",
                2,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV61"),
            Arc::new(decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV61"),
                "nv61-cpu",
                2,
                1,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV24"),
            Arc::new(decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV24"),
                "nv24-cpu",
                1,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"NV42"),
            Arc::new(decoder::raw::NvToRgbDecoder::new(
                FourCc::new(*b"NV42"),
                "nv42-cpu",
                1,
                1,
                false,
                max_width,
                max_height,
            )),
        );

        // Common RGB passthroughs (already in target ordering).
        for code in [*b"RG24", *b"RGB3", *b"RGB6"] {
            self.register(
                FourCc::new(code),
                Arc::new(decoder::raw::PassthroughDecoder::new(FourCc::new(code))),
            );
        }

        // Bayer demosaic.
        let bayer_codes = [
            *b"BA81", *b"BA10", *b"BA12", *b"BA14", *b"BG10", *b"BG12", *b"BG14", *b"BG16",
            *b"GB10", *b"GB12", *b"GB14", *b"GB16", *b"RG10", *b"RG12", *b"RG14", *b"RG16",
            *b"GR10", *b"GR12", *b"GR14", *b"GR16", *b"BYR2", *b"RGGB", *b"GRBG", *b"GBRG",
            *b"BGGR", // MIPI packed RAW10/RAW12 (V4L2_PIX_FMT_S*10P / S*12P).
            *b"pBAA", *b"pGAA", *b"pgAA", *b"pRAA", *b"pBCC", *b"pGCC", *b"pgCC", *b"pRCC",
        ];
        for code in bayer_codes {
            let fcc = FourCc::new(code);
            if let Some(info) = decoder::raw::bayer_info(fcc) {
                self.register(
                    fcc,
                    decoder::raw::bayer_decoder_for(fcc, info, max_width, max_height),
                );
            }
        }

        self.register(
            FourCc::new(*b"YV12"),
            Arc::new(decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YV12"),
                "yv12-cpu",
                2,
                2,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YU16"),
            Arc::new(decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YU16"),
                "yu16-cpu",
                2,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YV16"),
            Arc::new(decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YV16"),
                "yv16-cpu",
                2,
                1,
                false,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YU24"),
            Arc::new(decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YU24"),
                "yu24-cpu",
                1,
                1,
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"YV24"),
            Arc::new(decoder::raw::PlanarYuvToRgbDecoder::new(
                FourCc::new(*b"YV24"),
                "yv24-cpu",
                1,
                1,
                false,
                max_width,
                max_height,
            )),
        );

        self.register(
            FourCc::new(*b"YVYU"),
            Arc::new(decoder::raw::Packed422ToRgbDecoder::new(
                FourCc::new(*b"YVYU"),
                "yvyu-cpu",
                [0, 3, 2, 1],
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"UYVY"),
            Arc::new(decoder::raw::Packed422ToRgbDecoder::new(
                FourCc::new(*b"UYVY"),
                "uyvy-cpu",
                [1, 0, 3, 2],
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"VYUY"),
            Arc::new(decoder::raw::Packed422ToRgbDecoder::new(
                FourCc::new(*b"VYUY"),
                "vyuy-cpu",
                [1, 2, 3, 0],
                max_width,
                max_height,
            )),
        );

        // Channel swap/strip variants.
        self.register(
            FourCc::new(*b"BG24"),
            Arc::new(decoder::raw::BgrToRgbDecoder::with_input_for_max(
                FourCc::new(*b"BG24"),
                "bg24-swap",
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"XB24"),
            Arc::new(decoder::raw::BgraToRgbDecoder::with_input_for_max(
                FourCc::new(*b"XB24"),
                "xb24-strip",
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"XR24"),
            Arc::new(decoder::raw::RgbaToRgbDecoder::with_input_for_max(
                FourCc::new(*b"XR24"),
                "xr24-strip",
                max_width,
                max_height,
            )),
        );

        // 16-bit RGB/BGR converters.
        self.register(
            FourCc::new(*b"BG48"),
            Arc::new(decoder::raw::Rgb48ToRgbDecoder::new(
                FourCc::new(*b"BG48"),
                "bg48-strip",
                true,
                max_width,
                max_height,
            )),
        );
        self.register(
            FourCc::new(*b"RG48"),
            Arc::new(decoder::raw::Rgb48ToRgbDecoder::new(
                FourCc::new(*b"RG48"),
                "rg48-strip",
                false,
                max_width,
                max_height,
            )),
        );

        // Optional image crate decoder.
        #[cfg(feature = "image")]
        self.register(
            FourCc::new(*b"ANY "),
            Arc::new(image_any::ImageAnyDecoder::new(FourCc::new(*b"RGBA"))),
        );

        // Optional FFmpeg codecs.
        #[cfg(feature = "codec-ffmpeg")]
        {
            use crate::ffmpeg::{
                FfmpegEncoderOptions, FfmpegH264Decoder, FfmpegH264Encoder, FfmpegH265Decoder,
                FfmpegH265Encoder, FfmpegMjpegDecoder, FfmpegMjpegEncoder,
            };
            let default_decoder_threads = std::env::var("STYX_FFMPEG_DECODER_THREADS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0);
            let mjpeg_mjpg = Arc::new(FfmpegMjpegDecoder::with_options_for_input(
                FourCc::new(*b"MJPG"),
                false,
                default_decoder_threads,
                None,
            )?);
            self.register(FourCc::new(*b"MJPG"), mjpeg_mjpg);
            // Some V4L2 stacks report JPEG-coded streams as `JPEG` instead of `MJPG`.
            let mjpeg_jpeg = Arc::new(FfmpegMjpegDecoder::with_options_for_input(
                FourCc::new(*b"JPEG"),
                false,
                default_decoder_threads,
                None,
            )?);
            self.register(FourCc::new(*b"JPEG"), mjpeg_jpeg);
            self.register(
                FourCc::new(*b"H264"),
                Arc::new(FfmpegH264Decoder::with_options(
                    false,
                    default_decoder_threads,
                    None,
                )?),
            );
            if let Ok(dec) = FfmpegH264Decoder::new_v4l2request_nv12_zero_copy() {
                self.register(FourCc::new(*b"H264"), Arc::new(dec));
            }
            let h265 = Arc::new(FfmpegH265Decoder::with_options_for_input(
                FourCc::new(*b"H265"),
                false,
                default_decoder_threads,
                None,
            )?);
            self.register(FourCc::new(*b"H265"), h265);
            // V4L2 uses `HEVC` FourCC for H.265.
            let hevc = Arc::new(FfmpegH265Decoder::with_options_for_input(
                FourCc::new(*b"HEVC"),
                false,
                default_decoder_threads,
                None,
            )?);
            self.register(FourCc::new(*b"HEVC"), hevc);
            if let Ok(dec) = FfmpegH265Decoder::new_v4l2request_nv12_zero_copy() {
                let dec = Arc::new(dec);
                self.register(FourCc::new(*b"H265"), dec.clone());
                // Note: the v4l2request decoder advertises `H265` input. Registering it under `HEVC`
                // would cause a format mismatch if upstream frames are labeled as `HEVC`.
            }
            self.register(
                FourCc::new(*b"RG24"),
                Arc::new(FfmpegMjpegEncoder::new_rgb24()?),
            );
            self.register(
                FourCc::new(*b"NV12"),
                Arc::new(FfmpegMjpegEncoder::new_nv12()?),
            );
            self.register(
                FourCc::new(*b"YUYV"),
                Arc::new(FfmpegMjpegEncoder::with_options_for_input(
                    FourCc::new(*b"YUYV"),
                    FfmpegEncoderOptions::default(),
                )?),
            );
            self.register(
                FourCc::new(*b"RG24"),
                Arc::new(FfmpegH264Encoder::new_rgb24()?),
            );
            self.register(
                FourCc::new(*b"NV12"),
                Arc::new(FfmpegH264Encoder::new_nv12()?),
            );
            if let Ok(enc) = FfmpegH264Encoder::new_v4l2m2m_rgb24() {
                self.register(FourCc::new(*b"RG24"), Arc::new(enc));
            }
            if let Ok(enc) = FfmpegH264Encoder::new_v4l2m2m_nv12() {
                self.register(FourCc::new(*b"NV12"), Arc::new(enc));
            }
            self.register(
                FourCc::new(*b"YUYV"),
                Arc::new(FfmpegH264Encoder::with_options_for_input(
                    FourCc::new(*b"YUYV"),
                    FfmpegEncoderOptions::default(),
                )?),
            );
            self.register(
                FourCc::new(*b"RG24"),
                Arc::new(FfmpegH265Encoder::new_rgb24()?),
            );
            self.register(
                FourCc::new(*b"NV12"),
                Arc::new(FfmpegH265Encoder::new_nv12()?),
            );
            if let Ok(enc) = FfmpegH265Encoder::new_v4l2m2m_rgb24() {
                self.register(FourCc::new(*b"RG24"), Arc::new(enc));
            }
            if let Ok(enc) = FfmpegH265Encoder::new_v4l2m2m_nv12() {
                self.register(FourCc::new(*b"NV12"), Arc::new(enc));
            }
            self.register(
                FourCc::new(*b"YUYV"),
                Arc::new(FfmpegH265Encoder::with_options_for_input(
                    FourCc::new(*b"YUYV"),
                    FfmpegEncoderOptions::default(),
                )?),
            );
        }

        // Optional mozjpeg encoder.
        #[cfg(feature = "codec-mozjpeg")]
        self.register(
            FourCc::new(*b"RG24"),
            Arc::new(jpeg_encoder::MozjpegEncoder::new(FourCc::new(*b"RG24"), 85)),
        );

        // Optional turbojpeg decoder.
        #[cfg(feature = "codec-turbojpeg")]
        self.register(
            FourCc::new(*b"MJPG"),
            Arc::new(mjpeg_turbojpeg::TurbojpegDecoder::new(FourCc::new(
                *b"RG24",
            ))),
        );

        // Optional zune-jpeg decoder.
        #[cfg(feature = "codec-zune")]
        self.register(
            FourCc::new(*b"MJPG"),
            Arc::new(mjpeg_zune::ZuneMjpegDecoder::new(FourCc::new(*b"RG24"))),
        );

        Ok(())
    }

    /// List codec descriptors for the currently enabled set without requiring callers to register manually.
    pub fn list_enabled_codecs() -> Result<Vec<(FourCc, Vec<CodecDescriptor>)>, CodecError> {
        let registry = CodecRegistry::with_enabled_codecs()?;
        Ok(registry.handle.list_registered())
    }

    /// List enabled decoders for the current build (descriptors only).
    pub fn list_enabled_decoders() -> Result<Vec<(FourCc, Vec<CodecDescriptor>)>, CodecError> {
        let registry = CodecRegistry::with_enabled_codecs()?;
        Ok(registry.handle.list_registered_by_kind(CodecKind::Decoder))
    }

    /// List enabled encoders for the current build (descriptors only).
    pub fn list_enabled_encoders() -> Result<Vec<(FourCc, Vec<CodecDescriptor>)>, CodecError> {
        let registry = CodecRegistry::with_enabled_codecs()?;
        Ok(registry.handle.list_registered_by_kind(CodecKind::Encoder))
    }
}

/// Preference for selecting codecs by FourCc.
///
/// # Example
/// ```rust
/// use styx_codec::Preference;
///
/// let pref = Preference::hardware_biased(vec!["ffmpeg".into()]);
/// assert!(pref.prefer_hardware);
/// ```
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct Preference {
    /// Ordered list of impl names to prefer.
    pub impls: Vec<String>,
    /// Whether to favor hardware-accelerated impls.
    pub prefer_hardware: bool,
}

impl Preference {
    pub fn hardware_biased(impls: Vec<String>) -> Self {
        Self {
            impls,
            prefer_hardware: true,
        }
    }
}

/// Typed policy for choosing codecs (impl priorities + hardware bias).
///
/// # Example
/// ```rust
/// use styx_codec::CodecPolicy;
/// use styx_core::prelude::FourCc;
///
/// let policy = CodecPolicy::builder(FourCc::new(*b"MJPG"))
///     .prefer_hardware(false)
///     .build();
/// ```
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct CodecPolicy {
    fourcc: FourCc,
    prefer_hardware: bool,
    ordered_impls: Vec<String>,
    priorities: std::collections::HashMap<String, i32>,
}

impl CodecPolicy {
    /// Builder entry-point.
    pub fn builder(fourcc: FourCc) -> CodecPolicyBuilder {
        CodecPolicyBuilder {
            fourcc,
            prefer_hardware: true,
            ordered_impls: Vec::new(),
            priorities: std::collections::HashMap::new(),
        }
    }
}

/// Builder for codec selection policy.
///
/// # Example
/// ```rust
/// use styx_codec::CodecPolicy;
/// use styx_core::prelude::FourCc;
///
/// let policy = CodecPolicy::builder(FourCc::new(*b"MJPG"))
///     .ordered_impls(["ffmpeg", "jpeg-decoder"])
///     .priority("ffmpeg", 0)
///     .build();
/// ```
pub struct CodecPolicyBuilder {
    fourcc: FourCc,
    prefer_hardware: bool,
    ordered_impls: Vec<String>,
    priorities: std::collections::HashMap<String, i32>,
}

impl CodecPolicyBuilder {
    /// Disable or enable hardware bias.
    pub fn prefer_hardware(mut self, prefer: bool) -> Self {
        self.prefer_hardware = prefer;
        self
    }

    /// Explicit ordered implementations to try first.
    pub fn ordered_impls<I, S>(mut self, impls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ordered_impls = impls.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Set an integer priority for an impl name (lower wins).
    pub fn priority<S: Into<String>>(mut self, impl_name: S, priority: i32) -> Self {
        self.priorities
            .insert(impl_name.into().to_ascii_lowercase(), priority);
        self
    }

    pub fn build(self) -> CodecPolicy {
        CodecPolicy {
            fourcc: self.fourcc,
            prefer_hardware: self.prefer_hardware,
            ordered_impls: self.ordered_impls,
            priorities: self.priorities,
        }
    }
}

pub mod decoder;
pub mod encoder;
#[cfg(feature = "codec-ffmpeg")]
pub mod ffmpeg;
#[cfg(feature = "image")]
pub mod image_any;
#[cfg(feature = "image")]
pub mod image_utils;
#[cfg(feature = "codec-mozjpeg")]
pub mod jpeg_encoder;
pub mod mjpeg;
#[cfg(feature = "codec-turbojpeg")]
pub mod mjpeg_turbojpeg;
#[cfg(feature = "codec-zune")]
pub mod mjpeg_zune;

pub mod prelude {
    pub use crate::decoder::raw::{
        BgrToRgbDecoder, BgraToRgbDecoder, I420ToRgbDecoder, Nv12ToBgrDecoder, Nv12ToRgbDecoder,
        PassthroughDecoder, RgbaToRgbDecoder, YuyvToRgbDecoder,
    };
    #[cfg(feature = "codec-ffmpeg")]
    pub use crate::ffmpeg::{
        FfmpegH264Decoder, FfmpegH264Encoder, FfmpegH265Decoder, FfmpegH265Encoder,
        FfmpegMjpegDecoder, FfmpegMjpegEncoder,
    };
    #[cfg(feature = "image")]
    pub use crate::image_any::ImageAnyDecoder;
    #[cfg(feature = "image")]
    pub use crate::image_utils::{CodecImageExt, dynamic_image_to_frame};
    #[cfg(feature = "codec-mozjpeg")]
    pub use crate::jpeg_encoder::MozjpegEncoder;
    #[cfg(feature = "codec-turbojpeg")]
    pub use crate::mjpeg_turbojpeg::TurbojpegDecoder;
    #[cfg(feature = "codec-zune")]
    pub use crate::mjpeg_zune::ZuneMjpegDecoder;
    pub use crate::{
        Codec, CodecDescriptor, CodecError, CodecKind, CodecPolicy, CodecPolicyBuilder,
        CodecRegistry, CodecRegistryHandle, CodecStats, RegistryError, mjpeg::MjpegDecoder,
    };
    pub use styx_capture::prelude::*;
    #[allow(unused_imports)]
    pub use styx_core::prelude::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Rg24Passthrough {
        descriptor: CodecDescriptor,
    }

    impl Default for Rg24Passthrough {
        fn default() -> Self {
            Self {
                descriptor: CodecDescriptor {
                    kind: CodecKind::Encoder,
                    input: FourCc::new(*b"RG24"),
                    output: FourCc::new(*b"RG24"),
                    name: "passthrough",
                    impl_name: "test",
                },
            }
        }
    }

    impl Codec for Rg24Passthrough {
        fn descriptor(&self) -> &CodecDescriptor {
            &self.descriptor
        }

        fn process(&self, input: FrameLease) -> Result<FrameLease, CodecError> {
            if input.meta().format.code != self.descriptor.input {
                return Err(CodecError::FormatMismatch {
                    expected: self.descriptor.input,
                    actual: input.meta().format.code,
                });
            }
            Ok(input)
        }
    }

    #[test]
    fn auto_converts_rgba_to_rg24_for_rg24_codecs() {
        let registry = CodecRegistry::with_enabled_codecs_for_max(8, 8).expect("registry");
        registry.register(FourCc::new(*b"RG24"), Arc::new(Rg24Passthrough::default()));
        let handle = registry.handle();

        let res = Resolution::new(2, 2).unwrap();
        let layout = plane_layout_from_dims(res.width, res.height, 4);
        let pool = BufferPool::with_limits(1, layout.len, 4);
        let mut buf = pool.lease();
        buf.resize(layout.len);
        for (i, b) in buf.as_mut_slice().iter_mut().enumerate() {
            *b = i as u8;
        }
        let format = MediaFormat::new(FourCc::new(*b"RGBA"), res, ColorSpace::Srgb);
        let frame =
            FrameLease::single_plane(FrameMeta::new(format, 0), buf, layout.len, layout.stride);

        let out = handle
            .process_named(FourCc::new(*b"RG24"), "test", frame)
            .expect("process");
        assert_eq!(out.meta().format.code, FourCc::new(*b"RG24"));
        assert_eq!(out.meta().format.resolution.width.get(), 2);
        assert_eq!(out.meta().format.resolution.height.get(), 2);

        let data = out.planes().first().unwrap().data();
        assert_eq!(data.len(), 2 * 2 * 3);

        let expected: Vec<u8> = (0u8..16)
            .collect::<Vec<_>>()
            .chunks_exact(4)
            .flat_map(|px| px[..3].iter().copied())
            .collect();
        assert_eq!(data, expected.as_slice());
    }
}
