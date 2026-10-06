//! `FourCc` as a four-character string in serde formats.

use core::{fmt, str::FromStr};

use super::FourCc;

impl serde::Serialize for FourCc {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Prefer string encoding so decoding does not rely on `deserialize_any`.
        let encoded = self.as_str().unwrap_or("FFFF");
        serializer.serialize_str(encoded)
    }
}

impl<'de> serde::Deserialize<'de> for FourCc {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct FourCcVisitor;

        impl<'de> serde::de::Visitor<'de> for FourCcVisitor {
            type Value = FourCc;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a 4-character FourCc string")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                FourCc::from_str(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(FourCcVisitor)
    }
}
