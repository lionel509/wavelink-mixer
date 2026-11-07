//! `wavelink-mixer latency`: measures the delay the mixer adds to game audio, by playing a click
//! straight into the hardware sink and again through the game channel, recording the hardware
//! sink's monitor each time. Pure helpers live here; the subcommand (`run`) touches PipeWire.
use serde_json::Value;

pub const RATE: usize = 48000;

/// Interleaved stereo click: `pre_s` seconds of silence, 1 ms at 0.9 on both channels, `post_s` of silence.
pub fn click_signal(rate: usize, pre_s: f64, post_s: f64) -> Vec<f32> {
    todo!()
}

/// Index (in frames, not samples) of the first frame where |L| or |R| reaches `thresh`.
pub fn click_index(pcm: &[f32], thresh: f32) -> Option<usize> {
    todo!()
}

/// Device output latency in frames as PipeWire reports it: the `Latency` param of the node's
/// input port (`minQuantum * quantum + minRate`). `None` if the node or port is missing.
pub fn device_latency_frames(dump: &Value, node_name: &str, quantum: u64) -> Option<u64> {
    todo!()
}

pub fn median(xs: &[f64]) -> f64 {
    todo!()
}

/// Human summary: medians, the added delay (via − direct) and the total game → ears estimate.
pub fn summary(direct_ms: &[f64], via_ms: &[f64], device_ms: f64) -> String {
    todo!()
}

/// `latency [--runs N] [--channel game]`. Returns the process exit code.
pub fn run(_args: Vec<String>) -> i32 {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn click_signal_layout() {
        let s = click_signal(48000, 0.2, 0.3);
        assert_eq!(s.len(), 24048 * 2);
        assert!(s[..9600 * 2].iter().all(|&x| x == 0.0));
        for f in 9600..9648 {
            assert_eq!(s[f * 2], 0.9);
            assert_eq!(s[f * 2 + 1], 0.9);
        }
        assert!(s[9648 * 2..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn click_index_finds_first_frame() {
        let s = click_signal(48000, 0.2, 0.3);
        assert_eq!(click_index(&s, 0.3), Some(9600));
        assert_eq!(click_index(&vec![0.0f32; 1000], 0.3), None);
        assert_eq!(click_index(&s, 0.95), None);
    }

    #[test]
    fn device_latency_from_dump() {
        let dump = json!([
            {"id": 7, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": "hw"}}},
            {"id": 8, "type": "PipeWire:Interface:Port", "info": {
                "props": {"node.id": 7, "port.direction": "in"},
                "params": {"Latency": [
                    {"direction": "Input", "minQuantum": 1.0, "minRate": 512},
                    {"direction": "Output", "minQuantum": 0.0, "minRate": 0}]}}},
            {"id": 9, "type": "PipeWire:Interface:Port", "info": {
                "props": {"node.id": 7, "port.direction": "out"},
                "params": {"Latency": [
                    {"direction": "Input", "minQuantum": 0.0, "minRate": 0},
                    {"direction": "Output", "minQuantum": 0.0, "minRate": 0}]}}}
        ]);
        assert_eq!(device_latency_frames(&dump, "hw", 1024), Some(1536));
        assert_eq!(device_latency_frames(&dump, "nope", 1024), None);
    }

    #[test]
    fn median_odd_even() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn summary_reports_difference() {
        let s = summary(&[10.0, 11.0, 12.0], &[40.0, 41.0, 42.0], 8.0);
        assert!(s.contains("added by mixer: 30.0 ms"), "{s}");
        assert!(s.contains("total game → ears ≈ 38.0 ms"), "{s}");
    }
}
