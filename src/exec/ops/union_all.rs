//! `UnionSource`: concatenates several `MorselSource`s into one (SPEC §7 UNION ALL). Field
//! names come from the first input; every input must share its column types.

use crate::exec::operator::MorselSource;
use crate::exec::{Batch, ExecContext, ExecError, Field, ScanStats};

pub(crate) struct UnionSource<'a> {
    inputs: Vec<Box<dyn MorselSource + 'a>>,
    fields: Vec<Field>,
}

impl<'a> UnionSource<'a> {
    pub(crate) fn new(inputs: Vec<Box<dyn MorselSource + 'a>>) -> Result<UnionSource<'a>, ExecError> {
        let fields = match inputs.first() {
            Some(first) => first.fields().to_vec(),
            None => {
                return Err(ExecError::Plan(
                    "UNION ALL needs at least one input".to_string(),
                ));
            }
        };
        for input in &inputs[1..] {
            let other = input.fields();
            let same_types =
                other.len() == fields.len() && fields.iter().zip(other).all(|(a, b)| a.ty == b.ty);
            if !same_types {
                return Err(ExecError::Plan(
                    "UNION ALL inputs must have the same column types".to_string(),
                ));
            }
        }
        Ok(UnionSource { inputs, fields })
    }

    /// Maps a global morsel index to its source input and that input's local morsel index.
    fn locate(&self, morsel: usize) -> (usize, usize) {
        let mut remaining = morsel;
        for (i, input) in self.inputs.iter().enumerate() {
            let n = input.morsels();
            if remaining < n {
                return (i, remaining);
            }
            remaining -= n;
        }
        unreachable!("morsel {morsel} out of range for {} total", self.morsels())
    }
}

impl<'a> MorselSource for UnionSource<'a> {
    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn morsels(&self) -> usize {
        self.inputs.iter().map(|i| i.morsels()).sum()
    }

    fn read(&self, morsel: usize, ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        let (i, local) = self.locate(morsel);
        self.inputs[i]
            .read(local, ctx)?
            .into_iter()
            .map(|b| Batch::new(self.fields.clone(), b.columns().to_vec()).map_err(ExecError::from))
            .collect()
    }

    fn stats(&self) -> ScanStats {
        let mut total = ScanStats::default();
        for input in &self.inputs {
            total.add(&input.stats());
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{BatchSource, Column};
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn one_row_batch(f: &Field, v: Value) -> Batch {
        Batch::new(vec![f.clone()], vec![Column::from_values(&f.ty, &[v]).unwrap()]).unwrap()
    }

    #[test]
    fn morsel_counts_sum_and_second_input_is_relabelled() {
        let fa = field("a", DataType::Int64);
        let fx = field("x", DataType::Int64);
        let src1 = BatchSource::new(vec![fa.clone()], vec![one_row_batch(&fa, Value::Int64(1))]);
        let src2 = BatchSource::new(
            vec![fx.clone()],
            vec![
                one_row_batch(&fx, Value::Int64(2)),
                one_row_batch(&fx, Value::Int64(3)),
            ],
        );
        let union = UnionSource::new(vec![Box::new(src1), Box::new(src2)]).unwrap();
        assert_eq!(union.fields().to_vec(), vec![fa.clone()]);
        assert_eq!(union.morsels(), 3);

        let ctx = ExecContext::unlimited();
        let second = union.read(1, &ctx).unwrap();
        assert_eq!(second[0].fields().to_vec(), vec![fa.clone()]);
        assert_eq!(second[0].column(0).get(0), Value::Int64(2));
    }

    #[test]
    fn type_mismatch_is_a_plan_error() {
        let fa = field("a", DataType::Int64);
        let fb = field("b", DataType::Bool);
        let src1 = BatchSource::new(vec![fa], vec![]);
        let src2 = BatchSource::new(vec![fb], vec![]);
        let err = UnionSource::new(vec![Box::new(src1), Box::new(src2)]).err().unwrap();
        assert!(matches!(err, ExecError::Plan(_)));
    }

    #[test]
    fn empty_inputs_is_a_plan_error() {
        let err = UnionSource::new(Vec::<Box<dyn MorselSource>>::new()).err().unwrap();
        assert!(matches!(err, ExecError::Plan(_)));
    }
}
