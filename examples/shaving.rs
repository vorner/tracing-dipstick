use std::thread;
use std::time::Duration;

use dipstick::{AtomicBucket, ScheduleFlush, Stream};
use tracing::{debug, info_span, subscriber};
use tracing_dipstick::DipstickLayer;
use tracing_subscriber::fmt::Layer as FmtLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

fn main() {
    let root = AtomicBucket::new();
    root.stats(dipstick::stats_all);
    root.drain(Stream::write_to_stdout());
    let _flush = root.flush_every(Duration::from_secs(5));

    let bridge = DipstickLayer::new(root);

    let subscriber = Registry::default()
        // FIXME: This, unfortunately, filters the metrics too, no matter if the metrics are inside
        // or outside of it :-(
        //.with(EnvFilter::from_default_env())
        .with(FmtLayer::new().pretty())
        .with(bridge);

    subscriber::set_global_default(subscriber).unwrap();

    const CNT: usize = 100;
    let _yaks = info_span!("Shaving yaks", cnt = CNT).entered();
    for i in 0..100 {
        let this_yak = info_span!("Yak", no = i, metric_scope = "yak");
        this_yak.in_scope(|| {
            debug!("Starting shaving");
        });
        debug!("Waiting for yak to calm down");
        thread::sleep(Duration::from_millis(600));
        this_yak.in_scope(|| {
            debug!(metric_counter = "done", "Shaving done");
        });
    }
}
