//! §7 operators that are neither join nor aggregate: the streaming `Filter` and `Project`,
//! and the pipeline-breaking sinks `Collect`, `Limit`, `Sort`, `TopK`, plus `UnionAll`.

mod collect;
mod filter;
mod limit;
mod project;
mod sort;
mod topk;
mod union_all;

pub(crate) use collect::CollectSink;
pub(crate) use filter::Filter;
pub(crate) use limit::LimitSink;
pub(crate) use project::Project;
pub(crate) use sort::{SortSink, sort_batches};
pub(crate) use topk::TopKSink;
pub(crate) use union_all::UnionSource;
