//! `DataType`: adelie's type set (SPEC §3, contract rule 1) and its `Display` spelling.

use std::fmt;

/// A column or value's type. `List` holds any non-list element type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DataType {
    Bool,
    Int64,
    UInt64,
    Float64,
    Decimal(DecimalType),
    String,
    Bytes,
    Timestamp,
    Date,
    Uuid,
    Ip,
    List(ListType),
}

impl DataType {
    /// `DataType::Decimal`, validated (1 ≤ precision ≤ 38, 0 ≤ scale ≤ precision).
    pub fn decimal(precision: u8, scale: u8) -> Result<Self, TypeError> {
        Ok(DataType::Decimal(DecimalType::new(precision, scale)?))
    }

    /// `DataType::List`, rejecting a list of lists.
    pub fn list(elem: DataType) -> Result<Self, TypeError> {
        Ok(DataType::List(ListType::new(elem)?))
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Bool => write!(f, "BOOL"),
            DataType::Int64 => write!(f, "INT64"),
            DataType::UInt64 => write!(f, "UINT64"),
            DataType::Float64 => write!(f, "FLOAT64"),
            DataType::Decimal(dt) => write!(f, "DECIMAL({},{})", dt.precision(), dt.scale()),
            DataType::String => write!(f, "STRING"),
            DataType::Bytes => write!(f, "BYTES"),
            DataType::Timestamp => write!(f, "TIMESTAMP"),
            DataType::Date => write!(f, "DATE"),
            DataType::Uuid => write!(f, "UUID"),
            DataType::Ip => write!(f, "IP"),
            DataType::List(lt) => write!(f, "LIST<{}>", lt.element()),
        }
    }
}

/// A `DECIMAL(precision, scale)` type: 1 ≤ precision ≤ 38, 0 ≤ scale ≤ precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecimalType {
    precision: u8,
    scale: u8,
}

impl DecimalType {
    pub const MAX_PRECISION: u8 = 38;

    pub fn new(precision: u8, scale: u8) -> Result<Self, TypeError> {
        if precision == 0 || precision > Self::MAX_PRECISION {
            return Err(TypeError::DecimalPrecision(precision));
        }
        if scale > precision {
            return Err(TypeError::DecimalScale { precision, scale });
        }
        Ok(Self { precision, scale })
    }

    pub fn precision(self) -> u8 {
        self.precision
    }

    pub fn scale(self) -> u8 {
        self.scale
    }
}

/// `LIST<element>`. `element` is never itself a `List` (contract rule 1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListType(Box<DataType>);

impl ListType {
    pub fn new(elem: DataType) -> Result<Self, TypeError> {
        if matches!(elem, DataType::List(_)) {
            return Err(TypeError::NestedList);
        }
        Ok(Self(Box::new(elem)))
    }

    pub fn element(&self) -> &DataType {
        &self.0
    }
}

/// A rejected `DataType` construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    DecimalPrecision(u8),
    DecimalScale { precision: u8, scale: u8 },
    NestedList,
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::DecimalPrecision(p) => {
                write!(f, "decimal precision {p} out of range 1..=38")
            }
            TypeError::DecimalScale { precision, scale } => {
                write!(f, "decimal scale {scale} out of range 0..={precision}")
            }
            TypeError::NestedList => write!(f, "list element type cannot itself be a list"),
        }
    }
}

impl std::error::Error for TypeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_type_accepts_boundary_precisions() {
        let wide = DecimalType::new(38, 0).unwrap();
        assert_eq!((wide.precision(), wide.scale()), (38, 0));
        let narrow = DecimalType::new(1, 1).unwrap();
        assert_eq!((narrow.precision(), narrow.scale()), (1, 1));
    }

    #[test]
    fn decimal_type_rejects_out_of_range_precision() {
        assert!(DecimalType::new(0, 0).is_err());
        assert!(DecimalType::new(39, 0).is_err());
    }

    #[test]
    fn decimal_type_rejects_scale_above_precision() {
        assert!(DecimalType::new(5, 6).is_err());
    }

    #[test]
    fn list_of_list_is_rejected() {
        let inner = DataType::list(DataType::Int64).unwrap();
        assert_eq!(DataType::list(inner), Err(TypeError::NestedList));
    }

    #[test]
    fn display_matches_spec_spelling() {
        let dec = DataType::decimal(10, 2).unwrap();
        assert_eq!(dec.to_string(), "DECIMAL(10,2)");
        let list = DataType::list(DataType::Uuid).unwrap();
        assert_eq!(list.to_string(), "LIST<UUID>");
    }
}
