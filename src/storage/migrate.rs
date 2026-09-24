//! The job runner (SPEC §19, D0012): `Store::run_job` advances one migration a bounded step at
//! a time, committing its progress so a crash loses at most one step.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::storage::manifest::{self, Edit, FieldId, Job, Manifest, SegmentEntry, TableEntry};
use crate::storage::segment::{self, WriterOptions};

use super::engines::engine_by_name;
use super::read;
use super::{Error, Shared, Store, io_err};

/// What one `run_job` step left behind.
#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running {
        job: u64,
        handled: usize,
        total: usize,
    },
    /// The swap committed at `version`; the job no longer exists.
    Swapped { version: u64 },
}

/// A job plus the live source it targets and the catch-up work still ahead of it.
struct Loaded {
    source: TableEntry,
    job: Job,
    /// Source segment ids not yet in `job.handled`, ascending (source segments are kept
    /// sorted by `(seq, id)`).
    gap: Vec<u64>,
    rewrite: bool,
}

fn load_job(shared: &Arc<Shared>, id: u64) -> Result<Loaded, Error> {
    let manifest = shared.state.lock().unwrap().current.manifest().clone();
    load_job_from(&manifest, id)
}

/// Shared by `load_job` and the swap's `commit_holding_state` plan, which is handed the
/// manifest under lock rather than reading it itself.
fn load_job_from(manifest: &Manifest, id: u64) -> Result<Loaded, Error> {
    let job = manifest
        .job(id)
        .cloned()
        .ok_or_else(|| Error::Manifest(manifest::Error::NoSuchJob { job: id }))?;
    let source = manifest
        .tables
        .iter()
        .find(|t| t.id == job.source)
        .cloned()
        .ok_or_else(|| {
            Error::Manifest(manifest::Error::Conflict {
                table: job.target.name.to_string(),
                detail: "source table does not exist".to_string(),
            })
        })?;
    let gap: Vec<u64> = source
        .segments
        .iter()
        .map(|s| s.id)
        .filter(|sid| !job.handled.contains(sid))
        .collect();
    let rewrite = manifest::rewrites_segments(&source, &job.target);
    Ok(Loaded {
        source,
        job,
        gap,
        rewrite,
    })
}

/// Rewrites `ids` onto `target`'s schema and ORDER BY (mirrors `compact::prepare`): each input
/// is read projected, merged by the target's engine, and written under `target.dir()` with a
/// fresh id but the source segment's own `seq` (so tombstones keep applying as before).
fn rewrite_segments(
    shared: &Arc<Shared>,
    ids: &[u64],
    source: &TableEntry,
    target: &TableEntry,
) -> Result<Vec<SegmentEntry>, Error> {
    let engine = engine_by_name(&target.engine).ok_or_else(|| {
        Error::Manifest(manifest::Error::UnknownEngine {
            table: target.name.to_string(),
            engine: target.engine.clone(),
        })
    })?;
    let fields = target.fields();
    let field_ids: Vec<FieldId> = target.schema.iter().map(|f| f.id).collect();
    let dir = target.dir();

    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for &id in ids {
        let seg =
            source.segments.iter().find(|s| s.id == id).ok_or_else(|| {
                Error::Usage(format!("migration: source segment {id} is not live"))
            })?;
        let batches = read::read_segment_as(&shared.root, seg, &target.schema)?;
        let outputs = engine.merge(target, vec![batches], shared.opts.max_rows)?;

        let partition_dir = Manifest::table_dir(&shared.root, &dir).join(&seg.partition);
        shared
            .io
            .create_dir_all(&partition_dir)
            .map_err(|e| io_err(&partition_dir, e))?;
        if !dirs.contains(&partition_dir) {
            dirs.push(partition_dir.clone());
        }

        for output in outputs {
            let new_id = shared.next_id.fetch_add(1, Ordering::SeqCst);
            let opts = WriterOptions {
                indexes: engine.indexes(&fields),
                ..Default::default()
            };
            let mut writer = segment::Writer::new(Vec::new(), fields.clone(), opts)?;
            for b in &output {
                writer.push(b)?;
            }
            let (bytes, meta) = writer.finish()?;
            let new_seg = SegmentEntry {
                id: new_id,
                partition: seg.partition.clone(),
                seq: seg.seq,
                rows: meta.rows,
                bytes: meta.bytes,
                footer_crc: meta.footer_crc,
                columns: meta.columns,
                side_files: Vec::new(),
                dir: dir.clone(),
                field_ids: field_ids.clone(),
                file_field_ids: Vec::new(),
            };
            let path = Manifest::segment_path(&shared.root, &new_seg);
            let file = shared
                .io
                .write_new(&path, &bytes)
                .map_err(|e| io_err(&path, e))?;
            crate::storage::fail::point("migrate.pre_segment_sync");
            shared
                .io
                .sync_file(&file, &path)
                .map_err(|e| io_err(&path, e))?;
            out.push(new_seg);
        }
    }
    for d in &dirs {
        shared.io.sync_dir(d).map_err(|e| io_err(d, e))?;
    }
    Ok(out)
}

/// One bounded step of `job` (SPEC §19 step 2/3): carries up to `job_step_segments` more source
/// segments into the target, or — once the remaining gap is small — flushes and swaps.
pub(crate) fn step(store: &Store, id: u64) -> Result<JobStatus, Error> {
    let shared = &store.shared;
    let loaded = load_job(shared, id)?;
    let table_name = loaded.source.name.clone();

    if loaded.gap.len() > shared.opts.job_swap_gap {
        let take = shared.opts.job_step_segments.min(loaded.gap.len());
        let chosen: Vec<u64> = loaded.gap[..take].to_vec();
        if loaded.rewrite {
            let segments = rewrite_segments(shared, &chosen, &loaded.source, &loaded.job.target)?;
            crate::storage::fail::point("migrate.pre_advance");
            shared.commit(
                move |_v| {
                    vec![Edit::AdvanceJob {
                        job: id,
                        reused: Vec::new(),
                        rewritten: chosen,
                        segments,
                    }]
                },
                &[],
            )?;
        } else {
            shared.commit(
                move |_v| {
                    vec![Edit::AdvanceJob {
                        job: id,
                        reused: chosen,
                        rewritten: Vec::new(),
                        segments: Vec::new(),
                    }]
                },
                &[],
            )?;
        }
        let after = load_job(shared, id)?;
        return Ok(JobStatus::Running {
            job: id,
            handled: after.job.handled.len(),
            total: after.source.segments.len(),
        });
    }

    for _ in 0..64 {
        store.flush()?;
        let loaded = load_job(shared, id)?;
        if loaded.rewrite && !loaded.gap.is_empty() {
            let segments =
                rewrite_segments(shared, &loaded.gap, &loaded.source, &loaded.job.target)?;
            crate::storage::fail::point("migrate.pre_advance");
            let rewritten = loaded.gap.clone();
            shared.commit(
                move |_v| {
                    vec![Edit::AdvanceJob {
                        job: id,
                        reused: Vec::new(),
                        rewritten,
                        segments,
                    }]
                },
                &[],
            )?;
        }

        let source_schema = loaded.source.schema.clone();
        let target_schema = loaded.job.target.schema.clone();
        let table_for_plan = table_name.clone();
        let table_for_after = table_name.clone();
        let result = shared.commit_holding_state(
            |manifest, state, _next| {
                let loaded = load_job_from(manifest, id)?;
                if state.in_flight.contains_key(&table_for_plan) {
                    return Ok(None);
                }
                if !loaded.gap.is_empty() && loaded.rewrite {
                    return Ok(None);
                }
                let mut edits = Vec::new();
                if !loaded.gap.is_empty() {
                    edits.push(Edit::AdvanceJob {
                        job: id,
                        reused: loaded.gap,
                        rewritten: Vec::new(),
                        segments: Vec::new(),
                    });
                }
                edits.push(Edit::SwapJob { job: id });
                crate::storage::fail::point("migrate.pre_swap");
                Ok(Some(edits))
            },
            move |state| {
                if let Some(batches) = state.pending.get_mut(&table_for_after) {
                    for b in batches.iter_mut() {
                        let projected = read::project_batch(b, &source_schema, &target_schema)
                            .unwrap_or_else(|e| {
                                panic!(
                                    "table {table_for_after}: a batch buffered against the old \
                                     schema failed to project onto the swap's target: {e}"
                                )
                            });
                        *b = Arc::new(projected);
                    }
                }
            },
        )?;
        if let Some(version) = result {
            return Ok(JobStatus::Swapped { version });
        }
    }

    let after = load_job(shared, id)?;
    Ok(JobStatus::Running {
        job: id,
        handled: after.job.handled.len(),
        total: after.source.segments.len(),
    })
}
