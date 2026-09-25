//! adelie's expression IR (SPEC §7 syntax) and its typing. `eval.rs` (U2) evaluates it.

mod arith;
mod cast;
mod eval;
mod like;

pub use eval::eval;

use crate::types::{DataType, DecimalType, Value, fits};

use super::batch::Field;
use super::context::ExecError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// `a < b` iff `b > a`: flips a scalar-left comparison onto a scalar-right kernel.
    pub fn flip(self) -> CmpOp {
        match self {
            CmpOp::Eq => CmpOp::Eq,
            CmpOp::Ne => CmpOp::Ne,
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// SPEC §8's expression syntax: comparison, boolean, arithmetic, CASE, CAST, IN, BETWEEN,
/// LIKE/ILIKE, IS NULL. Named scalar functions wait for the binder (adelie-1st.1).
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Index into the input batch's fields.
    Column(usize),
    /// `Value::Null`, or a value that `types::fits` the type.
    Literal(Value, DataType),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    InList {
        expr: Box<Expr>,
        list: Vec<Value>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: String,
        case_insensitive: bool,
        negated: bool,
    },
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    /// A searched CASE: each branch's condition is BOOL, and every result (plus `otherwise`)
    /// shares one exact `DataType`.
    Case {
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Cast(Box<Expr>, DataType),
}

impl Expr {
    pub fn col(i: usize) -> Expr {
        Expr::Column(i)
    }

    pub fn lit(v: Value, ty: DataType) -> Expr {
        Expr::Literal(v, ty)
    }

    pub fn cmp(op: CmpOp, l: Expr, r: Expr) -> Expr {
        Expr::Cmp(op, Box::new(l), Box::new(r))
    }

    /// The typing rules of SPEC §7/§8 (see this unit's blueprint §Contract for the full
    /// table). `eval` (U2) must agree with every rule here.
    pub fn data_type(&self, input: &[Field]) -> Result<DataType, ExecError> {
        match self {
            Expr::Column(i) => input.get(*i).map(|f| f.ty.clone()).ok_or_else(|| {
                ExecError::Plan(format!(
                    "column index {i} out of range for {} inputs",
                    input.len()
                ))
            }),
            Expr::Literal(v, ty) => {
                if !v.is_null() && !fits(v, ty) {
                    return Err(ExecError::Plan(format!(
                        "literal {v:?} does not losslessly fit {ty}"
                    )));
                }
                Ok(ty.clone())
            }
            Expr::Cmp(_, l, r) => {
                let lt = l.data_type(input)?;
                let rt = r.data_type(input)?;
                if !same_kind(&lt, &rt) {
                    return Err(ExecError::Plan(format!(
                        "comparison operands {lt} and {rt} are not the same kind"
                    )));
                }
                Ok(DataType::Bool)
            }
            Expr::And(parts) | Expr::Or(parts) => {
                if parts.is_empty() {
                    return Err(ExecError::Plan("AND/OR needs at least one operand".into()));
                }
                for p in parts {
                    let t = p.data_type(input)?;
                    if t != DataType::Bool {
                        return Err(ExecError::Plan(format!(
                            "AND/OR operand must be BOOL, found {t}"
                        )));
                    }
                }
                Ok(DataType::Bool)
            }
            Expr::Not(e) => {
                let t = e.data_type(input)?;
                if t != DataType::Bool {
                    return Err(ExecError::Plan(format!("NOT operand must be BOOL, found {t}")));
                }
                Ok(DataType::Bool)
            }
            Expr::IsNull(e) | Expr::IsNotNull(e) => {
                e.data_type(input)?;
                Ok(DataType::Bool)
            }
            Expr::InList { expr, list, .. } => {
                let t = expr.data_type(input)?;
                for v in list {
                    if !v.is_null() && !fits(v, &t) {
                        return Err(ExecError::Plan(format!(
                            "IN LIST value {v:?} does not fit {t}"
                        )));
                    }
                }
                Ok(DataType::Bool)
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                let t = expr.data_type(input)?;
                let lt = low.data_type(input)?;
                let ht = high.data_type(input)?;
                if !same_kind(&t, &lt) || !same_kind(&t, &ht) {
                    return Err(ExecError::Plan(format!(
                        "BETWEEN bounds must be the same kind as {t}"
                    )));
                }
                Ok(DataType::Bool)
            }
            Expr::Like { expr, .. } => {
                let t = expr.data_type(input)?;
                if t != DataType::String {
                    return Err(ExecError::Plan(format!(
                        "LIKE operand must be STRING, found {t}"
                    )));
                }
                Ok(DataType::Bool)
            }
            Expr::Arith(op, l, r) => {
                let lt = l.data_type(input)?;
                let rt = r.data_type(input)?;
                arith_type(*op, &lt, &rt)
            }
            Expr::Neg(e) => match e.data_type(input)? {
                t @ (DataType::Int64 | DataType::Float64 | DataType::Decimal(_)) => Ok(t),
                other => Err(ExecError::Plan(format!("NEG does not apply to {other}"))),
            },
            Expr::Case { branches, otherwise } => {
                if branches.is_empty() {
                    return Err(ExecError::Plan("CASE needs at least one branch".into()));
                }
                let mut result_ty: Option<DataType> = None;
                for (cond, res) in branches {
                    let ct = cond.data_type(input)?;
                    if ct != DataType::Bool {
                        return Err(ExecError::Plan(format!(
                            "CASE condition must be BOOL, found {ct}"
                        )));
                    }
                    let rt = res.data_type(input)?;
                    match &result_ty {
                        None => result_ty = Some(rt),
                        Some(prev) if *prev == rt => {}
                        Some(prev) => {
                            return Err(ExecError::Plan(format!(
                                "CASE results disagree: {prev} vs {rt}"
                            )));
                        }
                    }
                }
                let result_ty = result_ty.expect("checked non-empty above");
                if let Some(o) = otherwise {
                    let ot = o.data_type(input)?;
                    if ot != result_ty {
                        return Err(ExecError::Plan(format!(
                            "CASE ELSE disagrees: {result_ty} vs {ot}"
                        )));
                    }
                }
                Ok(result_ty)
            }
            Expr::Cast(e, to) => {
                let from = e.data_type(input)?;
                if cast::castable(&from, to) {
                    Ok(to.clone())
                } else {
                    Err(ExecError::Plan(format!("cannot cast {from} to {to}")))
                }
            }
        }
    }

    /// Flattens nested `And`; any other node is one conjunct on its own.
    pub fn conjuncts(&self) -> Vec<&Expr> {
        let mut out = Vec::new();
        self.push_conjuncts(&mut out);
        out
    }

    fn push_conjuncts<'a>(&'a self, out: &mut Vec<&'a Expr>) {
        match self {
            Expr::And(parts) => {
                for p in parts {
                    p.push_conjuncts(out);
                }
            }
            other => out.push(other),
        }
    }

    /// Every `Column` index this expression reads, in visit order (duplicates included).
    pub fn columns(&self, out: &mut Vec<usize>) {
        match self {
            Expr::Column(i) => out.push(*i),
            Expr::Literal(..) => {}
            Expr::Cmp(_, l, r) | Expr::Arith(_, l, r) => {
                l.columns(out);
                r.columns(out);
            }
            Expr::And(parts) | Expr::Or(parts) => {
                for p in parts {
                    p.columns(out);
                }
            }
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Neg(e) => e.columns(out),
            Expr::InList { expr, .. } => expr.columns(out),
            Expr::Between {
                expr, low, high, ..
            } => {
                expr.columns(out);
                low.columns(out);
                high.columns(out);
            }
            Expr::Like { expr, .. } => expr.columns(out),
            Expr::Case { branches, otherwise } => {
                for (cond, res) in branches {
                    cond.columns(out);
                    res.columns(out);
                }
                if let Some(o) = otherwise {
                    o.columns(out);
                }
            }
            Expr::Cast(e, _) => e.columns(out),
        }
    }
}

/// `Cmp`/`Between`'s notion of "same kind": any two DECIMALs match regardless of scale, LIST
/// needs a matching element type, everything else is exact `DataType` equality.
fn same_kind(a: &DataType, b: &DataType) -> bool {
    match (a, b) {
        (DataType::Decimal(_), DataType::Decimal(_)) => true,
        (DataType::List(la), DataType::List(lb)) => la.element() == lb.element(),
        _ => a == b,
    }
}

fn is_numeric(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Int64 | DataType::UInt64 | DataType::Float64 | DataType::Decimal(_)
    )
}

fn arith_type(op: ArithOp, lt: &DataType, rt: &DataType) -> Result<DataType, ExecError> {
    if !is_numeric(lt) || !is_numeric(rt) {
        return Err(ExecError::Plan(format!(
            "arithmetic operands must be numeric, found {lt} and {rt}"
        )));
    }
    match (lt, rt) {
        (DataType::Decimal(a), DataType::Decimal(b)) => decimal_arith_type(op, *a, *b),
        _ if lt == rt => Ok(lt.clone()),
        _ => Err(ExecError::Plan(format!(
            "arithmetic operands must be the same numeric kind, found {lt} and {rt}"
        ))),
    }
}

/// `max(p1−s1, p2−s2) + scale + 1`, capped at 38: the ADD/SUB (and MOD) precision rule.
fn add_sub_precision(scale: u8, a: DecimalType, b: DecimalType) -> u8 {
    let int_digits =
        (a.precision() as i32 - a.scale() as i32).max(b.precision() as i32 - b.scale() as i32);
    (int_digits + scale as i32 + 1).min(38) as u8
}

fn decimal_arith_type(op: ArithOp, a: DecimalType, b: DecimalType) -> Result<DataType, ExecError> {
    match op {
        ArithOp::Add | ArithOp::Sub | ArithOp::Mod => {
            let scale = a.scale().max(b.scale());
            let precision = add_sub_precision(scale, a, b);
            Ok(DataType::decimal(precision, scale).expect("computed decimal type is always valid"))
        }
        ArithOp::Mul => {
            let scale = a.scale() + b.scale();
            if scale > 38 {
                return Err(ExecError::Plan(format!(
                    "DECIMAL multiply scale {scale} exceeds 38"
                )));
            }
            let precision = (a.precision() as u16 + b.precision() as u16).min(38) as u8;
            Ok(DataType::decimal(precision, scale).expect("computed decimal type is always valid"))
        }
        ArithOp::Div => Ok(DataType::Float64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(tys: &[DataType]) -> Vec<Field> {
        tys.iter()
            .enumerate()
            .map(|(i, ty)| Field {
                name: format!("c{i}"),
                ty: ty.clone(),
            })
            .collect()
    }

    #[test]
    fn column_type_and_out_of_range() {
        let input = fields(&[DataType::Int64]);
        assert_eq!(Expr::col(0).data_type(&input).unwrap(), DataType::Int64);
        assert!(matches!(
            Expr::col(1).data_type(&input),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn literal_must_fit_its_type() {
        let input: Vec<Field> = Vec::new();
        assert_eq!(
            Expr::lit(Value::Int64(1), DataType::Int64)
                .data_type(&input)
                .unwrap(),
            DataType::Int64
        );
        assert_eq!(
            Expr::lit(Value::Null, DataType::Int64)
                .data_type(&input)
                .unwrap(),
            DataType::Int64
        );
        let bad = Expr::lit(Value::Int64(-1), DataType::UInt64);
        assert!(matches!(bad.data_type(&input), Err(ExecError::Plan(_))));
    }

    #[test]
    fn cmp_same_kind_ok_mismatch_is_plan() {
        let input = fields(&[DataType::Int64, DataType::UInt64]);
        let ok = Expr::cmp(CmpOp::Eq, Expr::col(0), Expr::col(0));
        assert_eq!(ok.data_type(&input).unwrap(), DataType::Bool);
        let bad = Expr::cmp(CmpOp::Eq, Expr::col(0), Expr::col(1));
        assert!(matches!(bad.data_type(&input), Err(ExecError::Plan(_))));
    }

    #[test]
    fn cmp_decimal_any_scale_matches() {
        let input = fields(&[
            DataType::decimal(5, 2).unwrap(),
            DataType::decimal(3, 1).unwrap(),
        ]);
        let ok = Expr::cmp(CmpOp::Eq, Expr::col(0), Expr::col(1));
        assert_eq!(ok.data_type(&input).unwrap(), DataType::Bool);
    }

    #[test]
    fn and_or_need_bool_operands_and_at_least_one() {
        let input = fields(&[DataType::Bool, DataType::Int64]);
        assert_eq!(
            Expr::And(vec![Expr::col(0)]).data_type(&input).unwrap(),
            DataType::Bool
        );
        assert!(matches!(
            Expr::And(vec![]).data_type(&input),
            Err(ExecError::Plan(_))
        ));
        assert!(matches!(
            Expr::Or(vec![Expr::col(1)]).data_type(&input),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn not_needs_bool() {
        let input = fields(&[DataType::Bool, DataType::Int64]);
        assert_eq!(
            Expr::Not(Box::new(Expr::col(0))).data_type(&input).unwrap(),
            DataType::Bool
        );
        assert!(matches!(
            Expr::Not(Box::new(Expr::col(1))).data_type(&input),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn is_null_and_is_not_null_are_bool_for_any_type() {
        let input = fields(&[DataType::String]);
        assert_eq!(
            Expr::IsNull(Box::new(Expr::col(0)))
                .data_type(&input)
                .unwrap(),
            DataType::Bool
        );
        assert_eq!(
            Expr::IsNotNull(Box::new(Expr::col(0)))
                .data_type(&input)
                .unwrap(),
            DataType::Bool
        );
    }

    #[test]
    fn in_list_values_must_fit() {
        let input = fields(&[DataType::Int64]);
        let ok = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::Int64(1), Value::Null],
            negated: false,
        };
        assert_eq!(ok.data_type(&input).unwrap(), DataType::Bool);
        let bad = Expr::InList {
            expr: Box::new(Expr::col(0)),
            list: vec![Value::String("x".into())],
            negated: false,
        };
        assert!(matches!(bad.data_type(&input), Err(ExecError::Plan(_))));
    }

    #[test]
    fn between_bounds_must_match_kind() {
        let input = fields(&[DataType::Int64]);
        let ok = Expr::Between {
            expr: Box::new(Expr::col(0)),
            low: Box::new(Expr::lit(Value::Int64(1), DataType::Int64)),
            high: Box::new(Expr::lit(Value::Int64(9), DataType::Int64)),
            negated: false,
        };
        assert_eq!(ok.data_type(&input).unwrap(), DataType::Bool);
        let bad = Expr::Between {
            expr: Box::new(Expr::col(0)),
            low: Box::new(Expr::lit(Value::String("a".into()), DataType::String)),
            high: Box::new(Expr::lit(Value::Int64(9), DataType::Int64)),
            negated: false,
        };
        assert!(matches!(bad.data_type(&input), Err(ExecError::Plan(_))));
    }

    #[test]
    fn like_needs_string() {
        let input = fields(&[DataType::String, DataType::Int64]);
        let ok = Expr::Like {
            expr: Box::new(Expr::col(0)),
            pattern: "a%".into(),
            case_insensitive: false,
            negated: false,
        };
        assert_eq!(ok.data_type(&input).unwrap(), DataType::Bool);
        let bad = Expr::Like {
            expr: Box::new(Expr::col(1)),
            pattern: "a%".into(),
            case_insensitive: false,
            negated: false,
        };
        assert!(matches!(bad.data_type(&input), Err(ExecError::Plan(_))));
    }

    #[test]
    fn arith_keeps_type_for_int_uint_float_and_rejects_mismatch() {
        let input = fields(&[DataType::Int64, DataType::Float64, DataType::UInt64]);
        let ok = Expr::Arith(ArithOp::Add, Box::new(Expr::col(0)), Box::new(Expr::col(0)));
        assert_eq!(ok.data_type(&input).unwrap(), DataType::Int64);
        let ok_uint = Expr::Arith(ArithOp::Add, Box::new(Expr::col(2)), Box::new(Expr::col(2)));
        assert_eq!(ok_uint.data_type(&input).unwrap(), DataType::UInt64);
        let bad = Expr::Arith(ArithOp::Add, Box::new(Expr::col(0)), Box::new(Expr::col(1)));
        assert!(matches!(bad.data_type(&input), Err(ExecError::Plan(_))));
    }

    #[test]
    fn decimal_add_sub_scale_and_precision() {
        let input = fields(&[
            DataType::decimal(5, 2).unwrap(),
            DataType::decimal(3, 1).unwrap(),
        ]);
        let add = Expr::Arith(ArithOp::Add, Box::new(Expr::col(0)), Box::new(Expr::col(1)));
        assert_eq!(add.data_type(&input).unwrap(), DataType::decimal(6, 2).unwrap());
        let sub = Expr::Arith(ArithOp::Sub, Box::new(Expr::col(0)), Box::new(Expr::col(1)));
        assert_eq!(sub.data_type(&input).unwrap(), DataType::decimal(6, 2).unwrap());
    }

    #[test]
    fn decimal_mul_scale_and_precision_and_overflow() {
        let input = fields(&[
            DataType::decimal(5, 2).unwrap(),
            DataType::decimal(3, 1).unwrap(),
        ]);
        let mul = Expr::Arith(ArithOp::Mul, Box::new(Expr::col(0)), Box::new(Expr::col(1)));
        assert_eq!(mul.data_type(&input).unwrap(), DataType::decimal(8, 3).unwrap());

        let wide = fields(&[
            DataType::decimal(38, 30).unwrap(),
            DataType::decimal(38, 30).unwrap(),
        ]);
        let overflow = Expr::Arith(ArithOp::Mul, Box::new(Expr::col(0)), Box::new(Expr::col(1)));
        assert!(matches!(overflow.data_type(&wide), Err(ExecError::Plan(_))));
    }

    #[test]
    fn decimal_div_is_float64_mod_matches_add_formula() {
        let input = fields(&[
            DataType::decimal(5, 2).unwrap(),
            DataType::decimal(3, 1).unwrap(),
        ]);
        let div = Expr::Arith(ArithOp::Div, Box::new(Expr::col(0)), Box::new(Expr::col(1)));
        assert_eq!(div.data_type(&input).unwrap(), DataType::Float64);
        let m = Expr::Arith(ArithOp::Mod, Box::new(Expr::col(0)), Box::new(Expr::col(1)));
        assert_eq!(m.data_type(&input).unwrap(), DataType::decimal(6, 2).unwrap());
    }

    #[test]
    fn neg_rejects_uint64() {
        let input = fields(&[
            DataType::Int64,
            DataType::UInt64,
            DataType::Float64,
            DataType::decimal(5, 2).unwrap(),
        ]);
        assert_eq!(
            Expr::Neg(Box::new(Expr::col(0))).data_type(&input).unwrap(),
            DataType::Int64
        );
        assert_eq!(
            Expr::Neg(Box::new(Expr::col(2))).data_type(&input).unwrap(),
            DataType::Float64
        );
        assert_eq!(
            Expr::Neg(Box::new(Expr::col(3))).data_type(&input).unwrap(),
            DataType::decimal(5, 2).unwrap()
        );
        assert!(matches!(
            Expr::Neg(Box::new(Expr::col(1))).data_type(&input),
            Err(ExecError::Plan(_))
        ));
    }

    #[test]
    fn case_requires_bool_conditions_and_matching_results() {
        let input = fields(&[DataType::Bool, DataType::Int64]);
        let ok = Expr::Case {
            branches: vec![(Expr::col(0), Expr::lit(Value::Int64(1), DataType::Int64))],
            otherwise: Some(Box::new(Expr::lit(Value::Int64(2), DataType::Int64))),
        };
        assert_eq!(ok.data_type(&input).unwrap(), DataType::Int64);

        let bad_cond = Expr::Case {
            branches: vec![(Expr::col(1), Expr::lit(Value::Int64(1), DataType::Int64))],
            otherwise: None,
        };
        assert!(matches!(bad_cond.data_type(&input), Err(ExecError::Plan(_))));

        let mismatched = Expr::Case {
            branches: vec![
                (Expr::col(0), Expr::lit(Value::Int64(1), DataType::Int64)),
                (Expr::col(0), Expr::lit(Value::String("x".into()), DataType::String)),
            ],
            otherwise: None,
        };
        assert!(matches!(mismatched.data_type(&input), Err(ExecError::Plan(_))));
    }

    #[test]
    fn conjuncts_flattens_nested_and() {
        let a = Expr::col(0);
        let b = Expr::col(1);
        let c = Expr::col(2);
        let expr = Expr::And(vec![Expr::And(vec![a.clone(), b.clone()]), c.clone()]);
        assert_eq!(expr.conjuncts(), vec![&a, &b, &c]);
    }

    #[test]
    fn conjuncts_of_a_non_and_node_is_itself() {
        let e = Expr::col(0);
        assert_eq!(e.conjuncts(), vec![&e]);
    }

    #[test]
    fn columns_collects_every_referenced_index_in_visit_order() {
        let e = Expr::cmp(CmpOp::Eq, Expr::col(2), Expr::col(0));
        let mut out = Vec::new();
        e.columns(&mut out);
        assert_eq!(out, vec![2, 0]);
    }
}
