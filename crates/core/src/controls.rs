use alloc::{string::String, vec::Vec};
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

/// Optional control metadata flags.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ControlMetadata {
    /// Control requires a dedicated TDN output stream to be enabled.
    #[cfg_attr(feature = "serde", serde(default))]
    pub requires_tdn_output: bool,
}

/// Simplified control metadata.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{Access, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlValue};
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
///     metadata: ControlMetadata::default(),
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
    /// The entries of a menu control ([`ControlKind::Menu`] or [`ControlKind::IntMenu`]), in
    /// ascending value: only values the control takes, so a sparse menu (a V4L2 driver that
    /// skips indexes, e.g. a UVC exposure menu of 1 manual and 3 aperture priority) lists just
    /// those. Look entries up with [`ControlMeta::menu_entry`] and
    /// [`ControlMeta::menu_entry_named`]; set one with [`ControlMeta::menu_value`].
    #[cfg_attr(feature = "serde", serde(default))]
    pub menu: Option<Vec<MenuEntry>>,
    /// Optional metadata flags.
    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: ControlMetadata,
}

/// One entry of a menu control: the control value that selects it and what it stands for.
///
/// # Example
/// ```rust
/// use styx_core::prelude::{MenuEntry, MenuLabel};
///
/// let modes = MenuEntry::names(["off", "50 Hz", "60 Hz", "auto"]);
/// assert_eq!(modes[2], MenuEntry::named(2, "60 Hz"));
/// let rates = MenuEntry::int(1, 456_000_000);
/// assert_eq!(rates.label, MenuLabel::Int(456_000_000));
/// assert_eq!(rates.to_string(), "456000000");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct MenuEntry {
    /// The control value that selects the entry (a V4L2 menu index).
    pub value: i64,
    /// What the entry stands for.
    pub label: MenuLabel,
}

/// What a menu entry stands for: a name ([`ControlKind::Menu`]) or a number
/// ([`ControlKind::IntMenu`], e.g. a link frequency in Hz).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(tag = "kind", content = "value", rename_all = "snake_case")
)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum MenuLabel {
    Name(String),
    Int(i64),
}

impl MenuEntry {
    /// An entry named `name`, selected by `value`.
    pub fn named(value: i64, name: impl Into<String>) -> Self {
        Self {
            value,
            label: MenuLabel::Name(name.into()),
        }
    }

    /// An integer-menu entry standing for `number`, selected by `value`.
    pub fn int(value: i64, number: i64) -> Self {
        Self {
            value,
            label: MenuLabel::Int(number),
        }
    }

    /// Entries named `names`, selected by 0, 1, 2, ...
    pub fn names<S: Into<String>>(names: impl IntoIterator<Item = S>) -> Vec<Self> {
        names
            .into_iter()
            .enumerate()
            .map(|(i, name)| Self::named(i as i64, name))
            .collect()
    }

    /// The entry's name, if it has one.
    pub fn name(&self) -> Option<&str> {
        match &self.label {
            MenuLabel::Name(name) => Some(name),
            MenuLabel::Int(_) => None,
        }
    }
}

impl core::fmt::Display for MenuEntry {
    /// The name, or the number in decimal.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.label {
            MenuLabel::Name(name) => f.write_str(name),
            MenuLabel::Int(n) => write!(f, "{n}"),
        }
    }
}

/// Control kind/type metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum ControlKind {
    None,
    Bool,
    /// A rectangle (or one rectangle per output), e.g. libcamera `ScalerCrop`/`ScalerCrops`.
    Rectangle,
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
    /// Rectangle, e.g. a crop region in sensor pixel-array coordinates.
    Rect(ControlRect),
    /// One rectangle per output, e.g. libcamera's per-stream `ScalerCrops`.
    Rects(Vec<ControlRect>),
}

/// Rectangle control value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ControlRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
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
                Rect(ControlRect),
                Rects(Vec<ControlRect>),
            }
            let h = match self {
                ControlValue::None => Human::None,
                ControlValue::Bool(v) => Human::Bool(*v),
                ControlValue::Int(v) => Human::Int(*v),
                ControlValue::Uint(v) => Human::Uint(*v),
                ControlValue::Float(v) => Human::Float(*v),
                ControlValue::Rect(v) => Human::Rect(*v),
                ControlValue::Rects(v) => Human::Rects(v.clone()),
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
                Rect(ControlRect),
                Rects(Vec<ControlRect>),
            }
            let b = match self {
                ControlValue::None => Binary::None,
                ControlValue::Bool(v) => Binary::Bool(*v),
                ControlValue::Int(v) => Binary::Int(*v),
                ControlValue::Uint(v) => Binary::Uint(*v),
                ControlValue::Float(v) => Binary::Float(*v),
                ControlValue::Rect(v) => Binary::Rect(*v),
                ControlValue::Rects(v) => Binary::Rects(v.clone()),
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
                Rect(ControlRect),
                Rects(Vec<ControlRect>),
            }
            let h = Human::deserialize(deserializer)?;
            Ok(match h {
                Human::None => ControlValue::None,
                Human::Bool(v) => ControlValue::Bool(v),
                Human::Int(v) => ControlValue::Int(v),
                Human::Uint(v) => ControlValue::Uint(v),
                Human::Float(v) => ControlValue::Float(v),
                Human::Rect(v) => ControlValue::Rect(v),
                Human::Rects(v) => ControlValue::Rects(v),
            })
        } else {
            #[derive(serde::Deserialize)]
            enum Binary {
                None,
                Bool(bool),
                Int(i32),
                Uint(u32),
                Float(f32),
                Rect(ControlRect),
                Rects(Vec<ControlRect>),
            }
            let b = Binary::deserialize(deserializer)?;
            Ok(match b {
                Binary::None => ControlValue::None,
                Binary::Bool(v) => ControlValue::Bool(v),
                Binary::Int(v) => ControlValue::Int(v),
                Binary::Uint(v) => ControlValue::Uint(v),
                Binary::Float(v) => ControlValue::Float(v),
                Binary::Rect(v) => ControlValue::Rect(v),
                Binary::Rects(v) => ControlValue::Rects(v),
            })
        }
    }
}

/// A menu control's value as the index of an entry.
fn menu_index(v: &ControlValue) -> Option<i64> {
    match v {
        ControlValue::Uint(n) => Some(i64::from(*n)),
        ControlValue::Int(n) => Some(i64::from(*n)),
        _ => None,
    }
}

impl ControlMeta {
    /// The menu entry `value` selects (`None`: not a menu control, or no such entry).
    pub fn menu_entry(&self, value: &ControlValue) -> Option<&MenuEntry> {
        let index = menu_index(value)?;
        self.menu.as_ref()?.iter().find(|e| e.value == index)
    }

    /// The menu entry whose name (or, in an integer menu, number in decimal) is `name`,
    /// ignoring ASCII case.
    pub fn menu_entry_named(&self, name: &str) -> Option<&MenuEntry> {
        self.menu.as_ref()?.iter().find(|e| match &e.label {
            MenuLabel::Name(n) => n.eq_ignore_ascii_case(name),
            MenuLabel::Int(n) => name.trim().parse::<i64>().is_ok_and(|v| v == *n),
        })
    }

    /// The value that selects `entry`, in the variant the control takes (that of its minimum).
    pub fn menu_value(&self, entry: &MenuEntry) -> ControlValue {
        match self.min {
            ControlValue::Uint(_) => {
                ControlValue::Uint(entry.value.clamp(0, i64::from(u32::MAX)) as u32)
            }
            _ => {
                ControlValue::Int(entry.value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
            }
        }
    }

    /// Basic range validation respecting the variant.
    ///
    /// # Example
    /// ```rust
    /// use styx_core::prelude::{Access, ControlId, ControlKind, ControlMeta, ControlMetadata, ControlValue};
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
    ///     metadata: ControlMetadata::default(),
    /// };
    /// assert!(meta.validate(&ControlValue::Int(5)));
    /// ```
    pub fn validate(&self, candidate: &ControlValue) -> bool {
        if self.menu.is_some() {
            // A menu's value selects one of its entries, within the control's range.
            let Some(idx) = menu_index(candidate) else {
                return false;
            };
            return menu_index(&self.min).is_none_or(|lo| idx >= lo)
                && menu_index(&self.max).is_none_or(|hi| idx <= hi)
                && self.menu_entry(candidate).is_some();
        }

        if self.kind == ControlKind::Rectangle {
            let valid = |r: &ControlRect| r.width > 0 && r.height > 0;
            return match candidate {
                ControlValue::Rect(r) => valid(r),
                ControlValue::Rects(rects) => !rects.is_empty() && rects.iter().all(valid),
                _ => false,
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn exposure_menu() -> ControlMeta {
        ControlMeta {
            id: ControlId(1),
            name: "Auto Exposure".into(),
            kind: ControlKind::Menu,
            access: Access::ReadWrite,
            min: ControlValue::Uint(0),
            max: ControlValue::Uint(3),
            default: ControlValue::Uint(3),
            step: None,
            // Sparse, as a UVC camera's: 1 manual and 3 aperture priority only.
            menu: Some(vec![
                MenuEntry::named(1, "Manual Mode"),
                MenuEntry::named(3, "Aperture Priority Mode"),
            ]),
            metadata: ControlMetadata::default(),
        }
    }

    #[test]
    fn menu_entries_are_found_by_value_and_by_name() {
        let m = exposure_menu();
        assert_eq!(
            m.menu_entry(&ControlValue::Int(3))
                .and_then(MenuEntry::name),
            Some("Aperture Priority Mode")
        );
        assert!(m.menu_entry(&ControlValue::Uint(2)).is_none());
        let manual = m.menu_entry_named("manual mode").unwrap();
        assert_eq!(m.menu_value(manual), ControlValue::Uint(1));
        assert!(m.menu_entry_named("Shutter Priority Mode").is_none());
    }

    #[test]
    fn only_listed_entries_within_range_validate() {
        let m = exposure_menu();
        for (v, ok) in [(0, false), (1, true), (2, false), (3, true), (4, false)] {
            assert_eq!(m.validate(&ControlValue::Int(v)), ok, "{v}");
        }
        assert!(!m.validate(&ControlValue::Bool(true)));
        let mut narrowed = m.clone();
        narrowed.max = ControlValue::Uint(2);
        assert!(
            !narrowed.validate(&ControlValue::Uint(3)),
            "an entry beyond the range"
        );
    }

    #[test]
    fn integer_menus_look_up_by_number() {
        let m = ControlMeta {
            kind: ControlKind::IntMenu,
            min: ControlValue::Int(0),
            max: ControlValue::Int(1),
            menu: Some(vec![
                MenuEntry::int(0, 400_000_000),
                MenuEntry::int(1, 456_000_000),
            ]),
            ..exposure_menu()
        };
        let e = m.menu_entry_named("456000000").unwrap();
        assert_eq!(m.menu_value(e), ControlValue::Int(1));
        assert_eq!(alloc::format!("{e}"), "456000000");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn menu_entries_serialize_with_their_label_kind() {
        let e = MenuEntry::named(3, "Auto");
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"value":3,"label":{"kind":"name","value":"Auto"}}"#
        );
        assert_eq!(serde_json::from_str::<MenuEntry>(&json).unwrap(), e);
    }
}
