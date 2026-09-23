//! Judges a reopened store against what the parent saw the child send and ack. Pure: no IO,
//! so Miri can run it directly (SPEC §6, §13).

use std::collections::{BTreeMap, BTreeSet};

use super::Row;

/// A judged defect, before the run/dir context that `crash::run` attaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Defect {
    LostAck {
        batch: u64,
    },
    Torn {
        batch: u64,
        present: usize,
        expected: usize,
    },
    Phantom {
        batch: u64,
    },
    Duplicate {
        batch: u64,
        row: u32,
    },
}

/// `acked`: batches `0..acked` were acknowledged. `highest_sent`: the highest batch the
/// child ever announced sending. Torn applies to any sent batch, acked or not: a write must
/// be all-or-nothing; only an unacked batch is also allowed to be simply empty.
pub(super) fn verdict(
    acked: u64,
    highest_sent: Option<u64>,
    rows_per_batch: u32,
    rows: &[Row],
) -> Result<(), Defect> {
    let mut per_batch: BTreeMap<u64, BTreeSet<u32>> = BTreeMap::new();
    for &(batch, idx) in rows {
        if !per_batch.entry(batch).or_default().insert(idx) {
            return Err(Defect::Duplicate { batch, row: idx });
        }
    }

    let expected = rows_per_batch as usize;
    let sent_ceiling = highest_sent.map_or(0, |h| h + 1);
    for batch in 0..sent_ceiling {
        let present = per_batch.get(&batch).map_or(0, BTreeSet::len);
        if present == 0 {
            if batch < acked {
                return Err(Defect::LostAck { batch });
            }
            continue;
        }
        if present != expected {
            return Err(Defect::Torn {
                batch,
                present,
                expected,
            });
        }
    }

    for &batch in per_batch.keys() {
        if batch >= sent_ceiling {
            return Err(Defect::Phantom { batch });
        }
    }
    Ok(())
}
