//! Choosing an implementation for a request: native first, then FFmpeg, with every reason kept.

use std::fmt;

use styx_format::FourCc;

use crate::codec::{CodecDescriptor, CodecError, CodecKind, Implementation};

/// Builds one implementation. Called at most once per [`Registry::select`]: only for a candidate
/// whose descriptor matches the request, and only until one candidate constructs.
pub type Constructor<M> =
    Box<dyn Fn() -> Result<Implementation<M>, CodecError> + Send + Sync + 'static>;

/// A registrable implementation: what it is ([`CodecDescriptor`]) and how to build it.
///
/// A candidate that cannot run here says so from its constructor (`CodecError::Unavailable`), and
/// the registry moves on to the next one.
pub struct Candidate<M = ()> {
    descriptor: CodecDescriptor,
    construct: Constructor<M>,
}

impl<M> Candidate<M> {
    pub fn new(
        descriptor: CodecDescriptor,
        construct: impl Fn() -> Result<Implementation<M>, CodecError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            descriptor,
            construct: Box::new(construct),
        }
    }

    pub fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

impl<M> fmt::Debug for Candidate<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Candidate")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

/// What a caller asks for: a kind, and the formats it converts from and to. An encoder is
/// `Encoder` from a raw format to a compressed one; a decoder the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Request {
    pub kind: CodecKind,
    pub input: FourCc,
    pub output: FourCc,
}

impl Request {
    pub const fn new(kind: CodecKind, input: FourCc, output: FourCc) -> Self {
        Self {
            kind,
            input,
            output,
        }
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} to {}", self.kind, self.input, self.output)
    }
}

/// What happened to one candidate during a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// This one was built and is the selection.
    Chosen,
    /// Not used, and why: the kind or formats differ, or it could not be built.
    Skipped(String),
    /// Never looked at, because an earlier candidate was chosen.
    NotTried,
}

/// One candidate and what the selection did with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub descriptor: CodecDescriptor,
    pub verdict: Verdict,
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.descriptor;
        write!(f, "{} {} ({})", d.backend, d.name, d.kind)?;
        match &self.verdict {
            Verdict::Chosen => f.write_str(": chosen"),
            Verdict::Skipped(why) => write!(f, ": skipped, {why}"),
            Verdict::NotTried => f.write_str(": not tried"),
        }
    }
}

/// The chosen implementation, and the decision for every candidate in the registry, in the order
/// they were considered.
pub struct Selection<M = ()> {
    implementation: Implementation<M>,
    decisions: Vec<Decision>,
}

impl<M> Selection<M> {
    pub fn implementation(&self) -> &Implementation<M> {
        &self.implementation
    }

    pub fn into_implementation(self) -> Implementation<M> {
        self.implementation
    }

    pub fn decisions(&self) -> &[Decision] {
        &self.decisions
    }
}

impl<M> fmt::Debug for Selection<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Selection")
            .field("implementation", &self.implementation)
            .field("decisions", &self.decisions)
            .finish()
    }
}

/// No candidate could serve the request. The message names the formats; the decisions say why
/// each candidate was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectError {
    pub request: Request,
    pub decisions: Vec<Decision>,
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no {} available", self.request)?;
        if self.decisions.is_empty() {
            return f.write_str(": none registered");
        }
        f.write_str(":")?;
        for decision in &self.decisions {
            write!(f, " [{decision}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for SelectError {}

/// The candidates for codecs, tried in order: native first, FFmpeg after; registration order
/// within a backend.
pub struct Registry<M = ()> {
    candidates: Vec<Candidate<M>>,
}

impl<M> Default for Registry<M> {
    fn default() -> Self {
        Self {
            candidates: Vec::new(),
        }
    }
}

impl<M> fmt::Debug for Registry<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("candidates", &self.candidates)
            .finish()
    }
}

impl<M> Registry<M> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, candidate: Candidate<M>) {
        self.candidates.push(candidate);
    }

    pub fn candidates(&self) -> &[Candidate<M>] {
        &self.candidates
    }

    /// Chooses the first candidate that matches `request` and builds.
    ///
    /// Candidates are considered native first, FFmpeg after, each backend in registration order.
    /// A candidate is skipped (with the reason) if its kind or formats differ, if its constructor
    /// fails, or if it builds something other than its descriptor says. The first that builds is
    /// chosen; the rest are reported as not tried. Construction only happens for matching
    /// candidates.
    pub fn select(&self, request: Request) -> Result<Selection<M>, SelectError> {
        let mut order: Vec<usize> = (0..self.candidates.len()).collect();
        order.sort_by_key(|&index| (self.candidates[index].descriptor.backend.rank(), index));

        let mut decisions = Vec::with_capacity(order.len());
        let mut chosen = None;
        for index in order {
            let candidate = &self.candidates[index];
            let descriptor = candidate.descriptor.clone();
            if chosen.is_some() {
                decisions.push(Decision {
                    descriptor,
                    verdict: Verdict::NotTried,
                });
                continue;
            }
            let verdict = match mismatch(&candidate.descriptor, &request) {
                Some(why) => Verdict::Skipped(why),
                None => match (candidate.construct)() {
                    Ok(implementation) if *implementation.descriptor() == candidate.descriptor => {
                        chosen = Some(implementation);
                        Verdict::Chosen
                    }
                    Ok(implementation) => Verdict::Skipped(format!(
                        "its constructor built {} instead",
                        implementation.descriptor().name
                    )),
                    Err(error) => Verdict::Skipped(error.to_string()),
                },
            };
            decisions.push(Decision {
                descriptor,
                verdict,
            });
        }

        match chosen {
            Some(implementation) => Ok(Selection {
                implementation,
                decisions,
            }),
            None => Err(SelectError { request, decisions }),
        }
    }
}

/// Why a descriptor does not serve a request, if it does not.
fn mismatch(descriptor: &CodecDescriptor, request: &Request) -> Option<String> {
    if descriptor.kind != request.kind {
        return Some(format!(
            "its kind is {}, not {}",
            descriptor.kind, request.kind
        ));
    }
    if descriptor.input != request.input || descriptor.output != request.output {
        return Some(format!(
            "it converts {} to {}, not {} to {}",
            descriptor.input, descriptor.output, request.input, request.output
        ));
    }
    None
}
