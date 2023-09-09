use crate::{
    models::{QuickPulseEnvelope, QuickPulseMetric},
    uploader_quick_pulse::{self, PostOrPing},
    Exporter,
};
use futures_util::{
    future::{self, Either},
    pin_mut, StreamExt as _,
};
use opentelemetry::{
    runtime::{RuntimeChannel, TrySend},
    sdk::trace::{IdGenerator as _, RandomIdGenerator},
};
use opentelemetry_http::HttpClient;
use std::{time::Duration, time::SystemTime};
use sysinfo::{CpuExt as _, System, SystemExt as _};

const MAX_POST_WAIT_TIME: Duration = Duration::from_secs(20);
const MAX_PING_WAIT_TIME: Duration = Duration::from_secs(60);
const FALLBACK_INTERVAL: Duration = Duration::from_secs(60);
const PING_INTERVAL: Duration = Duration::from_secs(5);
const POST_INTERVAL: Duration = Duration::from_secs(1);

/// Live metrics
#[derive(Debug)]
pub struct QuickPulseManager<R: RuntimeChannel<()>> {
    message_sender: R::Sender,
}

impl<R: RuntimeChannel<()>> QuickPulseManager<R> {
    /// Start live metrics
    pub fn new<C: HttpClient + 'static>(exporter: Exporter<C>, runtime: R) -> QuickPulseManager<R> {
        let (message_sender, message_receiver) = runtime.batch_message_channel(1);
        let delay_runtime = runtime.clone();
        runtime.spawn(Box::pin(async move {
            let mut is_collecting = false;
            let mut last_success_time = SystemTime::UNIX_EPOCH;
            let mut redirected_host: Option<http::Uri> = None;
            let mut polling_interval_hint: Option<Duration> = None;
            let stream_id = format!("{:032x}", RandomIdGenerator::default().new_trace_id());
            let mut sys = System::new();
            let mut cpu_metric = QuickPulseMetric {
                name: "\\Processor(_Total)\\% Processor Time".into(),
                value: 0.0,
                weight: 0,
            };
            let mut current_timeout = PING_INTERVAL;

            let stop = Box::pin(message_receiver).into_future();
            pin_mut!(stop);
            loop {
                if let Either::Left(_) = future::select(&mut stop, delay_runtime.delay(current_timeout)).await {
                    break;
                }

                println!("[QPS] Tick");

                // TODO: collect metrics
                sys.refresh_cpu();
                let mut cpu_usage = 0.;
                for cpu in sys.cpus() {
                    cpu_usage += cpu.cpu_usage();
                }
                add_metric(&mut cpu_metric, cpu_usage);

                let now = SystemTime::now();

                println!("[QPS] Action is_collecting={}", is_collecting);

                let now_ms = now
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let envelope = QuickPulseEnvelope {
                    documents: Vec::new(),
                    metrics: vec![cpu_metric.clone()],
                    invariant_version: 1,
                    timestamp: format!("/Date({})/", now_ms),
                    version: None,
                    stream_id: stream_id.clone(),
                    machine_name: "Unknown".into(),
                    instance: "Unknown".into(),
                    role_name: None,
                };

                reset_metric(&mut cpu_metric);

                let res = uploader_quick_pulse::send(
                    exporter.client.as_ref(),
                    redirected_host
                        .as_ref()
                        .unwrap_or(&exporter.live_metrics_endpoint),
                    &exporter.instrumentation_key,
                    if is_collecting {
                        PostOrPing::Post
                    } else {
                        PostOrPing::Ping
                    },
                    envelope,
                )
                .await
                .map_err(|_| ());
                let last_send_succeeded = if let Ok(res) = res {
                    println!(
                        "[QPS] Success should_post={} redirected_host={:?} polling_interval_hint={:?}",
                        res.should_post, res.redirected_host, res.polling_interval_hint
                    );
                    last_success_time = now;
                    is_collecting = res.should_post;
                    if res.redirected_host.is_some() {
                        redirected_host = res.redirected_host;
                    }
                    if res.polling_interval_hint.is_some() {
                        polling_interval_hint = res.polling_interval_hint;
                    }
                    true
                } else {
                    println!("[QPS] Failure");
                    false
                };

                current_timeout = if is_collecting {
                    POST_INTERVAL
                } else {
                    polling_interval_hint.unwrap_or(PING_INTERVAL)
                };
                if !last_send_succeeded {
                    let time_since_last_success = now
                        .duration_since(last_success_time)
                        .unwrap_or(Duration::MAX);
                    if is_collecting && time_since_last_success >= MAX_POST_WAIT_TIME {
                        // Haven't posted successfully in 20 seconds, so wait 60 seconds and ping
                        is_collecting = false;
                        current_timeout = FALLBACK_INTERVAL;
                    } else if !is_collecting && time_since_last_success >= MAX_PING_WAIT_TIME {
                        // Haven't pinged successfully in 60 seconds, so wait another 60 seconds
                        current_timeout = FALLBACK_INTERVAL;
                    }
                }

                println!("[QPS] Next in {:?}", current_timeout);
            }
        }));

        QuickPulseManager { message_sender }
    }
}

impl<R: RuntimeChannel<()>> Drop for QuickPulseManager<R> {
    fn drop(&mut self) {
        if let Err(err) = self.message_sender.try_send(()) {
            opentelemetry::global::handle_error(opentelemetry::metrics::MetricsError::Other(
                err.to_string(),
            ));
        }
    }
}

fn add_metric(metric: &mut QuickPulseMetric, value: f32) {
    if metric.weight == 0 {
        metric.value = value;
        metric.weight = 1;
    } else {
        metric.value = (metric.value * (metric.weight as f32) + value) / (metric.weight + 1) as f32;
        metric.weight += 1;
    }
}

fn reset_metric(metric: &mut QuickPulseMetric) {
    metric.value = 0.0;
    metric.weight = 0;
}
