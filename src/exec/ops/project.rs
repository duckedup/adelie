//! Row-wise projection (SPEC §7): evaluates `exprs` against each input batch.

use crate::exec::expr::{Expr, eval};
use crate::exec::operator::Operator;
use crate::exec::{Batch, ExecContext, ExecError, Field};

pub(crate) struct Project {
    exprs: Vec<(String, Expr)>,
    fields: Vec<Field>,
}

impl Project {
    /// Computes the output fields via `Expr::data_type`; a duplicate output name is `Plan`.
    pub(crate) fn new(exprs: Vec<(String, Expr)>, input: &[Field]) -> Result<Project, ExecError> {
        let mut fields: Vec<Field> = Vec::with_capacity(exprs.len());
        for (name, expr) in &exprs {
            if fields.iter().any(|f| &f.name == name) {
                return Err(ExecError::Plan(format!("duplicate output column {name}")));
            }
            let ty = expr.data_type(input)?;
            fields.push(Field {
                name: name.clone(),
                ty,
            });
        }
        Ok(Project { exprs, fields })
    }

    pub(crate) fn fields(&self) -> &[Field] {
        &self.fields
    }
}

impl Operator for Project {
    fn push(
        &mut self,
        _ctx: &ExecContext,
        batch: Batch,
        out: &mut Vec<Batch>,
    ) -> Result<(), ExecError> {
        let mut columns = Vec::with_capacity(self.exprs.len());
        for (_, expr) in &self.exprs {
            columns.push(match expr {
                // Zero-copy fast path: a bare column reference needs no `eval`.
                Expr::Column(i) => batch.column(*i).clone(),
                other => eval(other, &batch)?,
            });
        }
        out.push(Batch::new(self.fields.clone(), columns)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ArithOp, Column};
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    #[test]
    fn projects_column_and_arithmetic_with_right_fields_and_values() {
        let input = vec![field("a", DataType::Int64)];
        let batch = Batch::new(
            input.clone(),
            vec![Column::from_values(&DataType::Int64, &[Value::Int64(5)]).unwrap()],
        )
        .unwrap();
        let exprs = vec![
            ("a".to_string(), Expr::col(0)),
            (
                "b".to_string(),
                Expr::Arith(
                    ArithOp::Add,
                    Box::new(Expr::col(0)),
                    Box::new(Expr::lit(Value::Int64(1), DataType::Int64)),
                ),
            ),
        ];
        let mut project = Project::new(exprs, &input).unwrap();
        assert_eq!(
            project.fields().to_vec(),
            vec![field("a", DataType::Int64), field("b", DataType::Int64)]
        );
        let mut out = Vec::new();
        let ctx = ExecContext::unlimited();
        project.push(&ctx, batch, &mut out).unwrap();
        assert_eq!(out[0].column(0).get(0), Value::Int64(5));
        assert_eq!(out[0].column(1).get(0), Value::Int64(6));
    }

    #[test]
    fn duplicate_output_name_is_plan_error() {
        let input = vec![field("a", DataType::Int64)];
        let exprs = vec![("a".to_string(), Expr::col(0)), ("a".to_string(), Expr::col(0))];
        let err = Project::new(exprs, &input).err().unwrap();
        assert!(matches!(err, ExecError::Plan(_)));
    }
}
