//! Tracing subscriber that accumulates per-span wall time plus the maximum
//! `have_set_len` debug event field.  Results are snapshot-and-appended as
//! JSON Lines to a file at the end of each bench group.
//!
//! Installs a global subscriber the first time [`install`] is called; later
//! calls are no-ops.  Benches running on the tokio runtime thread (the axum
//! server is on a background thread) require a global subscriber — a
//! per-thread default would miss the instrumented spans.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

struct Timer(Instant);

struct Inner {
    totals: HashMap<&'static str, (u128, u64)>,
    max_have_set_len: u64,
}

static STATE: OnceLock<Arc<Mutex<Inner>>> = OnceLock::new();
static INSTALLED: OnceLock<()> = OnceLock::new();

fn state() -> &'static Arc<Mutex<Inner>> {
    STATE.get_or_init(|| {
        Arc::new(Mutex::new(Inner {
            totals: HashMap::new(),
            max_have_set_len: 0,
        }))
    })
}

/// Install the global subscriber.  Idempotent.
pub fn install() {
    INSTALLED.get_or_init(|| {
        let layer = SpanTotalsLayer {
            inner: state().clone(),
        };
        let subscriber = Registry::default().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Clear all accumulated totals.  Call between bench cases.
pub fn reset() {
    let mut s = state().lock().unwrap();
    s.totals.clear();
    s.max_have_set_len = 0;
}

#[derive(Serialize)]
pub struct SpanRecord {
    pub name: String,
    pub total_ns: u128,
    pub count: u64,
    pub mean_ns: u128,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub bench_id: String,
    pub spans: Vec<SpanRecord>,
    pub have_set_len_max: u64,
}

pub fn snapshot(bench_id: impl Into<String>) -> Snapshot {
    let s = state().lock().unwrap();
    let mut spans: Vec<SpanRecord> = s
        .totals
        .iter()
        .map(|(name, (total, count))| SpanRecord {
            name: (*name).to_string(),
            total_ns: *total,
            count: *count,
            mean_ns: if *count > 0 {
                *total / (*count as u128)
            } else {
                0
            },
        })
        .collect();
    spans.sort_by(|a, b| a.name.cmp(&b.name));
    Snapshot {
        bench_id: bench_id.into(),
        spans,
        have_set_len_max: s.max_have_set_len,
    }
}

/// Append one JSON Lines record for the snapshot and also print a
/// human-readable summary to stderr.
pub fn append_snapshot(path: &Path, snap: &Snapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(snap).unwrap())?;

    eprintln!("[spans] {}", snap.bench_id);
    for r in &snap.spans {
        eprintln!(
            "[spans]   {:<32} total={:>10}ns  count={:>4}  mean={:>10}ns",
            r.name, r.total_ns, r.count, r.mean_ns
        );
    }
    if snap.have_set_len_max > 0 {
        eprintln!("[spans]   have_set_len_max={}", snap.have_set_len_max);
    }
    Ok(())
}

struct SpanTotalsLayer {
    inner: Arc<Mutex<Inner>>,
}

impl<S> Layer<S> for SpanTotalsLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Timer(Instant::now()));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(start) = span
            .extensions()
            .get::<Timer>()
            .map(|Timer(instant)| *instant)
        else {
            return;
        };
        let elapsed = start.elapsed().as_nanos();
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.totals.entry(span.name()).or_insert((0, 0));
        entry.0 += elapsed;
        entry.1 += 1;
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        struct V {
            have_set_len: Option<u64>,
        }
        impl tracing::field::Visit for V {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                if field.name() == "have_set_len" {
                    self.have_set_len = Some(value);
                }
            }
            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                if field.name() == "have_set_len" && value >= 0 {
                    self.have_set_len = Some(value as u64);
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        let mut v = V { have_set_len: None };
        event.record(&mut v);
        if let Some(val) = v.have_set_len {
            let mut inner = self.inner.lock().unwrap();
            if val > inner.max_have_set_len {
                inner.max_have_set_len = val;
            }
        }
    }
}
