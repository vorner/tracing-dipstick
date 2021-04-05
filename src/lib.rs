use std::fmt::Debug;

use dipstick::{InputScope, Prefixed};
use tracing_core::{Event, Subscriber};
use tracing_core::field::{Field, Visit};
use tracing_core::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

const SCOPE_NAME: &str = "metric_scope";
const SCOPE_NAME_FULL: &str = "metric_scope_full";

const VALUE: &str = "metric_value";
const COUNTER: &str = "metric_counter";
const GAUGE: &str = "metric_gauge";

#[derive(Clone)]
struct Scope<S>(S);

#[derive(Copy, Clone, Debug, Default)]
pub struct DipstickLayer<S> {
    scope: S,
}

impl<S> DipstickLayer<S>
where
    S: Clone + InputScope + Prefixed + 'static,
{
    pub fn new(input_scope: S) -> Self {
        DipstickLayer {
            scope: input_scope,
        }
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
        // TODO: Is it the newly created, or the parent?
        let scope = ctx
            .lookup_current()
            .and_then(|current| {
                current
                    .extensions()
                    .get::<Scope<S>>()
                    .map(|Scope(s)| named(s))
            })
            .unwrap_or_else(|| named(&self.scope));
        ctx.span(id)
            .expect("Missing newly created span")
            .extensions_mut()
            .insert(Scope(scope));
    }
    // TODO: How about cloning/creating new IDs for spans?
    fn on_event(&self, event: &Event, ctx: Context<I>) {
        // TODO: Lazify
        let scope = ctx
            .lookup_current()
            .map(|current| {
                // FIXME: The clone!
                current.extensions().get::<Scope<S>>().cloned().expect("Missing prepared scope").0
            })
            .unwrap_or_else(|| self.scope.clone());

        let mut value = 1i64;
        struct ValueVisitor<'a>(&'a mut i64);
        impl Visit for ValueVisitor<'_> {
            fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
            fn record_i64(&mut self, field: &Field, value: i64) {
                if field.name() == VALUE {
                    *self.0 = value;
                }
            }
            fn record_u64(&mut self, field: &Field, value: u64) {
                if field.name() == VALUE {
                    // TODO: Is this OK?
                    *self.0 = value as _;
                }
            }
        }
        event.record(&mut ValueVisitor(&mut value));

        struct MetricVisitor<'a, S> {
            scope: &'a S,
            value: i64,
        }
        impl<S: InputScope> Visit for MetricVisitor<'_, S> {
            fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
            fn record_str(&mut self, field: &Field, value: &str) {
                let name = field.name();
                if name == COUNTER {
                    self.scope.counter(value).count(self.value as _);
                } else if name == GAUGE {
                    self.scope.gauge(value).value(self.value);
                }
            }
        }
        event.record(&mut MetricVisitor {
            scope: &scope,
            value,
        });
    }
    // TODO: on_span exit/enter/...
}
