use anyhow::{Context as _, Result};
use gpui::{FrameDurationSnapshot, InputLatencySnapshot, Window};
use hdrhistogram::Histogram;

const FRAME_BUDGET_120_HZ_NS: u64 = 8_333_333;
const FRAME_BUDGET_60_HZ_NS: u64 = 16_666_667;

#[derive(Default)]
pub(crate) struct PerformanceReporter {
    previous: Option<PerformanceSnapshot>,
    previous_label: Option<&'static str>,
}

impl PerformanceReporter {
    pub(crate) fn mark_baseline(&mut self, window: &mut Window) {
        window.begin_performance_measurement();
        self.previous = Some(PerformanceSnapshot::capture(window));
        self.previous_label = Some(":perf-j baseline");
    }

    pub(crate) fn snapshot_report(&mut self, window: &Window) -> Result<String> {
        self.snapshot_report_as(window, ":perf")
    }

    pub(crate) fn snapshot_benchmark_report(&mut self, window: &Window) -> Result<String> {
        self.snapshot_report_as(window, ":perf-j")
    }

    fn snapshot_report_as(
        &mut self,
        window: &Window,
        current_label: &'static str,
    ) -> Result<String> {
        let current = PerformanceSnapshot::capture(window);
        let result = PerformanceSample::between(&current, self.previous.as_ref())
            .map(|sample| sample.format(self.previous_label));
        self.previous = Some(current);
        self.previous_label = Some(current_label);
        result
    }
}

struct PerformanceSnapshot {
    frames: FrameDurationSnapshot,
    input: InputLatencySnapshot,
}

impl PerformanceSnapshot {
    fn capture(window: &Window) -> Self {
        Self {
            frames: window.frame_duration_snapshot(),
            input: window.input_latency_snapshot(),
        }
    }
}

struct PerformanceSample {
    draw_duration: Histogram<u64>,
    prepaint_duration: Histogram<u64>,
    paint_duration: Histogram<u64>,
    list_prepaint_duration: Histogram<u64>,
    input_only_editor_prepaint_duration: Histogram<u64>,
    input_to_present: Histogram<u64>,
    input_arrival_interval: Histogram<u64>,
    input_dispatch_duration: Histogram<u64>,
    input_present_interval: Histogram<u64>,
    animation_interval: Histogram<u64>,
    events_per_frame: Histogram<u64>,
    mid_draw_events_dropped: u64,
}

impl PerformanceSample {
    fn between(
        current: &PerformanceSnapshot,
        previous: Option<&PerformanceSnapshot>,
    ) -> Result<Self> {
        let previous_frames = previous.map(|snapshot| &snapshot.frames);
        let previous_input = previous.map(|snapshot| &snapshot.input);
        let mid_draw_events_dropped = match previous_input {
            Some(previous) => current
                .input
                .mid_draw_events_dropped
                .checked_sub(previous.mid_draw_events_dropped)
                .context("subtract mid-draw event baseline")?,
            None => current.input.mid_draw_events_dropped,
        };

        Ok(Self {
            draw_duration: histogram_delta(
                &current.frames.draw_duration_histogram,
                previous_frames.map(|snapshot| &snapshot.draw_duration_histogram),
                "draw duration",
            )?,
            prepaint_duration: histogram_delta(
                &current.frames.prepaint_duration_histogram,
                previous_frames.map(|snapshot| &snapshot.prepaint_duration_histogram),
                "prepaint duration",
            )?,
            paint_duration: histogram_delta(
                &current.frames.paint_duration_histogram,
                previous_frames.map(|snapshot| &snapshot.paint_duration_histogram),
                "paint duration",
            )?,
            list_prepaint_duration: histogram_delta(
                &current.frames.list_prepaint_duration_histogram,
                previous_frames.map(|snapshot| &snapshot.list_prepaint_duration_histogram),
                "list prepaint duration",
            )?,
            input_only_editor_prepaint_duration: histogram_delta(
                &current.frames.input_only_editor_prepaint_duration_histogram,
                previous_frames
                    .map(|snapshot| &snapshot.input_only_editor_prepaint_duration_histogram),
                "input-only editor prepaint duration",
            )?,
            input_to_present: histogram_delta(
                &current.input.latency_histogram,
                previous_input.map(|snapshot| &snapshot.latency_histogram),
                "input-to-present latency",
            )?,
            input_arrival_interval: histogram_delta(
                &current.input.input_arrival_interval_histogram,
                previous_input.map(|snapshot| &snapshot.input_arrival_interval_histogram),
                "input arrival interval",
            )?,
            input_dispatch_duration: histogram_delta(
                &current.input.input_dispatch_duration_histogram,
                previous_input.map(|snapshot| &snapshot.input_dispatch_duration_histogram),
                "input dispatch duration",
            )?,
            input_present_interval: histogram_delta(
                &current.frames.input_driven_present_interval_histogram,
                previous_frames.map(|snapshot| &snapshot.input_driven_present_interval_histogram),
                "input-driven present interval",
            )?,
            animation_interval: histogram_delta(
                &current.frames.present_interval_histogram,
                previous_frames.map(|snapshot| &snapshot.present_interval_histogram),
                "animation interval",
            )?,
            events_per_frame: histogram_delta(
                &current.input.events_per_frame_histogram,
                previous_input.map(|snapshot| &snapshot.events_per_frame_histogram),
                "events per frame",
            )?,
            mid_draw_events_dropped,
        })
    }

    fn format(&self, previous_label: Option<&str>) -> String {
        let period = previous_label
            .map(|label| format!("since {label}"))
            .unwrap_or_else(|| "cumulative".into());
        [
            format!("Harness performance ({period})"),
            format_duration_histogram("draw", &self.draw_duration),
            format_duration_histogram("prepaint", &self.prepaint_duration),
            format_duration_histogram("paint", &self.paint_duration),
            format_duration_histogram("list prepaint", &self.list_prepaint_duration),
            format_duration_histogram(
                "input-only editor prepaint",
                &self.input_only_editor_prepaint_duration,
            ),
            format_duration_histogram("input arrival", &self.input_arrival_interval),
            format_duration_histogram("input dispatch", &self.input_dispatch_duration),
            format_duration_histogram("input→present", &self.input_to_present),
            format_duration_histogram("input present cadence", &self.input_present_interval),
            format_duration_histogram("animation", &self.animation_interval),
            format_count_histogram("events/frame", &self.events_per_frame),
            format!("mid-draw drops n={}", self.mid_draw_events_dropped),
        ]
        .join("\n")
    }
}

fn histogram_delta(
    current: &Histogram<u64>,
    previous: Option<&Histogram<u64>>,
    label: &str,
) -> Result<Histogram<u64>> {
    let mut delta = current.clone();
    if let Some(previous) = previous {
        delta
            .subtract(previous)
            .with_context(|| format!("subtract {label} histogram baseline"))?;
    }
    Ok(delta)
}

fn format_duration_histogram(label: &str, histogram: &Histogram<u64>) -> String {
    if histogram.is_empty() {
        return format!("{label} n=0 p50=— p95=— p99=— max=— >8.33ms=0 >16.67ms=0");
    }

    format!(
        "{label} n={} p50={} p95={} p99={} max={} >8.33ms={} >16.67ms={}",
        histogram.len(),
        format_milliseconds(histogram.value_at_quantile(0.50)),
        format_milliseconds(histogram.value_at_quantile(0.95)),
        format_milliseconds(histogram.value_at_quantile(0.99)),
        format_milliseconds(histogram.max()),
        count_greater_than(histogram, FRAME_BUDGET_120_HZ_NS),
        count_greater_than(histogram, FRAME_BUDGET_60_HZ_NS),
    )
}

fn format_count_histogram(label: &str, histogram: &Histogram<u64>) -> String {
    if histogram.is_empty() {
        return format!("{label} n=0 p50=— p95=— p99=— max=—");
    }

    format!(
        "{label} n={} p50={} p95={} p99={} max={}",
        histogram.len(),
        histogram.value_at_quantile(0.50),
        histogram.value_at_quantile(0.95),
        histogram.value_at_quantile(0.99),
        histogram.max(),
    )
}

fn format_milliseconds(nanoseconds: u64) -> String {
    format!("{:.2}ms", nanoseconds as f64 / 1_000_000.)
}

fn count_greater_than(histogram: &Histogram<u64>, threshold: u64) -> u64 {
    histogram
        .iter_recorded()
        .filter(|value| value.value_iterated_to() > threshold)
        .map(|value| value.count_at_value())
        .fold(0, u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_delta_contains_only_samples_after_the_baseline() {
        let previous = snapshot(
            &[1_000_000],
            &[2_000_000],
            &[3_000_000],
            &[500_000],
            &[8_000_000],
            &[9_000_000],
            &[1],
            2,
        );
        let current = snapshot(
            &[1_000_000, 20_000_000],
            &[2_000_000, 3_000_000],
            &[3_000_000, 4_000_000],
            &[500_000, 1_500_000],
            &[8_000_000, 17_000_000],
            &[9_000_000, 10_000_000],
            &[1, 3],
            5,
        );

        let sample = PerformanceSample::between(&current, Some(&previous))
            .expect("monotonic snapshots should subtract");
        assert_eq!(sample.draw_duration.len(), 1);
        assert_eq!(sample.draw_duration.count_at(20_000_000), 1);
        assert_eq!(sample.input_arrival_interval.len(), 1);
        assert_eq!(sample.input_arrival_interval.count_at(4_000_000), 1);
        assert_eq!(sample.input_dispatch_duration.len(), 1);
        assert_eq!(sample.input_dispatch_duration.count_at(1_500_000), 1);
        assert_eq!(sample.input_present_interval.len(), 1);
        assert_eq!(sample.input_present_interval.count_at(17_000_000), 1);
        assert_eq!(sample.events_per_frame.len(), 1);
        assert_eq!(sample.events_per_frame.count_at(3), 1);
        let report = sample.format(Some("previous :perf"));

        assert!(report.contains("since previous :perf"));
        assert!(report.contains("draw n=1"));
        assert!(report.contains("input arrival n=1"));
        assert!(report.contains("input dispatch n=1"));
        assert!(report.contains("input present cadence n=1"));
        assert!(report.contains(">16.67ms=1"));
        assert!(report.contains("events/frame n=1 p50=3"));
        assert!(report.contains("mid-draw drops n=3"));
    }

    #[test]
    fn duration_report_counts_frame_budget_misses() {
        let histogram = histogram(&[8_000_000, 9_000_000, 17_000_000]);

        let report = format_duration_histogram("cadence", &histogram);

        assert!(report.contains(">8.33ms=2"));
        assert!(report.contains(">16.67ms=1"));
    }

    fn snapshot(
        draws: &[u64],
        input_latency: &[u64],
        input_arrival_intervals: &[u64],
        input_dispatch_durations: &[u64],
        input_intervals: &[u64],
        animation_intervals: &[u64],
        events_per_frame: &[u64],
        mid_draw_events_dropped: u64,
    ) -> PerformanceSnapshot {
        PerformanceSnapshot {
            frames: FrameDurationSnapshot {
                draw_duration_histogram: histogram(draws),
                prepaint_duration_histogram: histogram(draws),
                paint_duration_histogram: histogram(draws),
                list_prepaint_duration_histogram: histogram(draws),
                input_only_editor_prepaint_duration_histogram: histogram(draws),
                input_driven_present_interval_histogram: histogram(input_intervals),
                present_interval_histogram: histogram(animation_intervals),
            },
            input: InputLatencySnapshot {
                latency_histogram: histogram(input_latency),
                input_arrival_interval_histogram: histogram(input_arrival_intervals),
                input_dispatch_duration_histogram: histogram(input_dispatch_durations),
                events_per_frame_histogram: histogram(events_per_frame),
                mid_draw_events_dropped,
            },
        }
    }

    fn histogram(values: &[u64]) -> Histogram<u64> {
        let mut histogram = Histogram::new(3).expect("histogram should initialize");
        for value in values {
            histogram
                .record(*value)
                .expect("test sample should fit the histogram");
        }
        histogram
    }
}
