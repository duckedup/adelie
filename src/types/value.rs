//! `Value`: adelie's runtime value, one variant per `DataType` kind (contract rule 1).

use std::net::{IpAddr, Ipv6Addr};

use super::datatype::TypeError;

/// A runtime value. Derived `PartialEq` is structural (`NaN != NaN`, `1.5 != 1.50`); SQL
/// equality is `order::sql_eq`, not this trait.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    UInt64(u64),
    Float64(f64),
    Decimal(Decimal),
    String(String),
    Bytes(Vec<u8>),
    /// Nanoseconds since the Unix epoch, UTC.
    Timestamp(i64),
    /// Days since the Unix epoch.
    Date(i32),
    Uuid([u8; 16]),
    Ip(Ip),
    List(Vec<Value>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// `unscaled * 10^-scale`. `scale` ≤ 38 and `|unscaled|` < 10^38, so it always fits `Decimal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    unscaled: i128,
    scale: u8,
}

impl Decimal {
    /// Reuses `TypeError`'s decimal variants: a scale above 38, or a magnitude that would
    /// need more than 38 digits, are both "this doesn't fit the max precision".
    pub fn new(unscaled: i128, scale: u8) -> Result<Self, TypeError> {
        if scale > 38 {
            return Err(TypeError::DecimalScale {
                precision: 38,
                scale,
            });
        }
        if unscaled.unsigned_abs() >= pow10(38) as u128 {
            return Err(TypeError::DecimalPrecision(39));
        }
        Ok(Self { unscaled, scale })
    }

    pub fn unscaled(self) -> i128 {
        self.unscaled
    }

    pub fn scale(self) -> u8 {
        self.scale
    }
}

/// 10^`exp`. `exp` ≤ 38, which fits `i128` (max ≈ 1.7 × 10^38).
pub(crate) fn pow10(exp: u8) -> i128 {
    10i128.pow(exp as u32)
}

/// 16 bytes: an IPv4 address stored IPv4-mapped (`::ffff:a.b.c.d`), or a native IPv6 address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ip([u8; 16]);

impl From<IpAddr> for Ip {
    fn from(addr: IpAddr) -> Self {
        match addr {
            IpAddr::V4(v4) => Ip(v4.to_ipv6_mapped().octets()),
            IpAddr::V6(v6) => Ip(v6.octets()),
        }
    }
}

impl Ip {
    /// The address, with an IPv4-mapped `V6` folded back to `V4`.
    pub fn to_ip_addr(&self) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(self.0)).to_canonical()
    }

    pub fn octets(&self) -> [u8; 16] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_null() {
        assert!(Value::Null.is_null());
        assert!(!Value::Int64(0).is_null());
    }

    #[test]
    fn decimal_rejects_scale_above_max() {
        assert!(Decimal::new(0, 39).is_err());
    }

    #[test]
    fn decimal_rejects_magnitude_at_max_precision() {
        assert!(Decimal::new(pow10(38), 0).is_err());
        assert!(Decimal::new(pow10(38) - 1, 0).is_ok());
    }

    #[test]
    fn ip_v4_round_trips_through_mapped_v6() {
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        let ip = Ip::from(v4);
        assert_eq!(ip.to_ip_addr(), v4);
    }

    #[test]
    fn ip_v6_round_trips() {
        let v6: IpAddr = "::1".parse().unwrap();
        let ip = Ip::from(v6);
        assert_eq!(ip.to_ip_addr(), v6);
    }
}
