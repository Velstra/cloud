//! Numbers, in the one format everything already scrapes.
//!
//! Hand-rolled rather than a client library, because what a control plane needs
//! to expose is a few dozen series that are computed from state it already
//! holds, and a dependency that owns a global registry is a dependency that
//! decides how tests run.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{Arc, Mutex},
};

/// A metric name with its labels. Ordered labels, so one series is one key no
/// matter what order the caller passed them in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Series {
    name: String,
    labels: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Shape {
    Counter,
    Gauge,
}

#[derive(Default)]
struct Inner {
    values: BTreeMap<Series, (Shape, f64)>,
}

/// Every number this process publishes.
#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Mutex<Inner>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self, name: &str, labels: &[(&str, &str)]) {
        self.add(name, labels, 1.0);
    }

    pub fn add(&self, name: &str, labels: &[(&str, &str)], by: f64) {
        let mut inner = self.lock();
        let entry = inner
            .values
            .entry(series(name, labels))
            .or_insert((Shape::Counter, 0.0));
        entry.1 += by;
    }

    pub fn set(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.lock()
            .values
            .insert(series(name, labels), (Shape::Gauge, value));
    }

    pub fn get(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.lock()
            .values
            .get(&series(name, labels))
            .map(|(_, v)| *v)
    }

    /// Drop every series of `name` that carries all of `matching`, so a gauge
    /// computed by scanning does not keep reporting a label combination that no
    /// longer exists.
    ///
    /// A drift metric that never forgets is worse than none: the reason an
    /// object diverged three hours ago stays on the dashboard, at a count that
    /// stopped being true when the object converged. The label filter is what
    /// lets one kind's scan forget its own series without erasing another
    /// kind's, which is scanned on its own schedule.
    pub fn clear(&self, name: &str, matching: &[(&str, &str)]) {
        self.lock().values.retain(|series, _| {
            series.name != name
                || !matching
                    .iter()
                    .all(|(k, v)| series.labels.get(*k).map(String::as_str) == Some(*v))
        });
    }

    /// The Prometheus text exposition of everything, sorted so a diff between
    /// two scrapes is readable.
    pub fn render(&self) -> String {
        let inner = self.lock();
        let mut out = String::new();
        let mut typed: Option<&str> = None;
        for (series, (shape, value)) in &inner.values {
            if typed != Some(series.name.as_str()) {
                let kind = match shape {
                    Shape::Counter => "counter",
                    Shape::Gauge => "gauge",
                };
                let _ = writeln!(out, "# TYPE {} {kind}", series.name);
                typed = Some(series.name.as_str());
            }
            let _ = writeln!(
                out,
                "{}{} {value}",
                series.name,
                render_labels(&series.labels)
            );
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .expect("the metrics lock is never held across an await")
    }
}

fn series(name: &str, labels: &[(&str, &str)]) -> Series {
    Series {
        name: name.to_string(),
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn render_labels(labels: &BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let pairs: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_accumulates_per_label_set() {
        let m = Metrics::new();
        m.count("reconcile_total", &[("controller", "scheduler")]);
        m.count("reconcile_total", &[("controller", "scheduler")]);
        m.count("reconcile_total", &[("controller", "quota")]);
        assert_eq!(
            m.get("reconcile_total", &[("controller", "scheduler")]),
            Some(2.0)
        );
        assert_eq!(
            m.get("reconcile_total", &[("controller", "quota")]),
            Some(1.0)
        );
    }

    #[test]
    fn label_order_does_not_split_a_series() {
        let m = Metrics::new();
        m.count("drift", &[("type", "instances"), ("reason", "Unconverged")]);
        m.count("drift", &[("reason", "Unconverged"), ("type", "instances")]);
        assert_eq!(
            m.get("drift", &[("type", "instances"), ("reason", "Unconverged")]),
            Some(2.0),
            "the same series was counted twice under two orderings"
        );
    }

    #[test]
    fn a_scanned_gauge_forgets_what_stopped_being_true() {
        let m = Metrics::new();
        m.set(
            "drift",
            &[("type", "instances"), ("reason", "Unconverged")],
            3.0,
        );
        m.set(
            "drift",
            &[("type", "volumes"), ("reason", "Unconverged")],
            1.0,
        );
        m.clear("drift", &[("type", "instances")]);
        assert_eq!(
            m.get("drift", &[("type", "instances"), ("reason", "Unconverged")]),
            None
        );
        assert_eq!(
            m.get("drift", &[("type", "volumes"), ("reason", "Unconverged")]),
            Some(1.0),
            "one kind's scan erased another kind's series"
        );
    }

    #[test]
    fn the_rendering_is_scrapeable() {
        let m = Metrics::new();
        m.set(
            "objects_with_spec_status_mismatch",
            &[("type", "instances")],
            2.0,
        );
        let text = m.render();
        assert!(text.contains("# TYPE objects_with_spec_status_mismatch gauge"));
        assert!(text.contains("objects_with_spec_status_mismatch{type=\"instances\"} 2"));
    }
}
