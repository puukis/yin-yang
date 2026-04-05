use std::sync::{Arc, Mutex};

use yin_yang_proto::packets::ClientTelemetry;

pub type SharedClientTelemetry = Arc<ClientTelemetryAccumulator>;

#[derive(Default)]
pub struct ClientTelemetryAccumulator {
    inner: Mutex<ClientTelemetryState>,
}

#[derive(Default)]
struct ClientTelemetryState {
    unrecoverable_frames: u32,
    recovered_frames: u32,
    recovered_fragments: u32,
    presented_frames: u32,
    render_dropped_frames: u32,
    total_decode_queue_us: u64,
    total_render_queue_us: u64,
}

impl ClientTelemetryAccumulator {
    pub fn shared() -> SharedClientTelemetry {
        Arc::new(Self::default())
    }

    pub fn record_reassembly(
        &self,
        unrecoverable_frames: u32,
        recovered_fragments: u32,
        recovered_frame: bool,
    ) {
        let mut inner = self.inner.lock().expect("client telemetry mutex poisoned");
        inner.unrecoverable_frames = inner
            .unrecoverable_frames
            .saturating_add(unrecoverable_frames);
        inner.recovered_fragments = inner
            .recovered_fragments
            .saturating_add(recovered_fragments);
        if recovered_frame {
            inner.recovered_frames = inner.recovered_frames.saturating_add(1);
        }
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn record_render(&self, decode_queue_us: u32, render_queue_us: u32, dropped_frames: u32) {
        let mut inner = self.inner.lock().expect("client telemetry mutex poisoned");
        inner.presented_frames = inner.presented_frames.saturating_add(1);
        inner.render_dropped_frames = inner.render_dropped_frames.saturating_add(dropped_frames);
        inner.total_decode_queue_us += decode_queue_us as u64;
        inner.total_render_queue_us += render_queue_us as u64;
    }

    pub fn drain(&self) -> ClientTelemetry {
        let mut inner = self.inner.lock().expect("client telemetry mutex poisoned");
        let presented = inner.presented_frames;
        let telemetry = ClientTelemetry {
            unrecoverable_frames: inner.unrecoverable_frames,
            recovered_frames: inner.recovered_frames,
            recovered_fragments: inner.recovered_fragments,
            presented_frames: presented,
            render_dropped_frames: inner.render_dropped_frames,
            avg_decode_queue_us: average(inner.total_decode_queue_us, presented),
            avg_render_queue_us: average(inner.total_render_queue_us, presented),
        };
        *inner = ClientTelemetryState::default();
        telemetry
    }
}

fn average(total: u64, count: u32) -> u32 {
    if count > 0 {
        (total / count as u64) as u32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::ClientTelemetryAccumulator;

    #[test]
    fn drains_aggregated_client_telemetry() {
        let telemetry = ClientTelemetryAccumulator::default();
        telemetry.record_reassembly(2, 3, true);
        telemetry.record_render(120, 80, 1);
        telemetry.record_render(60, 40, 0);

        let sample = telemetry.drain();
        assert_eq!(sample.unrecoverable_frames, 2);
        assert_eq!(sample.recovered_frames, 1);
        assert_eq!(sample.recovered_fragments, 3);
        assert_eq!(sample.presented_frames, 2);
        assert_eq!(sample.render_dropped_frames, 1);
        assert_eq!(sample.avg_decode_queue_us, 90);
        assert_eq!(sample.avg_render_queue_us, 60);

        let next = telemetry.drain();
        assert_eq!(next.presented_frames, 0);
        assert_eq!(next.avg_decode_queue_us, 0);
    }
}
