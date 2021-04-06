use std::fmt::Debug;

use dipstick::{InputScope, Level, Prefixed, TimeHandle, Timer};
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

impl<S: InputScope> MetricPoint for &S {
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

#[derive(Copy, Clone, Debug, Default)]
pub struct DipstickLayer<S> {
    scope: S,
}

impl<S> DipstickLayer<S>
where
    S: Clone + InputScope + Prefixed + 'static,
{
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
        // TODO: Lazify
        let scope = ctx
            .lookup_current()
            .map(|current| {
                // FIXME: The clone!
                current
                    .extensions()
                    .get::<Scope<S>>()
                    .map(|s| s.scope.clone())
                    .expect("Missing prepared scope")
            })
            .unwrap_or_else(|| self.scope.clone());

        event.record(&mut PointWrap(&scope));
    }
}
