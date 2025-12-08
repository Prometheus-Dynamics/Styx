/// Strongly typed control identifier.
///
/// # Example
/// ```rust
/// use styx_core::prelude::ControlId;
///
/// let id = ControlId(42);
/// assert_eq!(id.0, 42);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = u32))]
pub struct ControlId(pub u32);

/// Access permissions for a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// Simplified control metadata.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{Access, ControlId, ControlKind, ControlMeta, ControlValue};
///
/// let meta = ControlMeta {
///     id: ControlId(1),
///     name: "gain".into(),
///     kind: ControlKind::Uint,
///     access: Access::ReadWrite,
///     min: ControlValue::Uint(0),
///     max: ControlValue::Uint(255),
///     default: ControlValue::Uint(16),
///     step: Some(ControlValue::Uint(1)),
///     menu: None,
/// };
/// assert!(meta.validate(&ControlValue::Uint(32)));
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ControlMeta {
    /// Stable identifier.
    pub id: ControlId,
    /// Human-readable name.
    pub name: String,
    /// Kind of control/value type.
    pub kind: ControlKind,
    /// Access permissions.
    pub access: Access,
    /// Minimum accepted value.
    pub min: ControlValue,
    /// Maximum accepted value.
    pub max: ControlValue,
    /// Default value.
    pub default: ControlValue,
    /// Optional step size for ranged controls.
    #[cfg_attr(feature = "serde", serde(default))]
    pub step: Option<ControlValue>,
    /// Optional enumerated menu entries (for menu controls).
    #[cfg_attr(feature = "serde", serde(default))]
    pub menu: Option<Vec<String>>,
}

/// Control kind/type metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum ControlKind {
    None,
    Bool,
    Int,
    Uint,
    Float,
    Menu,
    IntMenu,
    Unknown,
}

/// Control value variants with minimal footprint.
///
/// # Example
/// ```rust
/// use styx_core::prelude::ControlValue;
///
/// let v = ControlValue::Bool(true);
/// assert_eq!(v, ControlValue::Bool(true));
/// ```
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum ControlValue {
    /// No value.
    None,
    /// Boolean value.
    Bool(bool),
    /// Signed integer.
    Int(i32),
    /// Unsigned integer.
    Uint(u32),
    /// Floating-point value.
    Float(f32),
}

#[cfg(feature = "serde")]
impl serde::Serialize for ControlValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            #[derive(serde::Serialize)]
            #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
            enum Human {
                None,
                Bool(bool),
                Int(i32),
                Uint(u32),
                Float(f32),
            }
            let h = match self {
                ControlValue::None => Human::None,
                ControlValue::Bool(v) => Human::Bool(*v),
                ControlValue::Int(v) => Human::Int(*v),
                ControlValue::Uint(v) => Human::Uint(*v),
                ControlValue::Float(v) => Human::Float(*v),
            };
            h.serialize(serializer)
        } else {
            #[derive(serde::Serialize)]
            enum Binary {
                None,
                Bool(bool),
                Int(i32),
                Uint(u32),
                Float(f32),
            }
            let b = match self {
                ControlValue::None => Binary::None,
                ControlValue::Bool(v) => Binary::Bool(*v),
                ControlValue::Int(v) => Binary::Int(*v),
                ControlValue::Uint(v) => Binary::Uint(*v),
                ControlValue::Float(v) => Binary::Float(*v),
            };
            b.serialize(serializer)
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for ControlValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            #[derive(serde::Deserialize)]
            #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
            enum Human {
                None,
                Bool(bool),
                Int(i32),
                Uint(u32),
                Float(f32),
            }
            let h = Human::deserialize(deserializer)?;
            Ok(match h {
                Human::None => ControlValue::None,
                Human::Bool(v) => ControlValue::Bool(v),
                Human::Int(v) => ControlValue::Int(v),
                Human::Uint(v) => ControlValue::Uint(v),
                Human::Float(v) => ControlValue::Float(v),
            })
        } else {
            #[derive(serde::Deserialize)]
            enum Binary {
                None,
                Bool(bool),
                Int(i32),
                Uint(u32),
                Float(f32),
            }
            let b = Binary::deserialize(deserializer)?;
            Ok(match b {
                Binary::None => ControlValue::None,
                Binary::Bool(v) => ControlValue::Bool(v),
                Binary::Int(v) => ControlValue::Int(v),
                Binary::Uint(v) => ControlValue::Uint(v),
                Binary::Float(v) => ControlValue::Float(v),
            })
        }
    }
}

impl ControlMeta {
    /// Basic range validation respecting the variant.
    ///
    /// # Example
    /// ```rust
    /// use styx_core::prelude::{Access, ControlId, ControlKind, ControlMeta, ControlValue};
    ///
    /// let meta = ControlMeta {
    ///     id: ControlId(1),
    ///     name: "brightness".into(),
    ///     kind: ControlKind::Int,
    ///     access: Access::ReadWrite,
    ///     min: ControlValue::Int(-10),
    ///     max: ControlValue::Int(10),
    ///     default: ControlValue::Int(0),
    ///     step: Some(ControlValue::Int(1)),
    ///     menu: None,
    /// };
    /// assert!(meta.validate(&ControlValue::Int(5)));
    /// ```
    pub fn validate(&self, candidate: &ControlValue) -> bool {
        if let Some(menu) = &self.menu {
            // For menus, only None/Uint within menu length or Int for IntMenu.
            if let ControlValue::Uint(idx) = candidate {
                return (*idx as usize) < menu.len();
            }
            if let ControlValue::Int(idx) = candidate {
                return (*idx >= 0) && ((*idx as usize) < menu.len());
            }
        }

        match (candidate, &self.min, &self.max) {
            (ControlValue::Bool(v), ControlValue::Bool(min), ControlValue::Bool(max)) => {
                let v = *v as u8;
                let min = *min as u8;
                let max = *max as u8;
                v >= min && v <= max
            }
            (ControlValue::Int(v), ControlValue::Int(min), ControlValue::Int(max)) => {
                let within = v >= min && v <= max;
                if !within {
                    return false;
                }
                if let Some(ControlValue::Int(step)) = &self.step
                    && *step > 0
                {
                    return ((v - min) % step) == 0;
                }
                true
            }
            (ControlValue::Uint(v), ControlValue::Uint(min), ControlValue::Uint(max)) => {
                let within = v >= min && v <= max;
                if !within {
                    return false;
                }
                if let Some(ControlValue::Uint(step)) = &self.step
                    && *step > 0
                {
                    return ((v - min) % step) == 0;
                }
                true
            }
            (ControlValue::Float(v), ControlValue::Float(min), ControlValue::Float(max)) => {
                v >= min && v <= max
            }
            (ControlValue::None, ControlValue::None, ControlValue::None) => true,
            _ => false,
        }
    }
}
