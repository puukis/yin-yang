use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use streamd_proto::packets::{RemoteCursorShape, RemoteCursorState};

const MAX_CURSOR_STATES: usize = 512;
const MAX_CURSOR_SHAPES: usize = 32;
const MAX_CURSOR_STATE_AGE_US: u64 = 5_000_000;

#[derive(Default)]
pub struct RemoteCursorStore {
    inner: Mutex<CursorStoreInner>,
}

#[derive(Default)]
struct CursorStoreInner {
    shapes: HashMap<u64, Arc<RemoteCursorShape>>,
    shape_order: VecDeque<u64>,
    states: Vec<RemoteCursorState>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone)]
pub struct CursorSnapshot {
    pub state: RemoteCursorState,
    pub shape: Option<Arc<RemoteCursorShape>>,
}

impl RemoteCursorStore {
    pub fn apply_shape(&self, shape: RemoteCursorShape) {
        let mut inner = self.inner.lock().expect("remote cursor store poisoned");
        let generation = shape.generation;
        if !inner.shapes.contains_key(&generation) {
            inner.shape_order.push_back(generation);
        }
        inner.shapes.insert(generation, Arc::new(shape));

        while inner.shape_order.len() > MAX_CURSOR_SHAPES {
            if let Some(old_generation) = inner.shape_order.pop_front() {
                inner.shapes.remove(&old_generation);
            }
        }
    }

    pub fn apply_state(&self, state: RemoteCursorState) {
        let mut inner = self.inner.lock().expect("remote cursor store poisoned");
        let insert_at = inner
            .states
            .partition_point(|existing| existing.timestamp_us <= state.timestamp_us);

        if inner
            .states
            .get(insert_at.saturating_sub(1))
            .is_some_and(|existing| existing == &state)
            || inner
                .states
                .get(insert_at)
                .is_some_and(|existing| existing == &state)
        {
            return;
        }

        inner.states.insert(insert_at, state);

        if let Some(latest_timestamp) = inner.states.last().map(|state| state.timestamp_us) {
            let cutoff = latest_timestamp.saturating_sub(MAX_CURSOR_STATE_AGE_US);
            let first_keep = inner
                .states
                .partition_point(|existing| existing.timestamp_us < cutoff);
            if first_keep > 0 {
                inner.states.drain(0..first_keep);
            }
        }

        if inner.states.len() > MAX_CURSOR_STATES {
            let remove_count = inner.states.len() - MAX_CURSOR_STATES;
            inner.states.drain(0..remove_count);
        }
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn snapshot_for(&self, timestamp_us: u64) -> Option<CursorSnapshot> {
        let inner = self.inner.lock().expect("remote cursor store poisoned");
        let idx = inner
            .states
            .partition_point(|state| state.timestamp_us <= timestamp_us);
        let state = if idx == 0 {
            inner.states.first().cloned()
        } else {
            inner.states.get(idx - 1).cloned()
        }?;
        let shape = inner.shapes.get(&state.generation).cloned();
        Some(CursorSnapshot { state, shape })
    }
}
