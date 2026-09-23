//! Total order (contract rule 2) and SQL equality (contract rule 3) over `Value`.

use std::cmp::Ordering;

use super::value::{Decimal, Value, pow10};

/// The total order used for sort, min/max and footer stats. `None` for mismatched kinds
/// (Decimal vs Decimal at different scales is still one kind) or when either side is NULL.
pub fn total_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Int64(x), Value::Int64(y)) => Some(x.cmp(y)),
        (Value::UInt64(x), Value::UInt64(y)) => Some(x.cmp(y)),
        (Value::Float64(x), Value::Float64(y)) => Some(float_total_cmp(*x, *y)),
        (Value::Decimal(x), Value::Decimal(y)) => Some(decimal_cmp(*x, *y)),
        (Value::String(x), Value::String(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (Value::Bytes(x), Value::Bytes(y)) => Some(x.cmp(y)),
        (Value::Timestamp(x), Value::Timestamp(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::Uuid(x), Value::Uuid(y)) => Some(x.cmp(y)),
        (Value::Ip(x), Value::Ip(y)) => Some(x.cmp(y)),
        (Value::List(x), Value::List(y)) => list_cmp(x, y),
        _ => None,
    }
}

/// -0.0 maps to 0.0 and NaN sorts greatest, unlike `f64::total_cmp` (which splits -0.0/0.0
/// and orders NaNs by sign and payload). Both NaNs land on the one "greatest" slot.
fn float_total_cmp(a: f64, b: f64) -> Ordering {
    let norm = |f: f64| if f == 0.0 { 0.0 } else { f };
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => norm(a)
            .partial_cmp(&norm(b))
            .expect("finite floats compare"),
    }
}

/// Exact cross-scale compare: integer parts first, then fractions normalised to 38 digits.
/// Never rescales by multiplying up to a common scale — that overflows i128 past 10^38.
fn decimal_cmp(a: Decimal, b: Decimal) -> Ordering {
    let (ia, fa) = split(a);
    let (ib, fb) = split(b);
    ia.cmp(&ib).then_with(|| {
        let fa_norm = fa * pow10(38 - a.scale());
        let fb_norm = fb * pow10(38 - b.scale());
        fa_norm.cmp(&fb_norm)
    })
}

fn split(d: Decimal) -> (i128, i128) {
    let p = pow10(d.scale());
    (d.unscaled().div_euclid(p), d.unscaled().rem_euclid(p))
}

/// Lexicographic by element; a NULL element sorts after any non-null one and equals another
/// NULL. A shorter prefix of otherwise-equal elements sorts first.
fn list_cmp(a: &[Value], b: &[Value]) -> Option<Ordering> {
    for (x, y) in a.iter().zip(b.iter()) {
        match list_elem_cmp(x, y)? {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(a.len().cmp(&b.len()))
}

fn list_elem_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Null, _) => Some(Ordering::Greater),
        (_, Value::Null) => Some(Ordering::Less),
        _ => total_cmp(a, b),
    }
}

/// SQL equality: NULL gives unknown, NaN = NaN is false, -0.0 = 0.0 is true, and mismatched
/// kinds give unknown (the binder casts before it compares) — separate from `total_cmp`.
pub fn sql_eq(a: &Value, b: &Value) -> Option<bool> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Bool(x), Value::Bool(y)) => Some(x == y),
        (Value::Int64(x), Value::Int64(y)) => Some(x == y),
        (Value::UInt64(x), Value::UInt64(y)) => Some(x == y),
        (Value::Float64(x), Value::Float64(y)) => Some(!x.is_nan() && !y.is_nan() && x == y),
        (Value::Decimal(x), Value::Decimal(y)) => Some(decimal_cmp(*x, *y) == Ordering::Equal),
        (Value::String(x), Value::String(y)) => Some(x == y),
        (Value::Bytes(x), Value::Bytes(y)) => Some(x == y),
        (Value::Timestamp(x), Value::Timestamp(y)) => Some(x == y),
        (Value::Date(x), Value::Date(y)) => Some(x == y),
        (Value::Uuid(x), Value::Uuid(y)) => Some(x == y),
        (Value::Ip(x), Value::Ip(y)) => Some(x == y),
        (Value::List(x), Value::List(y)) => list_sql_eq(x, y),
        _ => None,
    }
}

/// Not in the contract text (which covers scalars); Kleene AND over elements is the natural
/// extension: any definite mismatch is `false`, else any NULL comparison makes it unknown.
fn list_sql_eq(a: &[Value], b: &[Value]) -> Option<bool> {
    if a.len() != b.len() {
        return Some(false);
    }
    let mut unknown = false;
    for (x, y) in a.iter().zip(b) {
        match sql_eq(x, y) {
            Some(false) => return Some(false),
            None => unknown = true,
            Some(true) => {}
        }
    }
    if unknown { None } else { Some(true) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::{Equal, Greater, Less};

    fn dec(unscaled: i128, scale: u8) -> Value {
        Value::Decimal(Decimal::new(unscaled, scale).unwrap())
    }

    #[test]
    fn float_total_order_sorts_nan_greatest_and_folds_signed_zero() {
        let mut xs = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0, 1.0];
        xs.sort_by(|a, b| float_total_cmp(*a, *b));
        assert_eq!(xs[0], f64::NEG_INFINITY);
        assert!(xs[1] == 0.0 && xs[2] == 0.0);
        assert_eq!(xs[3], 1.0);
        assert_eq!(xs[4], f64::INFINITY);
        assert!(xs[5].is_nan());
    }

    #[test]
    fn float_total_order_treats_signed_zero_and_nan_as_equal() {
        assert_eq!(float_total_cmp(-0.0, 0.0), Equal);
        assert_eq!(float_total_cmp(f64::NAN, f64::NAN), Equal);
    }

    #[test]
    fn sql_eq_nan_is_false_but_signed_zero_is_true() {
        assert_eq!(
            sql_eq(&Value::Float64(f64::NAN), &Value::Float64(f64::NAN)),
            Some(false)
        );
        assert_eq!(
            sql_eq(&Value::Float64(-0.0), &Value::Float64(0.0)),
            Some(true)
        );
    }

    #[test]
    fn sql_eq_null_is_unknown() {
        assert_eq!(sql_eq(&Value::Null, &Value::Int64(1)), None);
    }

    #[test]
    fn sql_eq_mismatched_kind_is_unknown() {
        assert_eq!(sql_eq(&Value::Int64(1), &Value::UInt64(1)), None);
    }

    #[test]
    fn decimal_compares_exactly_across_scales() {
        assert_eq!(total_cmp(&dec(15, 1), &dec(150, 2)), Some(Equal));
        assert_eq!(sql_eq(&dec(15, 1), &dec(150, 2)), Some(true));
    }

    #[test]
    fn decimal_orders_negative_below_positive_across_scales() {
        assert_eq!(total_cmp(&dec(-1, 1), &dec(5, 2)), Some(Less));
    }

    #[test]
    fn decimal_near_max_precision_compares_without_overflow() {
        let a = dec(pow10(37), 0);
        let b = dec(pow10(37), 38);
        assert_eq!(total_cmp(&a, &b), Some(Greater));
    }

    #[test]
    fn list_shorter_prefix_sorts_first() {
        let a = Value::List(vec![Value::Int64(1)]);
        let b = Value::List(vec![Value::Int64(1), Value::Int64(2)]);
        assert_eq!(total_cmp(&a, &b), Some(Less));
    }

    #[test]
    fn list_null_element_sorts_after_non_null() {
        let a = Value::List(vec![Value::Int64(1), Value::Null]);
        let b = Value::List(vec![Value::Int64(1), Value::Int64(5)]);
        assert_eq!(total_cmp(&a, &b), Some(Greater));
    }

    #[test]
    fn list_null_elements_are_equal() {
        let a = Value::List(vec![Value::Null]);
        let b = Value::List(vec![Value::Null]);
        assert_eq!(total_cmp(&a, &b), Some(Equal));
    }
}
