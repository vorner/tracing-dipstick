//! A bridge from [`tracing`]
//!
//! Traditionally, applications used separate instrumentation for metrics and logging. That is
//! tiresome to set up. Using [`tracing`] offers an opportunity to use single instrumentation and
//! export as both.
//!
//! This crate exports metrics through the [`dipstick`] metrics library, provided the
//! instrumentation uses specific attributes to events and spans. To use it:
//!
//! * Register the [`DipstickLayer`] to consume the spans and events.
//! * Use the `metrics.scope` on spans to create hierarchy of the metrics.
//! * Mark the spans and events with further `metrics.*` attributes to collect metrics of specific
//!   types and names.
//!
//! # Recognized attributes
//!
//! Whenever there's a span or event with one of these attributes, a metric is collected whenever
//! it is encountered. The value of the attribute is the name of the metric and the type (the thing
//! after the `metrics.`) corresponds to the metric types in [`dipstick`]'s [`InputScope`]. Spans
//! happen when they are created and some effects happen when they are closed/destroyed.
//!
//! * `metrics.counter="name"`: Adds 1 to the metric counter called `name`.
//! * `metrics.level="name"`: Adds 1 to the level called `name`. If it is present on a span, the 1
//!   is subtracted when it is closed (it's more useful on spans).
//! * `metrics.gauge="name"`: Sets the gauge to 1. This one is more useful in the second form
//!   below.
//! * `metrics.time="name"`: Records the time between the creation of the span and its destruction.
//!   This attribute is accepted only on spans.
//! * `metrics.scope="scope-name"`: Names of metrics that are inside this span get prefixed by this
//!   name, eg. their names will be `scope-name.name`. Nested spans with this attributes accumulate
//!   the name, eg `outer-scope-name.inner-scope-name.name`. This is accepted on spans only.
//! * `metrics.scope.full="scope-name"`: Similar to the above, but the name is not nested, it is
//!   replaced.
//!
//! The `counter`, `level` and `gauge` accept alternative variant of `metrics.type.name=value` (for
//! example, `metrics.gauge.name=42`), which uses the given value instead of `1`.
//!
//! Unfortunately, typos don't cause compile errors, they are just ignored :-(.
//!
//! # Naming
//!
//! While the metrics are sent into the [`dipstick`] library, the attribute naming is quite
//! general. This is on purpose. The author envisions that other crates might offer similar
//! functionality, but export the metrics to a different library. In such case it is beneficial if
//! the attributes are the same ‒ in such case changing the "backend" means only different
//! initialization while the instrumentation of the whole code stays the same.
//!
//! # Crate status
//!
//! * There are some limitations about filtering (see the note at [`DipstickLayer`]). They may be
//!   fixed either in [`tracing_subscriber`] or by changes in here, but both needs some work.
//! * There are several performance inefficiencies that need to be eliminated.
//! * The crate has been tested only lightly and it's possible it might not act correctly in some
//!   corner cases.
//!
//! So, there's still some work to happen (and help in doing it is welcome). On the other hand, it
//! is unlikely to cause some _serious_ problems, only incorrect metric readings.
//!
//! # Examples
//!
//! ```
//! use std::thread;
//! use std::time::Duration;
//!
//! use dipstick::{AtomicBucket, ScheduleFlush, Stream};
//! use log::LevelFilter;
//! use tracing::{debug, info_span, subscriber};
//! use tracing_dipstick::DipstickLayer;
//! use tracing_subscriber::layer::SubscriberExt;
//! use tracing_subscriber::Registry;
//!
//! fn main() {
//!     /*
//!      * We use the log-always integration of tracing here and route that to the env logger, that has
//!      * INFO enabled by default and can override by RUST_LOG to something else.
//!      *
//!      * We could use tracing_subscriber::fmt, *but* the EnvFilter there unfortunately disables
//!      * events/spans for the whole stack, not for logging only. And we want all the metrics while we
//!      * want only certain level of events.
//!      */
//!     env_logger::builder()
//!         .filter_level(LevelFilter::Info)
//!         .parse_default_env()
//!         .init();
//!
//!     let root = AtomicBucket::new();
//!     root.stats(dipstick::stats_all);
//!     root.drain(Stream::write_to_stdout());
//!     let _flush = root.flush_every(Duration::from_secs(5));
//!
//!     let bridge = DipstickLayer::new(root);
//!     let subscriber = Registry::default().with(bridge);
//!
//!     subscriber::set_global_default(subscriber).unwrap();
//!
//!     const CNT: usize = 10;
//!     let _yaks = info_span!("Shaving yaks", cnt = CNT, metrics.scope = "shaving").entered();
//!     for i in 0..CNT {
//!         let _this_yak = info_span!(
//!             "Yak",
//!             metrics.gauge.order = i,
//!             metrics.scope = "yak",
//!             metrics.time = "time",
//!             metrics.level = "active"
//!         )
//!         .entered();
//!         debug!(metrics.counter = "started", "Starting shaving");
//!         thread::sleep(Duration::from_millis(60));
//!         debug!(metrics.counter = "done", metrics.counter.legs = 4, "Shaving done");
//!     }
//! }
//!
//! ```
//!
//! [`tracing`]: https://docs.rs/tracing
#![doc(test(attr(deny(warnings))))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt::Debug;

use dipstick::{InputScope, Level, Prefixed, TimeHandle, Timer};
use once_cell::unsync::Lazy;
use tracing_core::field::{Field, Visit};
use tracing_core::span::{Attributes, Id};
use tracing_core::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

const SCOPE_NAME: &str = "metrics.scope";
const SCOPE_NAME_FULL: &str = "metrics.scope.full";

#[derive(Copy, Clone, Debug)]
enum MetricType {
    Counter,
    Gauge,
    Level,
    Timer,
}

impl MetricType {
    fn measure<P: MetricPoint>(self, point: &mut P, name: &str, value: i64) {
        let scope = point.scope();
        match self {
            MetricType::Counter => scope.counter(name).count(value as _),
            MetricType::Gauge => scope.gauge(name).value(value),
            MetricType::Level => {
                let level = scope.level(name);
                level.adjust(value);
                point.push_level(level, value);
            }
            MetricType::Timer => {
                let timer = scope.timer(name);
                let start = timer.start();
                point.push_timer(timer, start);
            }
        }
    }
}

const METRIC_TYPES: &[(&str, &str, MetricType, bool)] = &[
    ("metrics.counter", "metrics.counter.", MetricType::Counter, true),
    ("metrics.gauge", "metrics.gauge.", MetricType::Gauge, true),
    ("metrics.level", "metrics.level.", MetricType::Level, true),
    ("metrics.time", "", MetricType::Timer, false),
];

trait MetricPoint {
    const SCOPED: bool;
    type Scope: InputScope;
    fn push_timer(&mut self, timer: Timer, start: TimeHandle);
    fn push_level(&mut self, level: Level, decrement: i64);
    fn scope(&self) -> &Self::Scope;
}

struct PointWrap<P>(P);

impl<P: MetricPoint> Visit for PointWrap<P> {
    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
    fn record_str(&mut self, field: &Field, value: &str) {
        let name = field.name();
        for tp in METRIC_TYPES {
            if (tp.3 || P::SCOPED) && name == tp.0 {
                tp.2.measure(&mut self.0, value, 1);
                break;
            }
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        let name = field.name();
        for tp in METRIC_TYPES {
            if tp.3 && name.starts_with(tp.1) {
                tp.2.measure(&mut self.0, &name[tp.1.len()..], value);
            }
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_i64(field, value as _);
    }
}

#[derive(Clone)]
struct Scope<S> {
    scope: S,
    // TODO: Small vecs? Put into the same vec to save one allocation?
    timers: Vec<(Timer, TimeHandle)>,
    levels: Vec<(Level, i64)>,
    // TODO: CPU timers
}

impl<S> Drop for Scope<S> {
    fn drop(&mut self) {
        for (timer, start) in self.timers.drain(..) {
            timer.stop(start);
        }

        for (level, decrement) in self.levels.drain(..) {
            level.adjust(-decrement);
        }
    }
}

impl<S: InputScope> MetricPoint for Scope<S> {
    const SCOPED: bool = true;
    type Scope = S;
    fn push_level(&mut self, level: Level, decrement: i64) {
        self.levels.push((level, decrement));
    }
    fn push_timer(&mut self, timer: Timer, start: TimeHandle) {
        self.timers.push((timer, start));
    }
    fn scope(&self) -> &S {
        &self.scope
    }
}

impl<S, F> MetricPoint for Lazy<S, F>
where
    S: InputScope,
    F: FnOnce() -> S,
{
    const SCOPED: bool = false;
    type Scope = S;

    fn push_timer(&mut self, _: Timer, _: TimeHandle) {
        unreachable!("Timers are not supported on events");
    }

    fn push_level(&mut self, _: Level, _: i64) {
        // Levels on events are decremented manually, not at the end of some scope
    }

    fn scope(&self) -> &S {
        self
    }
}

/// The bridge from [`tracing`](https://docs.rs/tracing) to [`dipstick`].
///
/// This takes information from tracing and propagates them into [`dipstick`] as metrics. It works
/// as [`Layer`].
///
/// # Warning
///
/// Currently, [`tracing_subscriber`] doesn't allow filtering on per-layer basis. That means if
/// there's another layer that filters (for example based on the level), it'll impact this layer
/// too. This would negatively impact the gathered metrics as this expects to get them all.
///
/// It has been observed to work together with the `tracing`s `log-always` feature.
///
/// Future versions might bypass the [`Layer`] system and wrap a
/// [`Subscriber`][tracing_core::Subscriber] directly.
///
/// # Examples
///
/// ```rust
/// use std::time::Duration;
///
/// use dipstick::{AtomicBucket, ScheduleFlush, Stream};
/// use log::LevelFilter;
/// use tracing::subscriber;
/// use tracing_dipstick::DipstickLayer;
/// use tracing_subscriber::layer::SubscriberExt;
/// use tracing_subscriber::Registry;
///
/// env_logger::builder()
///     .filter_level(LevelFilter::Info)
///     .parse_default_env()
///     .init();
///
/// let root = AtomicBucket::new();
/// root.stats(dipstick::stats_all);
/// root.drain(Stream::write_to_stdout());
/// let _flush = root.flush_every(Duration::from_secs(5));
///
/// let bridge = DipstickLayer::new(root);
/// let subscriber = Registry::default().with(bridge);
///
/// subscriber::set_global_default(subscriber).unwrap();
/// ```
#[derive(Copy, Clone, Debug, Default)]
pub struct DipstickLayer<S> {
    scope: S,
}

impl<S> DipstickLayer<S>
where
    S: Clone + InputScope + Prefixed + 'static,
{
    /// Creates the bridge.
    ///
    /// Expects the scope into which it will put metrics.
    pub fn new(input_scope: S) -> Self {
        DipstickLayer { scope: input_scope }
    }
}

impl<S, I> Layer<I> for DipstickLayer<S>
where
    S: Clone + InputScope + Prefixed + Send + Sync + 'static,
    I: Subscriber,
    for<'l> I: LookupSpan<'l>,
{
    fn new_span(&self, attrs: &Attributes, id: &Id, ctx: Context<I>) {
        let named = |scope: &S| -> S {
            let mut named: Option<S> = None;
            struct NameVisitor<'a, S> {
                target: &'a mut Option<S>,
                src: &'a S,
            }
            impl<S> Visit for NameVisitor<'_, S>
            where
                S: Prefixed,
            {
                fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
                fn record_str(&mut self, field: &Field, value: &str) {
                    let name = field.name();
                    if name == SCOPE_NAME {
                        *self.target = Some(self.src.add_name(value));
                    } else if name == SCOPE_NAME_FULL {
                        *self.target = Some(self.src.named(value));
                    }
                }
            }
            attrs.record(&mut NameVisitor {
                target: &mut named,
                src: scope,
            });
            named.unwrap_or_else(|| scope.clone())
        };
        let scope = ctx
            .lookup_current()
            .and_then(|current| {
                current
                    .extensions()
                    .get::<Scope<S>>()
                    .map(|Scope { scope: s, .. }| named(s))
            })
            .unwrap_or_else(|| named(&self.scope));

        let mut scope = PointWrap(Scope {
            scope,
            timers: Vec::new(),
            levels: Vec::new(),
        });
        attrs.record(&mut scope);

        ctx.span(id)
            .expect("Missing newly created span")
            .extensions_mut()
            .insert(scope.0);
    }
    // TODO: How about cloning/creating new IDs for spans?
    fn on_event(&self, event: &Event, ctx: Context<I>) {
        // TODO: Currently, we store a scope in each span. Instead we should store it only in the
        // ones that are interesting. In particular:
        // * Score on creation only if the span itself touches metrics (either has some or has a
        //   metric scope).
        // * Initialize it lazily on the first access. But extensions_mut might be slower?
        let scope = Lazy::new(|| {
            ctx
                .lookup_current()
                .map(|c| {
                    // FIXME: It would be nice to avoid the clone. That should be possible, in
                    // theory.
                    c.extensions()
                        .get::<Scope<S>>()
                        .expect("Missing prepared scope")
                        .scope
                        .clone()
                })
                .unwrap_or_else(|| self.scope.clone())
        });

        event.record(&mut PointWrap(scope));
    }
}
