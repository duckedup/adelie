//! `Edit` and `Commit`: OCC per table (SPEC §5, §18, D0012) — appends never conflict; a commit
//! conflicts only over a segment no longer live or a table that already exists.

use crate::types::{DataType, Value, coerce};

use super::alter;
use super::error::Error;
use super::{
    Alter, FieldId, Garbage, Job, Manifest, PartitionBy, Predicate, Retired, RetireReason,
    SchemaField, SegmentEntry, TableEntry, TableId, TableName, TableSpec, Tombstone, Ttl,
};

/// One change to a `Manifest`. `Commit::apply` applies a list of these in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    CreateTable {
        spec: TableSpec,
    },
    /// `seq` is set by the caller: the new version for a flush, the max input seq for a
    /// compaction.
    AddSegments {
        table: TableName,
        segments: Vec<SegmentEntry>,
    },
    /// Moves the named ids to garbage with `removed_at_ms`.
    RemoveSegments {
        table: TableName,
        ids: Vec<u64>,
    },
    /// The pushed tombstone's `seq` is the commit's new version.
    AddTombstone {
        table: TableName,
        predicates: Vec<Predicate>,
    },
    /// GC'd files. Missing ids are fine; this edit never conflicts.
    ForgetGarbage {
        ids: Vec<u64>,
    },
    ReserveSegmentIds {
        next: u64,
    },
    /// Plans `ops` against live `table` and records a job: reserves the target's table id and
    /// the job id; `snapshot` = the version this commit builds on.
    CreateJob {
        table: TableName,
        ops: Vec<Alter>,
    },
    /// Carries source segments into the job's target: `reused` ids are copied from the source
    /// and projected onto the target schema; `rewritten` ids' rows are now in `segments` (new
    /// files under the target's directory).
    AdvanceJob {
        job: u64,
        reused: Vec<u64>,
        rewritten: Vec<u64>,
        segments: Vec<SegmentEntry>,
    },
    /// Swaps the target in for the source (SPEC §19 step 3); the source is retired `Swapped`.
    SwapJob {
        job: u64,
    },
    /// Drops the job; its rewritten files go to garbage, reused ones stay with the source.
    CancelJob {
        job: u64,
    },
    RevertTable {
        table: TableName,
    },
    DropTable {
        table: TableName,
    },
    UndropTable {
        table: TableName,
    },
    /// Every segment goes to garbage, every tombstone goes; ids and definition are kept.
    TruncateTable {
        table: TableName,
    },
    /// Forgets retired entries retired before `before_ms`; releases their segments. Never
    /// conflicts.
    ExpireRetired {
        before_ms: u64,
    },
}

/// A batch of edits applied together against a recorded `base` version. `base` is kept for
/// audit only: the scope gate chose per-table OCC, so only the edits below decide a conflict.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub base: u64,
    pub edits: Vec<Edit>,
}

impl Commit {
    /// OCC check and apply: returns the next manifest, `version = current.version + 1`.
    pub fn apply(&self, current: &Manifest, now_ms: u64) -> Result<Manifest, Error> {
        let mut next = current.clone();
        for edit in &self.edits {
            apply_edit(&mut next, edit, now_ms)?;
        }
        next.version = current.version + 1;
        Ok(next)
    }
}

fn apply_edit(m: &mut Manifest, edit: &Edit, now_ms: u64) -> Result<(), Error> {
    match edit {
        Edit::CreateTable { spec } => create_table(m, spec),
        Edit::AddSegments { table, segments } => add_segments(m, table, segments),
        Edit::RemoveSegments { table, ids } => remove_segments(m, table, ids, now_ms),
        Edit::AddTombstone { table, predicates } => add_tombstone(m, table, predicates),
        Edit::ForgetGarbage { ids } => {
            forget_garbage(m, ids);
            Ok(())
        }
        Edit::ReserveSegmentIds { next } => {
            reserve_segment_ids(m, *next);
            Ok(())
        }
        Edit::CreateJob { table, ops } => create_job(m, table, ops),
        Edit::AdvanceJob {
            job,
            reused,
            rewritten,
            segments,
        } => advance_job(m, *job, reused, rewritten, segments),
        Edit::SwapJob { job } => swap_job(m, *job, now_ms),
        Edit::CancelJob { job } => cancel_job(m, *job, now_ms),
        Edit::RevertTable { table } => revert_table(m, table, now_ms),
        Edit::DropTable { table } => drop_table(m, table, now_ms),
        Edit::UndropTable { table } => undrop_table(m, table),
        Edit::TruncateTable { table } => truncate_table(m, table, now_ms),
        Edit::ExpireRetired { before_ms } => {
            expire_retired(m, *before_ms, now_ms);
            Ok(())
        }
    }
}

fn table_index(m: &Manifest, table: &TableName) -> Result<usize, Error> {
    m.tables
        .iter()
        .position(|t| &t.name == table)
        .ok_or_else(|| Error::Conflict {
            table: table.to_string(),
            detail: "table does not exist".to_string(),
        })
}

/// Validates `spec` (so every SPEC §18 rule is reachable through a commit alone, with no
/// store), then assigns the table its id and every column a field id (D0012).
fn create_table(m: &mut Manifest, spec: &TableSpec) -> Result<(), Error> {
    spec.validate()?;
    if m.table(&spec.name).is_some() {
        return Err(Error::Conflict {
            table: spec.name.to_string(),
            detail: "table already exists".to_string(),
        });
    }

    let id = TableId(m.next_table_id);
    m.next_table_id += 1;

    let schema: Vec<SchemaField> = spec
        .columns
        .iter()
        .enumerate()
        .map(|(i, field)| SchemaField {
            id: FieldId(i as u64 + 1),
            field: field.clone(),
        })
        .collect();
    let next_field_id = schema.len() as u64 + 1;
    let field_id_of = |name: &str| -> FieldId {
        schema
            .iter()
            .find(|f| f.field.name == name)
            .expect("validate already checked every clause names a real column")
            .id
    };

    // Resolved while `field_id_of` still borrows `schema`, and only then moved into `entry`:
    // the literal below would otherwise move `schema` before these fields could use it.
    let key: Vec<FieldId> = spec.key.iter().map(|c| field_id_of(c)).collect();
    let version = spec.version.as_deref().map(field_id_of);
    let order_by: Vec<FieldId> = spec
        .resolved_order_by()
        .iter()
        .map(|c| field_id_of(c))
        .collect();
    let partition_by = spec.partition_by.as_ref().map(|(c, bucket)| PartitionBy {
        column: field_id_of(c),
        bucket: *bucket,
    });
    let ttl = spec.ttl.as_ref().map(|(c, after)| Ttl {
        column: field_id_of(c),
        after: *after,
    });

    let entry = TableEntry {
        id,
        name: spec.name.clone(),
        engine: spec.engine.clone(),
        schema,
        next_field_id,
        key,
        version,
        order_by,
        partition_by,
        ttl,
        options: spec.options.clone(),
        segments: Vec::new(),
        tombstones: Vec::new(),
    };
    let pos = m.tables.partition_point(|t| t.name < spec.name);
    m.tables.insert(pos, entry);
    Ok(())
}

fn add_segments(
    m: &mut Manifest,
    table: &TableName,
    segments: &[SegmentEntry],
) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    let schema_ids: Vec<FieldId> = m.tables[idx].schema.iter().map(|f| f.id).collect();
    validate_new_segments(table, &schema_ids, segments)?;
    let entry = &mut m.tables[idx];
    entry.segments.extend(segments.iter().cloned());
    entry.segments.sort_by_key(|s| (s.seq, s.id));
    let max_id = entry.segments.iter().map(|s| s.id).max();
    if let Some(max_id) = max_id {
        m.next_segment_id = m.next_segment_id.max(max_id + 1);
    }
    Ok(())
}

/// The checks a batch of new segments must pass against a schema: column-stat count, field-id
/// alignment and a well-formed `dir` (SPEC §5). Shared by `AddSegments` and `AdvanceJob`.
fn validate_new_segments(
    table: &TableName,
    schema_ids: &[FieldId],
    segments: &[SegmentEntry],
) -> Result<(), Error> {
    for s in segments {
        if s.columns.len() != schema_ids.len() {
            return Err(Error::Usage(format!(
                "segment {} has {} column stats, table {table} has {} columns",
                s.id,
                s.columns.len(),
                schema_ids.len()
            )));
        }
        if s.field_ids != schema_ids {
            return Err(Error::Usage(format!(
                "segment {}: field ids {:?} do not match table {table}'s {:?}",
                s.id,
                ids_of(&s.field_ids),
                ids_of(schema_ids)
            )));
        }
        if s.dir.is_empty() || !s.dir.split('/').all(super::is_path_component) {
            return Err(Error::Usage(format!(
                "segment {}: dir {:?} is not a valid table directory",
                s.id, s.dir
            )));
        }
    }
    Ok(())
}

fn ids_of(ids: &[FieldId]) -> Vec<u64> {
    ids.iter().map(|id| id.0).collect()
}

fn remove_segments(
    m: &mut Manifest,
    table: &TableName,
    ids: &[u64],
    now_ms: u64,
) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    for &id in ids {
        if !m.tables[idx].segments.iter().any(|s| s.id == id) {
            return Err(Error::Conflict {
                table: table.to_string(),
                detail: format!("segment {id} no longer live"),
            });
        }
    }
    let mut removed = Vec::new();
    m.tables[idx].segments.retain(|s| {
        if ids.contains(&s.id) {
            removed.push(s.clone());
            false
        } else {
            true
        }
    });
    release(m, table, removed, now_ms);
    Ok(())
}

/// Moves each segment to garbage unless something still references it (D0012: a reused
/// segment is shared by a swapped table and the one kept for REVERT) or it is already there.
fn release(m: &mut Manifest, table: &TableName, segments: Vec<SegmentEntry>, now_ms: u64) {
    for mut segment in segments {
        if m.references_segment(segment.id) || m.garbage.iter().any(|g| g.segment.id == segment.id)
        {
            continue;
        }
        segment.columns.clear();
        segment.field_ids.clear();
        segment.file_field_ids.clear();
        m.garbage.push(Garbage {
            table: table.clone(),
            segment,
            removed_at_ms: now_ms,
        });
    }
}

fn table_index_by_id(m: &Manifest, id: TableId) -> Option<usize> {
    m.tables.iter().position(|t| t.id == id)
}

fn create_job(m: &mut Manifest, table: &TableName, ops: &[Alter]) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    if let Some(job) = m.job_for(table) {
        return Err(Error::JobRunning {
            table: table.to_string(),
            job: job.id,
        });
    }
    let mut target = alter::plan_alter(&m.tables[idx], ops)?;
    let source_id = m.tables[idx].id;
    target.id = TableId(m.next_table_id);
    m.next_table_id += 1;
    let job = Job {
        id: m.next_job_id,
        source: source_id,
        snapshot: m.version,
        target,
        handled: Vec::new(),
        reused: 0,
        rewritten: 0,
    };
    m.next_job_id += 1;
    m.jobs.push(job);
    Ok(())
}

fn advance_job(
    m: &mut Manifest,
    job_id: u64,
    reused: &[u64],
    rewritten: &[u64],
    segments: &[SegmentEntry],
) -> Result<(), Error> {
    let job_idx = m
        .jobs
        .iter()
        .position(|j| j.id == job_id)
        .ok_or(Error::NoSuchJob { job: job_id })?;
    let source_idx = table_index_by_id(m, m.jobs[job_idx].source).ok_or_else(|| Error::Conflict {
        table: m.jobs[job_idx].target.name.to_string(),
        detail: "source table does not exist".to_string(),
    })?;
    let table_name = m.tables[source_idx].name.clone();

    for &id in reused.iter().chain(rewritten.iter()) {
        let live = m.tables[source_idx].segments.iter().any(|s| s.id == id);
        let handled = m.jobs[job_idx].handled.contains(&id);
        if !live || handled {
            return Err(Error::Conflict {
                table: table_name.to_string(),
                detail: format!("segment {id} already handled / not live"),
            });
        }
    }

    let target_schema = m.jobs[job_idx].target.schema.clone();
    let schema_ids: Vec<FieldId> = target_schema.iter().map(|f| f.id).collect();
    validate_new_segments(&table_name, &schema_ids, segments)?;

    let mut new_segments: Vec<SegmentEntry> = reused
        .iter()
        .map(|&id| {
            m.tables[source_idx]
                .segments
                .iter()
                .find(|s| s.id == id)
                .expect("checked live above")
                .project_onto(&target_schema)
        })
        .collect();
    new_segments.extend(segments.iter().cloned());

    let job = &mut m.jobs[job_idx];
    job.target.segments.extend(new_segments);
    job.target.segments.sort_by_key(|s| (s.seq, s.id));
    job.handled
        .extend(reused.iter().chain(rewritten.iter()).copied());
    job.handled.sort_unstable();
    job.reused += reused.len() as u64;
    job.rewritten += rewritten.len() as u64;

    let max_id = job.target.segments.iter().map(|s| s.id).max();
    if let Some(max_id) = max_id {
        m.next_segment_id = m.next_segment_id.max(max_id + 1);
    }
    Ok(())
}

fn swap_job(m: &mut Manifest, job_id: u64, now_ms: u64) -> Result<(), Error> {
    let job_idx = m
        .jobs
        .iter()
        .position(|j| j.id == job_id)
        .ok_or(Error::NoSuchJob { job: job_id })?;
    let source_idx = table_index_by_id(m, m.jobs[job_idx].source).ok_or_else(|| Error::Conflict {
        table: m.jobs[job_idx].target.name.to_string(),
        detail: "source table does not exist".to_string(),
    })?;

    if let Some(id) = m.tables[source_idx]
        .segments
        .iter()
        .map(|s| s.id)
        .find(|id| !m.jobs[job_idx].handled.contains(id))
    {
        return Err(Error::Conflict {
            table: m.tables[source_idx].name.to_string(),
            detail: format!("catch-up incomplete: segment {id}"),
        });
    }

    let source = m.tables.remove(source_idx);
    let mut job = m.jobs.remove(job_idx);

    // Rename each tombstone predicate's column from the source's schema to the target's, by
    // field id; rule 3d of `plan_alter` makes a target missing the column impossible.
    let mut tombstones = Vec::with_capacity(source.tombstones.len());
    for t in &source.tombstones {
        let mut predicates = Vec::with_capacity(t.predicates.len());
        for p in &t.predicates {
            let id = source
                .schema
                .iter()
                .find(|f| f.field.name == p.column)
                .map(|f| f.id)
                .ok_or_else(|| {
                    Error::Usage(format!(
                        "table {}: tombstone at seq {} names unknown column {}",
                        source.name, t.seq, p.column
                    ))
                })?;
            let name = job
                .target
                .field(id)
                .map(|f| f.field.name.clone())
                .ok_or_else(|| {
                    Error::Usage(format!(
                        "table {}: tombstone at seq {} names column {} the migration target lacks",
                        source.name, t.seq, p.column
                    ))
                })?;
            predicates.push(Predicate {
                column: name,
                ..p.clone()
            });
        }
        tombstones.push(Tombstone {
            seq: t.seq,
            predicates,
        });
    }
    job.target.tombstones = tombstones;

    let mut successor_segments: Vec<u64> = job.target.segments.iter().map(|s| s.id).collect();
    successor_segments.sort_unstable();

    m.retired.push(Retired {
        entry: source,
        reason: RetireReason::Swapped,
        retired_at_ms: now_ms,
        version: m.version + 1,
        successor_segments,
    });

    let pos = m.tables.partition_point(|t| t.name < job.target.name);
    m.tables.insert(pos, job.target);
    Ok(())
}

fn cancel_job(m: &mut Manifest, job_id: u64, now_ms: u64) -> Result<(), Error> {
    let job_idx = m
        .jobs
        .iter()
        .position(|j| j.id == job_id)
        .ok_or(Error::NoSuchJob { job: job_id })?;
    let job = m.jobs.remove(job_idx);
    let table = job.target.name.clone();
    release(m, &table, job.target.segments, now_ms);
    Ok(())
}

fn revert_table(m: &mut Manifest, table: &TableName, now_ms: u64) -> Result<(), Error> {
    let r_idx = m
        .retired
        .iter()
        .rposition(|r| r.reason == RetireReason::Swapped && &r.entry.name == table)
        .ok_or_else(|| Error::NothingToRevert {
            table: table.to_string(),
        })?;
    let cur_idx = m
        .tables
        .iter()
        .position(|t| &t.name == table)
        .ok_or_else(|| Error::NothingToRevert {
            table: table.to_string(),
        })?;
    if let Some(job) = m.job_for(table) {
        return Err(Error::JobRunning {
            table: table.to_string(),
            job: job.id,
        });
    }

    let live_ids: std::collections::HashSet<u64> =
        m.tables[cur_idx].segments.iter().map(|s| s.id).collect();
    for &id in &m.retired[r_idx].successor_segments {
        if !live_ids.contains(&id) {
            return Err(Error::RevertStale {
                table: table.to_string(),
                detail: format!("segment {id} was compacted or removed since the swap"),
            });
        }
    }

    let retired = m.retired.remove(r_idx);
    let cur = m.tables.remove(cur_idx);

    let mut restored = retired.entry.clone();
    restored.next_field_id = restored.next_field_id.max(cur.next_field_id);

    let successor_ids: std::collections::HashSet<u64> =
        retired.successor_segments.iter().copied().collect();
    let mut segments = restored.segments.clone();
    for s in &cur.segments {
        if !successor_ids.contains(&s.id) {
            segments.push(s.project_onto(&restored.schema));
        }
    }
    segments.sort_by_key(|s| (s.seq, s.id));
    restored.segments = segments;

    let mut tombstones = restored.tombstones.clone();
    for t in cur.tombstones.iter().filter(|t| t.seq > retired.version) {
        let mut predicates = Vec::with_capacity(t.predicates.len());
        for p in &t.predicates {
            let id = cur
                .schema
                .iter()
                .find(|f| f.field.name == p.column)
                .map(|f| f.id)
                .ok_or_else(|| Error::RevertStale {
                    table: table.to_string(),
                    detail: format!(
                        "tombstone at seq {} names unknown column {}",
                        t.seq, p.column
                    ),
                })?;
            let name = restored.field(id).map(|f| f.field.name.clone()).ok_or_else(|| {
                Error::RevertStale {
                    table: table.to_string(),
                    detail: format!(
                        "tombstone at seq {} names column {} the reverted table lacks",
                        t.seq, p.column
                    ),
                }
            })?;
            predicates.push(Predicate {
                column: name,
                ..p.clone()
            });
        }
        tombstones.push(Tombstone {
            seq: t.seq,
            predicates,
        });
    }
    restored.tombstones = tombstones;

    m.retired.push(Retired {
        entry: cur,
        reason: RetireReason::Reverted,
        retired_at_ms: now_ms,
        version: m.version + 1,
        successor_segments: Vec::new(),
    });

    let pos = m.tables.partition_point(|t| t.name < restored.name);
    m.tables.insert(pos, restored);
    Ok(())
}

fn drop_table(m: &mut Manifest, table: &TableName, now_ms: u64) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    if let Some(job) = m.job_for(table) {
        return Err(Error::JobRunning {
            table: table.to_string(),
            job: job.id,
        });
    }
    let entry = m.tables.remove(idx);
    m.retired.push(Retired {
        entry,
        reason: RetireReason::Dropped,
        retired_at_ms: now_ms,
        version: m.version + 1,
        successor_segments: Vec::new(),
    });
    Ok(())
}

fn undrop_table(m: &mut Manifest, table: &TableName) -> Result<(), Error> {
    let r_idx = m
        .retired
        .iter()
        .rposition(|r| r.reason == RetireReason::Dropped && &r.entry.name == table)
        .ok_or_else(|| Error::NotDropped {
            table: table.to_string(),
        })?;
    if m.table(table).is_some() {
        return Err(Error::Conflict {
            table: table.to_string(),
            detail: "table already exists".to_string(),
        });
    }
    let retired = m.retired.remove(r_idx);
    let pos = m.tables.partition_point(|t| t.name < retired.entry.name);
    m.tables.insert(pos, retired.entry);
    Ok(())
}

fn truncate_table(m: &mut Manifest, table: &TableName, now_ms: u64) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    if let Some(job) = m.job_for(table) {
        return Err(Error::JobRunning {
            table: table.to_string(),
            job: job.id,
        });
    }
    let segments = std::mem::take(&mut m.tables[idx].segments);
    m.tables[idx].tombstones.clear();
    release(m, table, segments, now_ms);
    Ok(())
}

fn expire_retired(m: &mut Manifest, before_ms: u64, now_ms: u64) {
    let expired: Vec<Retired> = {
        let mut kept = Vec::with_capacity(m.retired.len());
        let mut expired = Vec::new();
        for r in m.retired.drain(..) {
            if r.retired_at_ms < before_ms {
                expired.push(r);
            } else {
                kept.push(r);
            }
        }
        m.retired = kept;
        expired
    };
    for r in expired {
        release(m, &r.entry.name, r.entry.segments, now_ms);
    }
}

fn add_tombstone(
    m: &mut Manifest,
    table: &TableName,
    predicates: &[Predicate],
) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    let schema = &m.tables[idx].schema;
    let mut stored = Vec::with_capacity(predicates.len());
    for p in predicates {
        if p.value.is_null() {
            return Err(Error::Usage(format!(
                "tombstone predicate on {} has a null value",
                p.column
            )));
        }
        let field = schema
            .iter()
            .find(|f| f.field.name == p.column)
            .ok_or_else(|| Error::Usage(format!("unknown column {}", p.column)))?;
        // Stored at the column's exact type, as `Column::push` does: the codec writes a DECIMAL's
        // unscaled digits and reads them back at the column's scale.
        let value = coerce(&p.value, &field.field.ty)
            .filter(|v| value_matches_type(v, &field.field.ty))
            .ok_or_else(|| {
                Error::Usage(format!(
                    "tombstone predicate value for {} does not fit column type {}",
                    p.column, field.field.ty
                ))
            })?;
        stored.push(Predicate { value, ..p.clone() });
    }
    let seq = m.version + 1;
    m.tables[idx].tombstones.push(Tombstone {
        seq,
        predicates: stored,
    });
    Ok(())
}

/// The same value/type pairing `segment::value::encode_value` accepts, checked ahead of time so
/// a bad tombstone is `Usage` here rather than a panic at the next `Manifest::encode`.
fn value_matches_type(v: &Value, ty: &DataType) -> bool {
    matches!(
        (v, ty),
        (Value::Bool(_), DataType::Bool)
            | (Value::Int64(_), DataType::Int64)
            | (Value::UInt64(_), DataType::UInt64)
            | (Value::Float64(_), DataType::Float64)
            | (Value::Decimal(_), DataType::Decimal(_))
            | (Value::String(_), DataType::String)
            | (Value::Bytes(_), DataType::Bytes)
            | (Value::Timestamp(_), DataType::Timestamp)
            | (Value::Date(_), DataType::Date)
            | (Value::Uuid(_), DataType::Uuid)
            | (Value::Ip(_), DataType::Ip)
    )
}

fn forget_garbage(m: &mut Manifest, ids: &[u64]) {
    m.garbage.retain(|g| !ids.contains(&g.segment.id));
}

fn reserve_segment_ids(m: &mut Manifest, next: u64) {
    m.next_segment_id = m.next_segment_id.max(next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ColumnStats, Field};
    use crate::storage::manifest::CmpOp;
    use crate::types::Decimal;
    use std::time::Duration;

    fn schema() -> Vec<Field> {
        vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }]
    }

    fn spec() -> TableSpec {
        TableSpec::new(TableName::new("d", "t"), schema())
    }

    fn base_with_table() -> Manifest {
        let mut m = Manifest::empty();
        m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable { spec: spec() }],
        }
        .apply(&m, 0)
        .unwrap();
        m
    }

    fn table_dir(m: &Manifest) -> String {
        m.table(&TableName::new("d", "t")).unwrap().dir()
    }

    fn seg(id: u64, seq: u64, dir: &str, field_ids: Vec<u64>) -> SegmentEntry {
        SegmentEntry {
            id,
            partition: "_".to_string(),
            seq,
            rows: 1,
            bytes: 1,
            footer_crc: 0,
            columns: vec![ColumnStats {
                rows: 1,
                null_count: 0,
                min: Some(Value::Int64(0)),
                max: Some(Value::Int64(0)),
            }],
            side_files: Vec::new(),
            dir: dir.to_string(),
            field_ids: field_ids.into_iter().map(FieldId).collect(),
            file_field_ids: Vec::new(),
        }
    }

    #[test]
    fn a_table_name_that_could_escape_the_store_root_is_usage() {
        for (db, name) in [
            ("..", "t"),
            ("d", ".."),
            ("d", "a/b"),
            ("", "t"),
            ("d", "."),
            ("/abs", "t"),
        ] {
            let commit = Commit {
                base: 0,
                edits: vec![Edit::CreateTable {
                    spec: TableSpec::new(TableName::new(db, name), schema()),
                }],
            };
            assert!(
                matches!(commit.apply(&Manifest::empty(), 0), Err(Error::Usage(_))),
                "{db:?}.{name:?}"
            );
        }
    }

    #[test]
    fn duplicate_create_table_conflicts() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::CreateTable { spec: spec() }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Conflict { .. })));
    }

    #[test]
    fn creating_two_tables_gives_ids_one_then_two_and_bumps_next_table_id() {
        let m = Manifest::empty();
        let m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable { spec: spec() }],
        }
        .apply(&m, 0)
        .unwrap();
        let other = TableSpec::new(TableName::new("d", "u"), schema());
        let m = Commit {
            base: m.version,
            edits: vec![Edit::CreateTable { spec: other }],
        }
        .apply(&m, 0)
        .unwrap();
        assert_eq!(m.table(&TableName::new("d", "t")).unwrap().id, TableId(1));
        assert_eq!(m.table(&TableName::new("d", "u")).unwrap().id, TableId(2));
        assert_eq!(m.next_table_id, 3);
    }

    #[test]
    fn a_keyed_engine_with_no_order_by_stores_order_by_equal_to_key() {
        let created = TableSpec::new(
            TableName::new("d", "t"),
            vec![Field {
                name: "id".to_string(),
                ty: DataType::Int64,
            }],
        )
        .engine("latest")
        .key(["id"]);
        let m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: created.clone(),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap();
        let entry = m.table(&TableName::new("d", "t")).unwrap();
        assert_eq!(entry.order_by, entry.key);
        // The stored default reads back as the implicit clause it was written as.
        assert_eq!(entry.spec(), created);
    }

    #[test]
    fn a_bad_spec_is_rejected_with_its_validate_variant() {
        let commit = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: spec().key(["a"]),
            }],
        };
        assert!(matches!(
            commit.apply(&Manifest::empty(), 0),
            Err(Error::KeyNotAllowed { .. })
        ));
    }

    #[test]
    fn append_from_a_stale_base_succeeds() {
        let m = base_with_table();
        let dir = table_dir(&m);
        let commit = Commit {
            base: 0, // stale: m.version is already 1
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(1, 1, &dir, vec![1])],
            }],
        };
        let next = commit.apply(&m, 0).unwrap();
        assert_eq!(next.version, m.version + 1);
        assert_eq!(
            next.table(&TableName::new("d", "t"))
                .unwrap()
                .segments
                .len(),
            1
        );
    }

    #[test]
    fn add_segments_rejects_a_column_count_mismatch() {
        let m = base_with_table();
        let dir = table_dir(&m);
        let mut bad = seg(1, 1, &dir, vec![1]);
        bad.columns.clear();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![bad],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Usage(_))));
    }

    #[test]
    fn add_segments_rejects_field_ids_that_do_not_match_the_schema() {
        let m = base_with_table();
        let dir = table_dir(&m);
        let bad = seg(1, 1, &dir, vec![99]);
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![bad],
            }],
        };
        let err = commit.apply(&m, 0).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
        assert!(err.to_string().contains("field ids"));
    }

    #[test]
    fn add_segments_rejects_an_empty_dir() {
        let m = base_with_table();
        let bad = seg(1, 1, "", vec![1]);
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![bad],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Usage(_))));
    }

    #[test]
    fn add_segments_bumps_next_segment_id_and_sorts_by_seq_then_id() {
        let m = base_with_table();
        let dir = table_dir(&m);
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(5, 2, &dir, vec![1]), seg(1, 1, &dir, vec![1])],
            }],
        };
        let next = commit.apply(&m, 0).unwrap();
        let table = next.table(&TableName::new("d", "t")).unwrap();
        let ids: Vec<u64> = table.segments.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![1, 5]);
        assert_eq!(next.next_segment_id, 6);
    }

    #[test]
    fn removing_the_same_segment_twice_conflicts_on_the_second_commit() {
        let m = base_with_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(7, 1, &dir, vec![1])],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let remove = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: TableName::new("d", "t"),
                ids: vec![7],
            }],
        };
        let after_first = remove.apply(&m, 100).unwrap();
        assert!(matches!(
            remove.apply(&after_first, 200),
            Err(Error::Conflict { .. })
        ));
    }

    #[test]
    fn remove_segments_moves_the_segment_to_garbage_with_removed_at_ms() {
        let m = base_with_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(7, 1, &dir, vec![1])],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let next = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: TableName::new("d", "t"),
                ids: vec![7],
            }],
        }
        .apply(&m, 4242)
        .unwrap();

        assert!(
            next.table(&TableName::new("d", "t"))
                .unwrap()
                .segments
                .is_empty()
        );
        assert_eq!(next.garbage.len(), 1);
        assert_eq!(next.garbage[0].removed_at_ms, 4242);
        assert!(next.garbage[0].segment.columns.is_empty());
        assert!(next.garbage[0].segment.field_ids.is_empty());
        assert_eq!(next.garbage[0].segment.dir, dir);
    }

    #[test]
    fn a_concurrent_flush_does_not_conflict_with_a_compaction_remove() {
        // Two compactions from the same base that remove the same segment conflict (above);
        // a flush (AddSegments) committed between a compaction's read and its publish must not.
        let m = base_with_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(7, 1, &dir, vec![1])],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let flush = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(8, 2, &dir, vec![1])],
            }],
        };
        let compact = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: TableName::new("d", "t"),
                ids: vec![7],
            }],
        };
        let after_flush = flush.apply(&m, 0).unwrap();
        let after_compact = compact.apply(&after_flush, 0).unwrap();
        let ids: Vec<u64> = after_compact
            .table(&TableName::new("d", "t"))
            .unwrap()
            .segments
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec![8]);
    }

    #[test]
    fn tombstone_seq_is_the_new_version() {
        let m = base_with_table();
        let next = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "t"),
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        assert_eq!(
            next.table(&TableName::new("d", "t")).unwrap().tombstones[0].seq,
            next.version
        );
    }

    fn decimal_table(precision: u8, scale: u8) -> Manifest {
        Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(
                    TableName::new("d", "p"),
                    vec![Field {
                        name: "price".to_string(),
                        ty: DataType::decimal(precision, scale).unwrap(),
                    }],
                ),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap()
    }

    fn delete_price(m: &Manifest, value: Value) -> Result<Manifest, Error> {
        Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "p"),
                predicates: vec![Predicate {
                    column: "price".to_string(),
                    op: CmpOp::Eq,
                    value,
                }],
            }],
        }
        .apply(m, 0)
    }

    #[test]
    fn a_decimal_tombstone_is_rescaled_to_the_column_and_survives_a_reload() {
        // 10.5 at scale 1 into DECIMAL(10, 2): stored as 10.50, not read back as 1.05.
        let m = delete_price(
            &decimal_table(10, 2),
            Value::Decimal(Decimal::new(105, 1).unwrap()),
        )
        .unwrap();
        let reloaded = Manifest::decode("manifest", &m.encode()).unwrap();
        let t = &reloaded
            .table(&TableName::new("d", "p"))
            .unwrap()
            .tombstones[0];
        assert_eq!(
            t.predicates[0].value,
            Value::Decimal(Decimal::new(1050, 2).unwrap())
        );
    }

    #[test]
    fn a_decimal_tombstone_beyond_the_column_precision_is_usage() {
        // 1234567.89 cannot fit DECIMAL(3, 2); committing it would leave an undecodable manifest.
        let r = delete_price(
            &decimal_table(3, 2),
            Value::Decimal(Decimal::new(123_456_789, 2).unwrap()),
        );
        assert!(matches!(r, Err(Error::Usage(_))), "{r:?}");
    }

    #[test]
    fn tombstone_with_a_null_value_is_usage() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "t"),
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Null,
                }],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Usage(_))));
    }

    #[test]
    fn tombstone_on_an_unknown_column_is_usage() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "t"),
                predicates: vec![Predicate {
                    column: "missing".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Usage(_))));
    }

    #[test]
    fn add_segments_on_an_unknown_table_conflicts() {
        let m = Manifest::empty();
        let commit = Commit {
            base: 0,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Conflict { .. })));
    }

    #[test]
    fn reserve_segment_ids_only_moves_forward() {
        let m = base_with_table();
        let next = Commit {
            base: m.version,
            edits: vec![Edit::ReserveSegmentIds { next: 10 }],
        }
        .apply(&m, 0)
        .unwrap();
        assert_eq!(next.next_segment_id, 10);
        let after = Commit {
            base: next.version,
            edits: vec![Edit::ReserveSegmentIds { next: 3 }],
        }
        .apply(&next, 0)
        .unwrap();
        assert_eq!(after.next_segment_id, 10);
    }

    #[test]
    fn forget_garbage_ignores_missing_ids() {
        let m = base_with_table();
        let next = Commit {
            base: m.version,
            edits: vec![Edit::ForgetGarbage { ids: vec![999] }],
        }
        .apply(&m, 0)
        .unwrap();
        assert!(next.garbage.is_empty());
    }

    #[test]
    fn a_partition_by_and_ttl_round_trip_through_a_commit() {
        let m = Manifest::empty();
        let m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(
                    TableName::new("d", "t"),
                    vec![Field {
                        name: "ts".to_string(),
                        ty: DataType::Timestamp,
                    }],
                )
                .partition_by("ts", Duration::from_secs(3600))
                .ttl("ts", Duration::from_secs(86_400)),
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let entry = m.table(&TableName::new("d", "t")).unwrap();
        assert_eq!(
            entry.partition_by.unwrap().bucket,
            Duration::from_secs(3600)
        );
        assert_eq!(entry.ttl.unwrap().after, Duration::from_secs(86_400));
    }

    // ── migration jobs, REVERT, DROP/UNDROP, TRUNCATE (SPEC §19) ────────────────────────────

    fn two_col_spec() -> TableSpec {
        TableSpec::new(
            TableName::new("d", "t"),
            vec![
                Field {
                    name: "a".to_string(),
                    ty: DataType::Int64,
                },
                Field {
                    name: "b".to_string(),
                    ty: DataType::Int64,
                },
            ],
        )
    }

    fn base_with_two_col_table() -> Manifest {
        Commit {
            base: 0,
            edits: vec![Edit::CreateTable { spec: two_col_spec() }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap()
    }

    /// A segment with one `ColumnStats` per id in `field_ids`, for tables with more than one
    /// column (the top-level `seg` fixture is hardcoded to a single column).
    fn seg_for(id: u64, seq: u64, dir: &str, field_ids: Vec<u64>) -> SegmentEntry {
        let columns = field_ids
            .iter()
            .map(|_| ColumnStats {
                rows: 1,
                null_count: 0,
                min: Some(Value::Int64(0)),
                max: Some(Value::Int64(0)),
            })
            .collect();
        SegmentEntry {
            id,
            partition: "_".to_string(),
            seq,
            rows: 1,
            bytes: 1,
            footer_crc: 0,
            columns,
            side_files: Vec::new(),
            dir: dir.to_string(),
            field_ids: field_ids.into_iter().map(FieldId).collect(),
            file_field_ids: Vec::new(),
        }
    }

    fn start_job(m: &Manifest, ops: Vec<Alter>) -> (Manifest, u64) {
        let m = Commit {
            base: m.version,
            edits: vec![Edit::CreateJob {
                table: TableName::new("d", "t"),
                ops,
            }],
        }
        .apply(m, 0)
        .unwrap();
        let job_id = m.jobs[0].id;
        (m, job_id)
    }

    #[test]
    fn job_lifecycle_create_advance_swap_renames_tombstones_and_retires_the_source() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg_for(1, 1, &dir, vec![1, 2])],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: table.clone(),
                predicates: vec![Predicate {
                    column: "b".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let (m, job_id) = start_job(
            &m,
            vec![Alter::RenameColumn {
                from: "b".to_string(),
                to: "bb".to_string(),
            }],
        );
        let target_id = m.jobs[0].target.id;
        assert_ne!(target_id, m.table(&table).unwrap().id);

        let m = Commit {
            base: m.version,
            edits: vec![Edit::AdvanceJob {
                job: job_id,
                reused: vec![1],
                rewritten: vec![],
                segments: vec![],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        assert_eq!(m.jobs[0].handled, vec![1]);
        assert_eq!(m.jobs[0].target.segments.len(), 1);
        assert_eq!(m.jobs[0].target.segments[0].field_ids.len(), 2);

        let m = Commit {
            base: m.version,
            edits: vec![Edit::SwapJob { job: job_id }],
        }
        .apply(&m, 999)
        .unwrap();

        assert!(m.jobs.is_empty());
        let live = m.table(&table).unwrap();
        assert_eq!(live.id, target_id);
        assert_eq!(live.segments.len(), 1);
        assert_eq!(live.tombstones.len(), 1);
        assert_eq!(live.tombstones[0].predicates[0].column, "bb");

        let retired = m
            .retired
            .iter()
            .find(|r| r.reason == RetireReason::Swapped)
            .unwrap();
        assert_eq!(retired.retired_at_ms, 999);
        assert_eq!(retired.successor_segments, vec![1]);
    }

    #[test]
    fn swap_with_an_unhandled_segment_conflicts() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg_for(1, 1, &dir, vec![1, 2])],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let (m, job_id) = start_job(
            &m,
            vec![Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        );
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::SwapJob { job: job_id }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Conflict { .. })));
    }

    #[test]
    fn advance_job_handling_a_segment_twice_conflicts() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg_for(1, 1, &dir, vec![1, 2])],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let (m, job_id) = start_job(
            &m,
            vec![Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        );
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AdvanceJob {
                job: job_id,
                reused: vec![1],
                rewritten: vec![],
                segments: vec![],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AdvanceJob {
                job: job_id,
                reused: vec![1],
                rewritten: vec![],
                segments: vec![],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Conflict { .. })));
    }

    #[test]
    fn create_job_while_one_runs_is_job_running() {
        let m = base_with_two_col_table();
        let (m, _job_id) = start_job(
            &m,
            vec![Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        );
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::CreateJob {
                table: TableName::new("d", "t"),
                ops: vec![Alter::AddColumn(Field {
                    name: "d".to_string(),
                    ty: DataType::Int64,
                })],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::JobRunning { .. })));
    }

    #[test]
    fn remove_segments_does_not_garbage_a_segment_still_referenced_by_a_retired_entry() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg_for(1, 1, &dir, vec![1, 2])],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let (m, job_id) = start_job(
            &m,
            vec![Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        );
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AdvanceJob {
                job: job_id,
                reused: vec![1],
                rewritten: vec![],
                segments: vec![],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::SwapJob { job: job_id }],
        }
        .apply(&m, 0)
        .unwrap();

        // Today's `remove_segments` garbages unconditionally; it must not here, because the
        // retired entry from the swap still lists segment 1.
        let after_remove = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: table.clone(),
                ids: vec![1],
            }],
        }
        .apply(&m, 100)
        .unwrap();
        assert!(
            after_remove.garbage.is_empty(),
            "segment is still referenced by the retired entry"
        );

        let after_expire = Commit {
            base: after_remove.version,
            edits: vec![Edit::ExpireRetired { before_ms: u64::MAX }],
        }
        .apply(&after_remove, 200)
        .unwrap();
        assert_eq!(after_expire.garbage.len(), 1);
        assert_eq!(after_expire.garbage[0].segment.id, 1);
    }

    #[test]
    fn cancel_job_garbages_the_rewritten_segment_but_not_the_reused_one() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![
                    seg_for(1, 1, &dir, vec![1, 2]),
                    seg_for(2, 2, &dir, vec![1, 2]),
                ],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let (m, job_id) = start_job(&m, vec![Alter::OrderBy(vec!["b".to_string(), "a".to_string()])]);
        let target_dir = m.jobs[0].target.dir();
        let rewritten_new = seg_for(10, 1, &target_dir, vec![1, 2]);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AdvanceJob {
                job: job_id,
                reused: vec![1],
                rewritten: vec![2],
                segments: vec![rewritten_new],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let m = Commit {
            base: m.version,
            edits: vec![Edit::CancelJob { job: job_id }],
        }
        .apply(&m, 500)
        .unwrap();

        assert!(m.jobs.is_empty());
        // Segment 1 is reused: still live under the (untouched) source table, so not garbage.
        assert!(m.table(&table).unwrap().segments.iter().any(|s| s.id == 1));
        assert!(!m.garbage.iter().any(|g| g.segment.id == 1));
        // Segment 10 was only ever referenced by the now-cancelled job: garbage, exactly once.
        assert_eq!(
            m.garbage.iter().filter(|g| g.segment.id == 10).count(),
            1
        );
    }

    #[test]
    fn revert_restores_the_original_with_original_stats_and_post_swap_writes() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg_for(1, 1, &dir, vec![1, 2])],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let original = m.table(&table).unwrap().clone();

        let (m, job_id) = start_job(&m, vec![Alter::DropColumn("b".to_string())]);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AdvanceJob {
                job: job_id,
                reused: vec![1],
                rewritten: vec![],
                segments: vec![],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let swapped = Commit {
            base: m.version,
            edits: vec![Edit::SwapJob { job: job_id }],
        }
        .apply(&m, 0)
        .unwrap();

        // A write since the swap: a new segment (single column "a") and a tombstone on "a",
        // which exists on both sides of the migration.
        let post_dir = swapped.table(&table).unwrap().dir();
        let a_id = swapped.table(&table).unwrap().schema[0].id.0;
        let m = Commit {
            base: swapped.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg_for(2, swapped.version, &post_dir, vec![a_id])],
            }],
        }
        .apply(&swapped, 0)
        .unwrap();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: table.clone(),
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let reverted = Commit {
            base: m.version,
            edits: vec![Edit::RevertTable {
                table: table.clone(),
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let restored = reverted.table(&table).unwrap();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.schema, original.schema);
        assert_eq!(
            restored.next_field_id,
            original
                .next_field_id
                .max(swapped.table(&table).unwrap().next_field_id)
        );
        // The dropped column's original stats are back.
        let seg1 = restored.segments.iter().find(|s| s.id == 1).unwrap();
        assert_eq!(seg1.columns, original.segments[0].columns);
        // The post-swap write is carried over, projected onto the restored (two-column) schema.
        let seg2 = restored.segments.iter().find(|s| s.id == 2).unwrap();
        assert_eq!(seg2.field_ids.len(), 2);
        // The post-swap tombstone survived, still naming "a".
        assert!(
            restored
                .tombstones
                .iter()
                .any(|t| t.predicates.iter().any(|p| p.column == "a"))
        );
        assert!(reverted.jobs.is_empty());
    }

    #[test]
    fn revert_is_refused_when_a_successor_segment_is_gone() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let dir = table_dir(&m);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg_for(1, 1, &dir, vec![1, 2])],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let (m, job_id) = start_job(&m, vec![Alter::DropColumn("b".to_string())]);
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AdvanceJob {
                job: job_id,
                reused: vec![1],
                rewritten: vec![],
                segments: vec![],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::SwapJob { job: job_id }],
        }
        .apply(&m, 0)
        .unwrap();
        // The successor segment (id 1) is compacted away.
        let m = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: table.clone(),
                ids: vec![1],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let commit = Commit {
            base: m.version,
            edits: vec![Edit::RevertTable {
                table: table.clone(),
            }],
        };
        assert!(matches!(
            commit.apply(&m, 0),
            Err(Error::RevertStale { .. })
        ));
    }

    #[test]
    fn revert_is_refused_when_a_post_swap_tombstone_names_a_column_the_original_lacks() {
        let table = TableName::new("d", "t");
        let m = base_with_two_col_table();
        let (m, job_id) = start_job(
            &m,
            vec![Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        );
        // No segments to catch up on, so the swap needs no `AdvanceJob`.
        let m = Commit {
            base: m.version,
            edits: vec![Edit::SwapJob { job: job_id }],
        }
        .apply(&m, 0)
        .unwrap();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: table.clone(),
                predicates: vec![Predicate {
                    column: "c".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let commit = Commit {
            base: m.version,
            edits: vec![Edit::RevertTable {
                table: table.clone(),
            }],
        };
        assert!(matches!(
            commit.apply(&m, 0),
            Err(Error::RevertStale { .. })
        ));
    }

    #[test]
    fn revert_with_no_swap_is_nothing_to_revert() {
        let m = base_with_two_col_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::RevertTable {
                table: TableName::new("d", "t"),
            }],
        };
        assert!(matches!(
            commit.apply(&m, 0),
            Err(Error::NothingToRevert { .. })
        ));
    }

    #[test]
    fn drop_then_undrop_round_trips_keeping_the_table_id() {
        let m = base_with_table();
        let id = m.table(&TableName::new("d", "t")).unwrap().id;
        let m = Commit {
            base: m.version,
            edits: vec![Edit::DropTable {
                table: TableName::new("d", "t"),
            }],
        }
        .apply(&m, 0)
        .unwrap();
        assert!(m.table(&TableName::new("d", "t")).is_none());
        let m = Commit {
            base: m.version,
            edits: vec![Edit::UndropTable {
                table: TableName::new("d", "t"),
            }],
        }
        .apply(&m, 0)
        .unwrap();
        assert_eq!(m.table(&TableName::new("d", "t")).unwrap().id, id);
    }

    #[test]
    fn undrop_over_a_recreated_name_conflicts() {
        let m = base_with_table();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::DropTable {
                table: TableName::new("d", "t"),
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::CreateTable { spec: spec() }],
        }
        .apply(&m, 0)
        .unwrap();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::UndropTable {
                table: TableName::new("d", "t"),
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Conflict { .. })));
    }

    #[test]
    fn undrop_with_nothing_dropped_is_not_dropped() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::UndropTable {
                table: TableName::new("d", "t"),
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::NotDropped { .. })));
    }

    #[test]
    fn drop_with_a_running_job_is_job_running() {
        let m = base_with_two_col_table();
        let (m, _job_id) = start_job(
            &m,
            vec![Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        );
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::DropTable {
                table: TableName::new("d", "t"),
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::JobRunning { .. })));
    }

    #[test]
    fn truncate_clears_segments_and_tombstones_but_keeps_the_id_and_field_ids() {
        let m = base_with_table();
        let dir = table_dir(&m);
        let table = TableName::new("d", "t");
        let id = m.table(&table).unwrap().id;
        let field_ids: Vec<FieldId> = m.table(&table).unwrap().schema.iter().map(|f| f.id).collect();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg(1, 1, &dir, vec![field_ids[0].0])],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: table.clone(),
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let m = Commit {
            base: m.version,
            edits: vec![Edit::TruncateTable { table: table.clone() }],
        }
        .apply(&m, 42)
        .unwrap();

        let t = m.table(&table).unwrap();
        assert!(t.segments.is_empty());
        assert!(t.tombstones.is_empty());
        assert_eq!(t.id, id);
        assert_eq!(
            t.schema.iter().map(|f| f.id).collect::<Vec<_>>(),
            field_ids
        );
        assert_eq!(m.garbage.len(), 1);
        assert_eq!(m.garbage[0].removed_at_ms, 42);
    }
}
