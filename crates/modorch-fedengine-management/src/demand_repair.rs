//! Bounded, single-flight priority repair queue for missing or stale relations.
use modorch_fedengine_contract::EngineMetrics;
use modorch_fedengine_core::{DataSourceId, EngineError, Result};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex, atomic::Ordering},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RepairTarget {
    pub source: DataSourceId,
    pub schema: String,
    pub table: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueStatus {
    Queued,
    Coalesced,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepairStats {
    pub pending: usize,
    pub enqueued: u64,
    pub coalesced: u64,
    pub completed: u64,
}

#[derive(Default)]
struct State {
    high: VecDeque<RepairTarget>,
    normal: VecDeque<RepairTarget>,
    queued_or_active: BTreeSet<RepairTarget>,
    enqueued: u64,
    coalesced: u64,
    completed: u64,
}

/// Bounded single-flight priority queue coalescing missing/stale relation repair requests.
#[derive(Clone)]
pub(super) struct DemandRepairQueue {
    max_pending: usize,
    state: Arc<Mutex<State>>,
}

impl Default for DemandRepairQueue {
    fn default() -> Self {
        Self::new(128)
    }
}

impl DemandRepairQueue {
    pub(super) fn new(max_pending: usize) -> Self {
        Self {
            max_pending,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    pub(super) fn enqueue(
        &self,
        target: RepairTarget,
        high_priority: bool,
        metrics: Option<&EngineMetrics>,
    ) -> Result<EnqueueStatus> {
        let mut s = self.state.lock().unwrap();
        if s.queued_or_active.contains(&target) {
            s.coalesced += 1;
            if let Some(m) = metrics {
                m.demand_repairs_coalesced.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(EnqueueStatus::Coalesced);
        }
        if s.high.len() + s.normal.len() >= self.max_pending {
            return Err(EngineError::new(
                "53300",
                "demand-repair queue is at capacity; retry after current source refresh",
            ));
        }
        s.queued_or_active.insert(target.clone());
        if high_priority {
            s.high.push_back(target);
        } else {
            s.normal.push_back(target);
        }
        s.enqueued += 1;
        Ok(EnqueueStatus::Queued)
    }

    #[cfg(test)]
    pub(super) fn pop_next(&self) -> Option<RepairTarget> {
        let mut s = self.state.lock().unwrap();
        s.high.pop_front().or_else(|| s.normal.pop_front())
    }

    #[cfg(test)]
    pub(super) fn complete(&self, target: &RepairTarget) {
        let mut s = self.state.lock().unwrap();
        if s.queued_or_active.remove(target) {
            s.completed += 1;
        }
    }

    pub(super) fn clear_source(&self, source: &DataSourceId) {
        let mut s = self.state.lock().unwrap();
        let before = s.queued_or_active.len();
        s.high.retain(|t| &t.source != source);
        s.normal.retain(|t| &t.source != source);
        s.queued_or_active.retain(|t| &t.source != source);
        let cleared = before.saturating_sub(s.queued_or_active.len());
        s.completed += cleared as u64;
    }

    pub(super) fn stats(&self) -> RepairStats {
        let s = self.state.lock().unwrap();
        RepairStats {
            pending: s.high.len() + s.normal.len(),
            enqueued: s.enqueued,
            coalesced: s.coalesced,
            completed: s.completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demand_repair_queue_coalesces_duplicates_prioritizes_high_and_bounds_capacity() {
        let metrics = EngineMetrics::default();
        let q = DemandRepairQueue::new(2);
        let src = DataSourceId("crm".into());
        let a = RepairTarget {
            source: src.clone(),
            schema: "public".into(),
            table: "a".into(),
        };
        let b = RepairTarget {
            source: src.clone(),
            schema: "public".into(),
            table: "b".into(),
        };
        let c = RepairTarget {
            source: src.clone(),
            schema: "public".into(),
            table: "c".into(),
        };

        assert_eq!(
            q.enqueue(a.clone(), false, Some(&metrics)).unwrap(),
            EnqueueStatus::Queued
        );
        assert_eq!(
            q.enqueue(a.clone(), false, Some(&metrics)).unwrap(),
            EnqueueStatus::Coalesced
        );
        assert_eq!(
            q.enqueue(b.clone(), true, Some(&metrics)).unwrap(),
            EnqueueStatus::Queued
        );
        assert_eq!(
            q.enqueue(c, false, Some(&metrics)).unwrap_err().sqlstate,
            "53300"
        );

        assert_eq!(q.pop_next(), Some(b.clone()));
        q.complete(&b);
        assert_eq!(q.pop_next(), Some(a.clone()));
        q.complete(&a);
        assert_eq!(q.pop_next(), None);

        let st = q.stats();
        assert_eq!(st.enqueued, 2);
        assert_eq!(st.coalesced, 1);
        assert_eq!(st.completed, 2);
        assert_eq!(metrics.snapshot().demand_repairs_coalesced, 1);

        q.enqueue(a.clone(), true, None).unwrap();
        q.clear_source(&src);
        assert_eq!(q.stats().pending, 0);
        assert_eq!(q.stats().completed, 3);
    }
}
