//! Sequence steps: register writes, delays, GPIO lines, clocks and supplies.

use alloc::{format, string::String, vec::Vec};
use core::fmt;
use core::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

/// One register write: `bytes` bytes starting at `address`, most significant byte first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegWrite {
    /// Register address.
    pub address: u16,
    /// Value.
    pub value: u32,
    /// Width in bytes, 1 to 4.
    pub bytes: u8,
}

impl RegWrite {
    /// A one-byte write.
    pub const fn byte(address: u16, value: u8) -> Self {
        Self {
            address,
            value: value as u32,
            bytes: 1,
        }
    }
}

impl fmt::Display for RegWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let w = usize::from(self.bytes) * 2;
        write!(f, "0x{:04x} <- 0x{:0w$x}", self.address, self.value)
    }
}

/// One step of a sequence.
///
/// In TOML a register write is `[address, value]` or `[address, value, bytes]`; everything else
/// is an inline table:
///
/// ```toml
/// power_up = [
///     { supply = "avdd", on = true },
///     { delay_us = 500 },
///     { gpio = "reset", value = 1, optional = true },
///     { clock = "xvclk", on = true },
///     { delay_ms = 1 },
///     [0x4800, 0x00],
///     { address = 0x380e, value = 0x071e, bytes = 2 },
/// ]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write a register.
    Write(RegWrite),
    /// Wait.
    Delay(Duration),
    /// Set a GPIO line by role (`reset`, `powerdown`, `enable`, ...). The value is logical: the
    /// [`SensorPins`](crate::SensorPins) implementation applies the line's polarity.
    Gpio {
        /// Role name.
        role: String,
        /// Logical value.
        value: bool,
        /// Skip the step when the board has no such line.
        optional: bool,
    },
    /// Enable or disable a clock by role; the rate comes from `sensor.clocks`.
    Clock {
        /// Role name.
        role: String,
        /// Enable or disable.
        on: bool,
        /// Skip the step when the board has no such clock.
        optional: bool,
    },
    /// Enable or disable a supply by role.
    Supply {
        /// Role name.
        role: String,
        /// Enable or disable.
        on: bool,
        /// Skip the step when the board has no such supply.
        optional: bool,
    },
}

impl Step {
    /// The register write, if this step is one.
    pub fn as_write(&self) -> Option<&RegWrite> {
        match self {
            Step::Write(w) => Some(w),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepTable {
    address: Option<u16>,
    value: Option<u32>,
    bytes: Option<u8>,
    delay_us: Option<u64>,
    delay_ms: Option<u64>,
    gpio: Option<String>,
    clock: Option<String>,
    supply: Option<String>,
    on: Option<bool>,
    #[serde(default)]
    optional: bool,
}

impl StepTable {
    fn into_step<E: de::Error>(self) -> Result<Step, E> {
        let kinds = [
            self.address.is_some(),
            self.delay_us.is_some() || self.delay_ms.is_some(),
            self.gpio.is_some(),
            self.clock.is_some(),
            self.supply.is_some(),
        ];
        if kinds.iter().filter(|k| **k).count() != 1 {
            return Err(E::custom(
                "a step needs exactly one of `address`, `delay_us`/`delay_ms`, `gpio`, `clock`, `supply`",
            ));
        }
        let no = |cond: bool, msg: &str| if cond { Err(E::custom(msg)) } else { Ok(()) };
        if let Some(address) = self.address {
            no(
                self.on.is_some() || self.optional,
                "a register write takes `value` and `bytes` only",
            )?;
            let value = self
                .value
                .ok_or_else(|| E::custom("register write without `value`"))?;
            return Ok(Step::Write(RegWrite {
                address,
                value,
                bytes: self.bytes.unwrap_or(1),
            }));
        }
        no(
            self.bytes.is_some(),
            "`bytes` only applies to register writes",
        )?;
        if self.delay_us.is_some() || self.delay_ms.is_some() {
            no(
                self.delay_us.is_some() && self.delay_ms.is_some(),
                "give `delay_us` or `delay_ms`, not both",
            )?;
            no(
                self.value.is_some() || self.on.is_some(),
                "a delay takes no `value` or `on`",
            )?;
            let d = match (self.delay_us, self.delay_ms) {
                (Some(us), _) => Duration::from_micros(us),
                (_, Some(ms)) => Duration::from_millis(ms),
                _ => Duration::ZERO,
            };
            return Ok(Step::Delay(d));
        }
        if let Some(role) = self.gpio {
            no(
                self.on.is_some(),
                "a gpio step takes `value = 0|1`, not `on`",
            )?;
            let value = match self.value {
                Some(0) => false,
                Some(1) => true,
                _ => return Err(E::custom("a gpio step needs `value = 0` or `value = 1`")),
            };
            return Ok(Step::Gpio {
                role,
                value,
                optional: self.optional,
            });
        }
        no(
            self.value.is_some(),
            "clock and supply steps take `on = true|false`, not `value`",
        )?;
        let on = self
            .on
            .ok_or_else(|| E::custom("clock and supply steps need `on = true|false`"))?;
        let optional = self.optional;
        Ok(match (self.clock, self.supply) {
            (Some(role), _) => Step::Clock { role, on, optional },
            (_, Some(role)) => Step::Supply { role, on, optional },
            _ => unreachable!("checked above"),
        })
    }
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Step;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("[address, value], [address, value, bytes] or a step table")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Step, A::Error> {
                let mut n = Vec::with_capacity(3);
                while let Some(v) = seq.next_element::<i64>()? {
                    n.push(v);
                }
                if !(2..=3).contains(&n.len()) {
                    return Err(de::Error::custom(format!(
                        "a register write is [address, value] or [address, value, bytes], got {} numbers",
                        n.len()
                    )));
                }
                let address = u16::try_from(n[0]).map_err(|_| {
                    de::Error::custom(format!("register address {} is out of range", n[0]))
                })?;
                let value = u32::try_from(n[1]).map_err(|_| {
                    de::Error::custom(format!("register value {} is out of range", n[1]))
                })?;
                let bytes = match n.get(2) {
                    Some(&b) => {
                        u8::try_from(b).map_err(|_| de::Error::custom("bytes out of range"))?
                    }
                    None => 1,
                };
                Ok(Step::Write(RegWrite {
                    address,
                    value,
                    bytes,
                }))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Step, A::Error> {
                StepTable::deserialize(de::value::MapAccessDeserializer::new(map))?.into_step()
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct S {
        s: Vec<Step>,
    }

    fn parse(src: &str) -> Result<Vec<Step>, String> {
        toml::from_str::<S>(src)
            .map(|s| s.s)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn parses_all_kinds() {
        let s = parse(
            r#"s = [[0x0100, 1], [0x380e, 0x071e, 2], { delay_us = 500 }, { delay_ms = 2 },
                { gpio = "reset", value = 1, optional = true }, { clock = "xvclk", on = true },
                { supply = "avdd", on = false }, { address = 0x3500, value = 0x2820, bytes = 3 }]"#,
        )
        .unwrap();
        assert_eq!(s[0], Step::Write(RegWrite::byte(0x0100, 1)));
        assert_eq!(
            s[1],
            Step::Write(RegWrite {
                address: 0x380e,
                value: 0x071e,
                bytes: 2
            })
        );
        assert_eq!(s[2], Step::Delay(Duration::from_micros(500)));
        assert_eq!(s[3], Step::Delay(Duration::from_millis(2)));
        assert_eq!(
            s[4],
            Step::Gpio {
                role: "reset".into(),
                value: true,
                optional: true
            }
        );
        assert_eq!(
            s[5],
            Step::Clock {
                role: "xvclk".into(),
                on: true,
                optional: false
            }
        );
        assert_eq!(
            s[6],
            Step::Supply {
                role: "avdd".into(),
                on: false,
                optional: false
            }
        );
        assert_eq!(
            s[7],
            Step::Write(RegWrite {
                address: 0x3500,
                value: 0x2820,
                bytes: 3
            })
        );
    }

    #[test]
    fn rejects_bad_steps() {
        assert!(
            parse("s = [[0x0100]]")
                .unwrap_err()
                .contains("[address, value]")
        );
        assert!(
            parse("s = [[0x10000, 1]]")
                .unwrap_err()
                .contains("out of range")
        );
        assert!(
            parse("s = [{ gpio = \"reset\", value = 2 }]")
                .unwrap_err()
                .contains("value = 0")
        );
        assert!(
            parse("s = [{ clock = \"x\" }]")
                .unwrap_err()
                .contains("on = true")
        );
        assert!(
            parse("s = [{ delay_us = 1, gpio = \"a\", value = 1 }]")
                .unwrap_err()
                .contains("exactly one")
        );
        assert!(
            parse("s = [{ dealy_us = 1 }]")
                .unwrap_err()
                .contains("unknown field")
        );
    }
}
