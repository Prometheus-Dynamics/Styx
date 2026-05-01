use styx_core::prelude::FourCc;

use crate::CodecImplementationId;

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct Preference {
    pub impls: Vec<CodecImplementationId>,
    pub prefer_hardware: bool,
}

impl Preference {
    pub fn hardware_biased<I, S>(impls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<CodecImplementationId>,
    {
        Self {
            impls: impls.into_iter().map(Into::into).collect(),
            prefer_hardware: true,
        }
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct CodecPolicy {
    pub(crate) fourcc: FourCc,
    pub(crate) prefer_hardware: bool,
    pub(crate) ordered_impls: Vec<CodecImplementationId>,
    pub(crate) priorities: std::collections::HashMap<CodecImplementationId, i32>,
}

impl CodecPolicy {
    pub fn builder(fourcc: FourCc) -> CodecPolicyBuilder {
        CodecPolicyBuilder {
            fourcc,
            prefer_hardware: true,
            ordered_impls: Vec::new(),
            priorities: std::collections::HashMap::new(),
        }
    }
}

pub struct CodecPolicyBuilder {
    fourcc: FourCc,
    prefer_hardware: bool,
    ordered_impls: Vec<CodecImplementationId>,
    priorities: std::collections::HashMap<CodecImplementationId, i32>,
}

impl CodecPolicyBuilder {
    pub fn prefer_hardware(mut self, prefer: bool) -> Self {
        self.prefer_hardware = prefer;
        self
    }

    pub fn ordered_impls<I, S>(mut self, impls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<CodecImplementationId>,
    {
        self.ordered_impls = impls.into_iter().map(Into::into).collect();
        self
    }

    pub fn priority<S: Into<CodecImplementationId>>(mut self, impl_name: S, priority: i32) -> Self {
        let impl_id = impl_name.into();
        self.priorities.insert(impl_id, priority);
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
