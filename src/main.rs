use core::str;
use std::collections::{HashMap, HashSet};
use std::error::Error as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use std::{io::Write, net::SocketAddr, pin::Pin, str::FromStr, sync::Arc};

use axum::{http::HeaderMap, response::IntoResponse, Router};
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use iprange::IpRange;
use opentelemetry::metrics::{Counter, Gauge, Histogram, MeterProvider as _, UpDownCounter};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, RwLock, Semaphore};
use tokio_stream::StreamExt;

#[cfg(feature = "avif-decoder")]
mod avif_seq;
mod browsersafe;
mod cache;
mod image_test;
mod img;
mod mng;
mod ssrf;
mod svg;

use cache::{CacheKey, CacheResult, ResponseCache};

const HOST_THROTTLE_FALLBACK_COOLDOWN: Duration = Duration::from_secs(5);
const HOST_THROTTLE_MAX_COOLDOWN: Duration = Duration::from_secs(60);
const HOST_THROTTLE_IDLE_TTL: Duration = Duration::from_secs(60 * 60);

fn default_host_throttle_queue_capacity() -> usize {
	64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HostThrottleConfig {
	requests_per_second: f64,
	burst: u32,
	max_wait_ms: u64,
	max_hosts: usize,
	#[serde(default = "default_host_throttle_queue_capacity")]
	queue_capacity: usize,
}

struct HostThrottleState {
	tokens: f64,
	rate_per_second: f64,
	last_refill: Instant,
	cooldown_until: Instant,
	consecutive_429: u32,
	last_seen: Instant,
	recovering: bool,
	gate: Arc<AsyncMutex<()>>,
	queue_slots: Arc<Semaphore>,
}

struct HostThrottlePermit {
	waited: Duration,
	throttled: bool,
	_gate: Option<OwnedMutexGuard<()>>,
}

#[derive(Debug)]
struct HostThrottleRejection {
	waited: Duration,
	retry_after: Duration,
}

struct HostThrottle {
	max_rate_per_second: f64,
	min_rate_per_second: f64,
	recovery_per_success: f64,
	burst: f64,
	max_wait: Duration,
	max_hosts: usize,
	queue_capacity: usize,
	states: AsyncMutex<HashMap<String, HostThrottleState>>,
}

impl HostThrottle {
	fn new(config: &HostThrottleConfig) -> Result<Self, String> {
		if !config.requests_per_second.is_finite() || config.requests_per_second <= 0.0 {
			return Err("host_throttle.requests_per_second must be greater than 0".to_owned());
		}
		if config.burst == 0 {
			return Err("host_throttle.burst must be greater than 0".to_owned());
		}
		if config.max_wait_ms == 0 {
			return Err("host_throttle.max_wait_ms must be greater than 0".to_owned());
		}
		if config.max_hosts == 0 {
			return Err("host_throttle.max_hosts must be greater than 0".to_owned());
		}
		if config.queue_capacity == 0 {
			return Err("host_throttle.queue_capacity must be greater than 0".to_owned());
		}
		Ok(Self::with_limits(
			config.requests_per_second,
			config.burst,
			Duration::from_millis(config.max_wait_ms),
			config.max_hosts,
			config.queue_capacity,
		))
	}

	fn with_limits(
		rate_per_second: f64,
		burst: u32,
		max_wait: Duration,
		max_hosts: usize,
		queue_capacity: usize,
	) -> Self {
		Self {
			max_rate_per_second: rate_per_second,
			min_rate_per_second: rate_per_second.min(0.25),
			recovery_per_success: rate_per_second / 120.0,
			burst: f64::from(burst),
			max_wait,
			max_hosts,
			queue_capacity,
			states: AsyncMutex::new(HashMap::new()),
		}
	}

	fn host_key(url: &reqwest::Url) -> Option<String> {
		url.host_str().map(str::to_ascii_lowercase)
	}

	fn state_for_host<'a>(
		&self,
		states: &'a mut HashMap<String, HostThrottleState>,
		host: &str,
		now: Instant,
	) -> &'a mut HostThrottleState {
		if !states.contains_key(host) && states.len() >= self.max_hosts {
			states.retain(|_, state| now.duration_since(state.last_seen) < HOST_THROTTLE_IDLE_TTL);
			if states.len() >= self.max_hosts {
				if let Some(oldest) = states
					.iter()
					.min_by_key(|(_, state)| state.last_seen)
					.map(|(host, _)| host.clone())
				{
					states.remove(&oldest);
				}
			}
		}
		states.entry(host.to_owned()).or_insert(HostThrottleState {
			tokens: self.burst,
			rate_per_second: self.max_rate_per_second,
			last_refill: now,
			cooldown_until: now,
			consecutive_429: 0,
			last_seen: now,
			recovering: false,
			gate: Arc::new(AsyncMutex::new(())),
			queue_slots: Arc::new(Semaphore::new(self.queue_capacity)),
		})
	}

	async fn acquire(
		&self,
		url: &reqwest::Url,
		wait_budget: Duration,
	) -> Result<HostThrottlePermit, HostThrottleRejection> {
		let Some(host) = Self::host_key(url) else {
			return Ok(HostThrottlePermit {
				waited: Duration::ZERO,
				throttled: false,
				_gate: None,
			});
		};

		let started = Instant::now();
		let (gate, queue_slots, retry_after) = {
			let mut states = self.states.lock().await;
			let now = Instant::now();
			let state = self.state_for_host(&mut states, &host, now);
			state.last_seen = now;
			if !state.recovering {
				return Ok(HostThrottlePermit {
					waited: Duration::ZERO,
					throttled: false,
					_gate: None,
				});
			}
			(
				state.gate.clone(),
				state.queue_slots.clone(),
				state.cooldown_until.saturating_duration_since(now),
			)
		};
		let max_wait = self.max_wait.min(wait_budget);
		let _queue_slot = queue_slots
			.try_acquire_owned()
			.map_err(|_| HostThrottleRejection {
				waited: Duration::ZERO,
				retry_after: retry_after.max(Duration::from_secs(1)),
			})?;
		let gate_guard = tokio::time::timeout(max_wait, gate.lock_owned())
			.await
			.map_err(|_| HostThrottleRejection {
				waited: started.elapsed(),
				retry_after: Duration::from_secs(1),
			})?;
		loop {
			let sleep_for = {
				let mut states = self.states.lock().await;
				let now = Instant::now();
				let state = self.state_for_host(&mut states, &host, now);
				if !state.recovering {
					return Ok(HostThrottlePermit {
						waited: started.elapsed(),
						throttled: true,
						_gate: None,
					});
				}
				let refill_seconds = now
					.saturating_duration_since(state.last_refill)
					.as_secs_f64();
				state.tokens =
					(state.tokens + refill_seconds * state.rate_per_second).min(self.burst);
				state.last_refill = now;
				state.last_seen = now;

				if now >= state.cooldown_until && state.tokens >= 1.0 {
					state.tokens -= 1.0;
					return Ok(HostThrottlePermit {
						waited: started.elapsed(),
						throttled: true,
						_gate: Some(gate_guard),
					});
				}

				let cooldown_wait = state.cooldown_until.saturating_duration_since(now);
				let token_wait = if state.tokens >= 1.0 {
					Duration::ZERO
				} else {
					Duration::from_secs_f64((1.0 - state.tokens) / state.rate_per_second)
				};
				cooldown_wait.max(token_wait)
			};

			let elapsed = started.elapsed();
			let remaining = max_wait.saturating_sub(elapsed);
			if sleep_for > remaining || remaining.is_zero() {
				return Err(HostThrottleRejection {
					waited: elapsed,
					retry_after: sleep_for.max(Duration::from_secs(1)),
				});
			}
			tokio::time::sleep(sleep_for).await;
		}
	}

	async fn observe_response(
		&self,
		url: &reqwest::Url,
		status: reqwest::StatusCode,
		headers: &reqwest::header::HeaderMap,
		was_throttled: bool,
	) {
		let Some(host) = Self::host_key(url) else {
			return;
		};

		let mut states = self.states.lock().await;
		let now = Instant::now();
		let state = self.state_for_host(&mut states, &host, now);
		state.last_seen = now;
		if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
			state.recovering = true;
			state.consecutive_429 = state.consecutive_429.saturating_add(1);
			state.rate_per_second = (state.rate_per_second * 0.5).max(self.min_rate_per_second);
			state.tokens = 1.0;
			let retry_after = parse_retry_after(headers).unwrap_or_else(|| {
				let shift = state.consecutive_429.saturating_sub(1).min(4);
				let exponential = (HOST_THROTTLE_FALLBACK_COOLDOWN * (1 << shift))
					.min(HOST_THROTTLE_MAX_COOLDOWN);
				let jitter_ms = SystemTime::now()
					.duration_since(SystemTime::UNIX_EPOCH)
					.unwrap_or_default()
					.subsec_millis() as u64;
				exponential + Duration::from_millis(jitter_ms)
			});
			state.cooldown_until = state.cooldown_until.max(now + retry_after);
			tracing::warn!(
				host,
				cooldown_ms = retry_after.as_millis() as u64,
				rate_per_second = state.rate_per_second,
				consecutive_429 = state.consecutive_429,
				"host throttle cooldown activated"
			);
		} else if status.is_success() && state.recovering && was_throttled {
			state.consecutive_429 = 0;
			state.rate_per_second =
				(state.rate_per_second + self.recovery_per_success).min(self.max_rate_per_second);
			if state.rate_per_second >= self.max_rate_per_second {
				state.recovering = false;
				state.tokens = self.burst;
			}
		}
	}

	#[cfg(test)]
	async fn rate_for(&self, url: &reqwest::Url) -> Option<f64> {
		let host = Self::host_key(url)?;
		self.states
			.lock()
			.await
			.get(&host)
			.map(|state| state.rate_per_second)
	}

	#[cfg(test)]
	async fn is_recovering(&self, url: &reqwest::Url) -> bool {
		let Some(host) = Self::host_key(url) else {
			return false;
		};
		self.states
			.lock()
			.await
			.get(&host)
			.is_some_and(|state| state.recovering)
	}
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
	let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
	if let Ok(seconds) = value.trim().parse::<u64>() {
		return (seconds > 0).then(|| Duration::from_secs(seconds));
	}
	let retry_at = httpdate::parse_http_date(value).ok()?;
	let duration = retry_at.duration_since(SystemTime::now()).ok()?;
	(!duration.is_zero()).then_some(duration)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RateLimitHeaderSnapshot {
	retry_after: Option<String>,
	retry_after_ms: Option<u64>,
	ratelimit_limit: Option<String>,
	ratelimit_remaining: Option<String>,
	ratelimit_reset: Option<String>,
	ratelimit_policy: Option<String>,
	x_ratelimit_limit: Option<String>,
	x_ratelimit_remaining: Option<String>,
	x_ratelimit_reset: Option<String>,
	server: Option<String>,
	cf_ray: Option<String>,
}

impl RateLimitHeaderSnapshot {
	fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
		fn value(headers: &reqwest::header::HeaderMap, name: &'static str) -> Option<String> {
			headers
				.get(name)
				.and_then(|value| value.to_str().ok())
				.map(|value| value.chars().take(256).collect())
		}

		Self {
			retry_after: value(headers, "retry-after"),
			retry_after_ms: parse_retry_after(headers)
				.map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64),
			ratelimit_limit: value(headers, "ratelimit-limit"),
			ratelimit_remaining: value(headers, "ratelimit-remaining"),
			ratelimit_reset: value(headers, "ratelimit-reset"),
			ratelimit_policy: value(headers, "ratelimit-policy"),
			x_ratelimit_limit: value(headers, "x-ratelimit-limit"),
			x_ratelimit_remaining: value(headers, "x-ratelimit-remaining"),
			x_ratelimit_reset: value(headers, "x-ratelimit-reset"),
			server: value(headers, "server"),
			cf_ray: value(headers, "cf-ray"),
		}
	}
}

fn log_upstream_429(url: &reqwest::Url, headers: &reqwest::header::HeaderMap) {
	let snapshot = RateLimitHeaderSnapshot::from_headers(headers);
	tracing::warn!(
		target_host = url.host_str().unwrap_or("unknown"),
		retry_after = snapshot.retry_after.as_deref().unwrap_or(""),
		retry_after_ms = snapshot.retry_after_ms.unwrap_or(0),
		retry_after_valid = snapshot.retry_after_ms.is_some(),
		ratelimit_limit = snapshot.ratelimit_limit.as_deref().unwrap_or(""),
		ratelimit_remaining = snapshot.ratelimit_remaining.as_deref().unwrap_or(""),
		ratelimit_reset = snapshot.ratelimit_reset.as_deref().unwrap_or(""),
		ratelimit_policy = snapshot.ratelimit_policy.as_deref().unwrap_or(""),
		x_ratelimit_limit = snapshot.x_ratelimit_limit.as_deref().unwrap_or(""),
		x_ratelimit_remaining = snapshot.x_ratelimit_remaining.as_deref().unwrap_or(""),
		x_ratelimit_reset = snapshot.x_ratelimit_reset.as_deref().unwrap_or(""),
		server = snapshot.server.as_deref().unwrap_or(""),
		cf_ray = snapshot.cf_ray.as_deref().unwrap_or(""),
		"upstream 429 response headers"
	);
}

fn throttle_wait_budget(config: &ConfigFile, request_started: Instant) -> Duration {
	let request_timeout = Duration::from_millis(config.timeout);
	let fetch_reserve = Duration::from_millis(config.connect_timeout_ms.min(config.timeout));
	request_timeout
		.saturating_sub(request_started.elapsed())
		.saturating_sub(fetch_reserve)
}

fn response_retry_after(wait: Duration) -> axum::http::HeaderValue {
	let seconds = wait
		.as_secs()
		.saturating_add(u64::from(wait.subsec_nanos() > 0))
		.max(1);
	axum::http::HeaderValue::from_str(&seconds.to_string())
		.unwrap_or_else(|_| axum::http::HeaderValue::from_static("1"))
}

struct OtlpMetrics {
	requests: Counter<u64>,
	errors: Counter<u64>,
	domain_requests: Counter<u64>,
	requests_active: UpDownCounter<i64>,
	upstream_responses: Counter<u64>,
	request_duration: Histogram<f64>,
	url_check_duration: Histogram<f64>,
	download_wait_duration: Histogram<f64>,
	upstream_ttfb_duration: Histogram<f64>,
	upstream_body_duration: Histogram<f64>,
	cpu_wait_duration: Histogram<f64>,
	decode_duration: Histogram<f64>,
	encode_duration: Histogram<f64>,
	cache_requests: Counter<u64>,
	cache_entries: Gauge<u64>,
	cache_bytes: Gauge<u64>,
	cache_capacity_bytes: Gauge<u64>,
	cache_capacity_evictions: Counter<u64>,
	cache_expired_evictions: Counter<u64>,
	singleflight_active: Gauge<u64>,
	static_requests: Counter<u64>,
	downloads_active: Gauge<u64>,
	downloads_limit: Gauge<u64>,
	cpu_active: Gauge<u64>,
	cpu_limit: Gauge<u64>,
	buffer_used_bytes: Gauge<u64>,
	buffer_limit_bytes: Gauge<u64>,
	buffer_wait_duration: Histogram<f64>,
	outputs: Counter<u64>,
	input_bytes: Counter<u64>,
	output_bytes: Counter<u64>,
	passthrough: Counter<u64>,
	processing_errors: Counter<u64>,
	fetch_errors: Counter<u64>,
	host_throttle_requests: Counter<u64>,
	host_throttle_wait_duration: Histogram<f64>,
	fetch_retry_attempts: Counter<u64>,
	fetch_retry_successes: Counter<u64>,
	dns_cache_requests: Counter<u64>,
	dns_cache_entries: Gauge<u64>,
	dns_cache_capacity_entries: Gauge<u64>,
	dns_retry_attempts: Counter<u64>,
	dns_retry_successes: Counter<u64>,
	stale_served: Counter<u64>,
	animations: Counter<u64>,
	animation_frames: Counter<u64>,
	animation_input_bytes: Counter<u64>,
	animation_output_bytes: Counter<u64>,
	uptime: Gauge<f64>,
}
impl OtlpMetrics {
	fn new(provider: &SdkMeterProvider) -> Self {
		let meter = provider.meter(env!("CARGO_PKG_NAME"));
		Self {
			requests: meter.u64_counter("media_proxy_requests_total").build(),
			errors: meter.u64_counter("media_proxy_errors_total").build(),
			domain_requests: meter
				.u64_counter("media_proxy_domain_requests_total")
				.build(),
			requests_active: meter
				.i64_up_down_counter("media_proxy_requests_active")
				.build(),
			upstream_responses: meter
				.u64_counter("media_proxy_upstream_responses_total")
				.build(),
			request_duration: duration_histogram(&meter, "media_proxy_request_duration"),
			url_check_duration: duration_histogram(&meter, "media_proxy_url_check_duration"),
			download_wait_duration: duration_histogram(
				&meter,
				"media_proxy_download_wait_duration",
			),
			upstream_ttfb_duration: duration_histogram(
				&meter,
				"media_proxy_upstream_ttfb_duration",
			),
			upstream_body_duration: duration_histogram(
				&meter,
				"media_proxy_upstream_body_duration",
			),
			cpu_wait_duration: duration_histogram(&meter, "media_proxy_cpu_wait_duration"),
			decode_duration: duration_histogram(&meter, "media_proxy_decode_duration"),
			encode_duration: duration_histogram(&meter, "media_proxy_encode_duration"),
			cache_requests: meter
				.u64_counter("media_proxy_cache_requests_total")
				.build(),
			cache_entries: meter.u64_gauge("media_proxy_cache_entries").build(),
			cache_bytes: meter.u64_gauge("media_proxy_cache_bytes").build(),
			cache_capacity_bytes: meter.u64_gauge("media_proxy_cache_capacity_bytes").build(),
			cache_capacity_evictions: meter
				.u64_counter("media_proxy_cache_capacity_evictions_total")
				.build(),
			cache_expired_evictions: meter
				.u64_counter("media_proxy_cache_expired_evictions_total")
				.build(),
			singleflight_active: meter.u64_gauge("media_proxy_singleflight_active").build(),
			static_requests: meter
				.u64_counter("media_proxy_static_requests_total")
				.build(),
			downloads_active: meter.u64_gauge("media_proxy_downloads_active").build(),
			downloads_limit: meter.u64_gauge("media_proxy_downloads_limit").build(),
			cpu_active: meter.u64_gauge("media_proxy_cpu_active").build(),
			cpu_limit: meter.u64_gauge("media_proxy_cpu_limit").build(),
			buffer_used_bytes: meter.u64_gauge("media_proxy_buffer_used_bytes").build(),
			buffer_limit_bytes: meter.u64_gauge("media_proxy_buffer_limit_bytes").build(),
			buffer_wait_duration: duration_histogram(&meter, "media_proxy_buffer_wait_duration"),
			outputs: meter.u64_counter("media_proxy_outputs_total").build(),
			input_bytes: meter.u64_counter("media_proxy_input_bytes_total").build(),
			output_bytes: meter.u64_counter("media_proxy_output_bytes_total").build(),
			passthrough: meter.u64_counter("media_proxy_passthrough_total").build(),
			processing_errors: meter
				.u64_counter("media_proxy_processing_errors_total")
				.build(),
			fetch_errors: meter.u64_counter("media_proxy_fetch_errors_total").build(),
			host_throttle_requests: meter
				.u64_counter("media_proxy_host_throttle_requests_total")
				.build(),
			host_throttle_wait_duration: duration_histogram(
				&meter,
				"media_proxy_host_throttle_wait_duration",
			),
			fetch_retry_attempts: meter
				.u64_counter("media_proxy_fetch_retry_attempts_total")
				.build(),
			fetch_retry_successes: meter
				.u64_counter("media_proxy_fetch_retry_successes_total")
				.build(),
			dns_cache_requests: meter
				.u64_counter("media_proxy_dns_cache_requests_total")
				.build(),
			dns_cache_entries: meter.u64_gauge("media_proxy_dns_cache_entries").build(),
			dns_cache_capacity_entries: meter
				.u64_gauge("media_proxy_dns_cache_capacity_entries")
				.build(),
			dns_retry_attempts: meter
				.u64_counter("media_proxy_dns_retry_attempts_total")
				.build(),
			dns_retry_successes: meter
				.u64_counter("media_proxy_dns_retry_successes_total")
				.build(),
			stale_served: meter.u64_counter("media_proxy_stale_served_total").build(),
			animations: meter.u64_counter("media_proxy_animations_total").build(),
			animation_frames: meter
				.u64_counter("media_proxy_animation_frames_total")
				.build(),
			animation_input_bytes: meter
				.u64_counter("media_proxy_animation_input_bytes_total")
				.build(),
			animation_output_bytes: meter
				.u64_counter("media_proxy_animation_output_bytes_total")
				.build(),
			uptime: meter.f64_gauge("media_proxy_uptime").with_unit("s").build(),
		}
	}

	fn record_completion(&self, status: u16, has_error: bool, total_duration: Duration) {
		let final_status_class = KeyValue::new("status_class", status_class(status));
		self.requests
			.add(1, std::slice::from_ref(&final_status_class));
		if status >= 400 || has_error {
			self.errors
				.add(1, std::slice::from_ref(&final_status_class));
		}
		self.request_duration
			.record(total_duration.as_secs_f64(), &[]);
	}

	fn record_domains(&self, caller_domain: &str, target_domain: &str, is_error: bool) {
		self.domain_requests.add(
			1,
			&[
				KeyValue::new("caller_domain", caller_domain.to_owned()),
				KeyValue::new("target_domain", target_domain.to_owned()),
				KeyValue::new("error", is_error),
			],
		);
	}

	fn record_phases(&self, timings: &PhaseTimings, is_static_path: bool) {
		if let Some(dns_hit) = timings.dns_hit {
			self.dns_cache_requests
				.add(1, &[KeyValue::new("result", dns_hit.to_string())]);
		}
		if let Some(cache_result) = timings.cache_result {
			let attributes = [KeyValue::new("result", cache_result.to_string())];
			self.cache_requests.add(1, &attributes);
			if is_static_path {
				self.static_requests.add(1, &attributes);
			}
		}
		if let Some(upstream_status) = timings.upstream_status {
			self.upstream_responses.add(
				1,
				&[KeyValue::new("status_class", status_class(upstream_status))],
			);
		}
		self.url_check_duration
			.record(timings.check.as_secs_f64(), &[]);
		self.download_wait_duration
			.record(timings.dl_wait.as_secs_f64(), &[]);
		self.upstream_ttfb_duration
			.record(timings.ttfb.as_secs_f64(), &[]);
		self.upstream_body_duration
			.record(timings.body.as_secs_f64(), &[]);
		self.cpu_wait_duration
			.record(timings.cpu_wait.as_secs_f64(), &[]);
		self.buffer_wait_duration
			.record(timings.buffer_wait.as_secs_f64(), &[]);
		self.decode_duration
			.record(timings.decode.as_secs_f64(), &[]);
		self.encode_duration
			.record(timings.encode.as_secs_f64(), &[]);
	}

	fn record_outcome(
		&self,
		timings: &PhaseTimings,
		status: u16,
		has_error: bool,
		error_detail: Option<&str>,
	) {
		if timings.passthrough {
			self.passthrough.add(1, &[]);
		}
		if matches!(timings.cache_result, Some(CacheResult::Stale)) {
			self.stale_served.add(1, &[]);
		}
		if let Some(fetch_error) = timings.fetch_err.as_deref() {
			self.fetch_errors.add(
				1,
				&[KeyValue::new("category", fetch_error_category(fetch_error))],
			);
		} else if status >= 400 || has_error {
			if let Some(category) = proxy_error_category(error_detail) {
				self.processing_errors
					.add(1, &[KeyValue::new("category", category)]);
			}
		}
		if timings.fetch_retry_succeeded {
			self.fetch_retry_successes.add(1, &[]);
		}
		if timings.anim {
			self.animations.add(1, &[]);
			self.animation_frames.add(timings.anim_frames as u64, &[]);
			self.animation_input_bytes
				.add(timings.anim_in_bytes as u64, &[]);
			self.animation_output_bytes
				.add(timings.anim_out_bytes as u64, &[]);
		}
	}

	fn record_processed_output(
		&self,
		content_type: Option<&str>,
		input_bytes: usize,
		output_bytes: usize,
	) {
		self.outputs
			.add(1, &[KeyValue::new("format", output_format(content_type))]);
		self.input_bytes.add(input_bytes as u64, &[]);
		self.output_bytes.add(output_bytes as u64, &[]);
	}

	fn record_resource_snapshot(
		&self,
		snapshot: ResourceSnapshot,
		eviction_deltas: (u64, u64),
		dns_retry_deltas: (u64, u64),
	) {
		self.cache_entries.record(snapshot.cache_entries, &[]);
		self.cache_bytes.record(snapshot.cache_bytes, &[]);
		self.cache_capacity_bytes
			.record(snapshot.cache_capacity_bytes, &[]);
		self.cache_capacity_evictions.add(eviction_deltas.0, &[]);
		self.cache_expired_evictions.add(eviction_deltas.1, &[]);
		self.singleflight_active
			.record(snapshot.singleflight_active, &[]);
		self.downloads_active.record(snapshot.downloads_active, &[]);
		self.downloads_limit.record(snapshot.downloads_limit, &[]);
		self.cpu_active.record(snapshot.cpu_active, &[]);
		self.cpu_limit.record(snapshot.cpu_limit, &[]);
		self.buffer_used_bytes
			.record(snapshot.buffer_used_bytes, &[]);
		self.buffer_limit_bytes
			.record(snapshot.buffer_limit_bytes, &[]);
		self.dns_cache_entries
			.record(snapshot.dns_cache_entries, &[]);
		self.dns_cache_capacity_entries
			.record(snapshot.dns_cache_capacity_entries, &[]);
		self.dns_retry_attempts.add(dns_retry_deltas.0, &[]);
		self.dns_retry_successes.add(dns_retry_deltas.1, &[]);
		self.uptime.record(snapshot.uptime_seconds, &[]);
	}
}

fn output_format(content_type: Option<&str>) -> &'static str {
	match content_type
		.unwrap_or_default()
		.split(';')
		.next()
		.unwrap_or_default()
		.trim()
	{
		"image/jpeg" => "jpeg",
		"image/png" => "png",
		"image/webp" => "webp",
		"image/avif" => "avif",
		_ => "other",
	}
}

struct ResourceSnapshot {
	cache_entries: u64,
	cache_bytes: u64,
	cache_capacity_bytes: u64,
	singleflight_active: u64,
	downloads_active: u64,
	downloads_limit: u64,
	cpu_active: u64,
	cpu_limit: u64,
	buffer_used_bytes: u64,
	buffer_limit_bytes: u64,
	dns_cache_entries: u64,
	dns_cache_capacity_entries: u64,
	uptime_seconds: f64,
}

fn record_resource_metrics(
	metrics: &OtlpMetrics,
	response_cache: &ResponseCache,
	dns_cache: &DnsCache,
	download_semaphore: &Semaphore,
	download_limit: usize,
	cpu_semaphore: &Semaphore,
	cpu_limit: usize,
	buffer_budget: &Semaphore,
	buffer_limit: usize,
	cache_capacity_bytes: u64,
	process_started: Instant,
	previous_evictions: &mut (u64, u64),
	previous_dns_retries: &mut (u64, u64),
) {
	let (cache_entries, cache_bytes) = response_cache.stats();
	let cumulative_evictions = response_cache.cumulative_evictions();
	let eviction_deltas = (
		cumulative_evictions.0.saturating_sub(previous_evictions.0),
		cumulative_evictions.1.saturating_sub(previous_evictions.1),
	);
	*previous_evictions = cumulative_evictions;
	let cumulative_dns_retries = dns_cache.cumulative_retries();
	let dns_retry_deltas = (
		cumulative_dns_retries
			.0
			.saturating_sub(previous_dns_retries.0),
		cumulative_dns_retries
			.1
			.saturating_sub(previous_dns_retries.1),
	);
	*previous_dns_retries = cumulative_dns_retries;
	metrics.record_resource_snapshot(
		ResourceSnapshot {
			cache_entries: cache_entries as u64,
			cache_bytes: cache_bytes as u64,
			cache_capacity_bytes,
			singleflight_active: response_cache.inflight_count() as u64,
			downloads_active: download_limit.saturating_sub(download_semaphore.available_permits())
				as u64,
			downloads_limit: download_limit as u64,
			cpu_active: cpu_limit.saturating_sub(cpu_semaphore.available_permits()) as u64,
			cpu_limit: cpu_limit as u64,
			buffer_used_bytes: buffer_limit.saturating_sub(buffer_budget.available_permits())
				as u64,
			buffer_limit_bytes: buffer_limit as u64,
			dns_cache_entries: dns_cache.len() as u64,
			dns_cache_capacity_entries: dns_cache.max_entries as u64,
			uptime_seconds: process_started.elapsed().as_secs_f64(),
		},
		eviction_deltas,
		dns_retry_deltas,
	);
}

fn duration_histogram(meter: &opentelemetry::metrics::Meter, name: &'static str) -> Histogram<f64> {
	meter.f64_histogram(name).with_unit("s").build()
}

fn status_class(status: u16) -> &'static str {
	match status {
		100..=199 => "1xx",
		200..=299 => "2xx",
		300..=399 => "3xx",
		400..=499 => "4xx",
		500..=599 => "5xx",
		_ => "other",
	}
}

fn proxy_error_category(detail: Option<&str>) -> Option<&'static str> {
	let detail = detail.unwrap_or_default().to_ascii_lowercase();
	if detail.starts_with("status:") {
		None
	} else if detail.contains("decode") || detail.contains("unknown format") {
		Some("decode")
	} else if detail.contains("encode") || detail.contains("cpusemaphore") {
		Some("encode")
	} else if detail.contains("length") || detail.contains("bufferbudget") {
		Some("size")
	} else if detail.contains("blocked")
		|| detail.contains("private")
		|| detail.contains("loopback")
		|| detail.starts_with("scheme:")
		|| detail == "no host"
		|| detail == "no port"
		|| detail.contains("relativeurlwithoutbase")
		|| detail.contains("emptyhost")
		|| detail.contains("invalidport")
		|| detail.contains("invalidipv")
		|| detail.contains("idna")
	{
		Some("policy")
	} else {
		Some("internal")
	}
}

fn fetch_error_category(detail: &str) -> &'static str {
	match detail.split(':').next().unwrap_or("other") {
		"dns" => "dns",
		"connect" => "connect",
		"timeout" => "timeout",
		"reset" => "reset",
		"body" => "body",
		"throttle" => "throttle",
		_ => "other",
	}
}

/// 定期統計ログ用のグローバルカウンタ。
/// emit_summary でインクリメントし、60秒ごとにリセット＋ログ出力する。
struct GlobalStats {
	otlp: Option<Arc<OtlpMetrics>>,
	requests: AtomicU64,
	errors: AtomicU64,
	cache_hits: AtomicU64,
	cache_misses: AtomicU64,
	cache_evicted_reaccesses: AtomicU64,
	ferr_connect: AtomicU64,
	ferr_timeout: AtomicU64,
	ferr_dns: AtomicU64,
	ferr_reset: AtomicU64,
	ferr_body: AtomicU64,
	ferr_throttle: AtomicU64,
	ferr_other: AtomicU64,
	throttle_passed: AtomicU64,
	throttle_waited: AtomicU64,
	throttle_rejected: AtomicU64,
	throttle_wait_ms: AtomicU64,
	throttle_wait_max_ms: AtomicU64,
	retry_attempts: AtomicU64,
	retry_saved: AtomicU64,
	http1_responses: AtomicU64,
	http2_responses: AtomicU64,
	cache_stale_served: AtomicU64,
	upstream_2xx: AtomicU64,
	upstream_3xx: AtomicU64,
	upstream_404: AtomicU64,
	upstream_410: AtomicU64,
	upstream_429: AtomicU64,
	upstream_other_4xx: AtomicU64,
	upstream_5xx: AtomicU64,
	dl_wait_ms: AtomicU64,
	dl_wait_max_ms: AtomicU64,
	cpu_wait_ms: AtomicU64,
	cpu_wait_max_ms: AtomicU64,
	ttfb_max_ms: AtomicU64,
	body_max_ms: AtomicU64,
	decode_max_ms: AtomicU64,
	encode_max_ms: AtomicU64,
	dl_active_max: AtomicU64,
	cpu_active_max: AtomicU64,
	buf_used_max_bytes: AtomicU64,
	perr_upstream_status: AtomicU64,
	decode_error: AtomicU64,
	encode_error: AtomicU64,
	size_reject: AtomicU64,
	policy_reject: AtomicU64,
	internal_error: AtomicU64,
	passthrough_requests: AtomicU64,
	output_jpeg: AtomicU64,
	output_png: AtomicU64,
	output_webp: AtomicU64,
	output_avif: AtomicU64,
	output_other: AtomicU64,
	processed_input_bytes: AtomicU64,
	processed_output_bytes: AtomicU64,
	static_requests: AtomicU64,
	static_hits: AtomicU64,
	static_misses: AtomicU64,
	static_joined: AtomicU64,
	static_cache_insertions: AtomicU64,
	static_output_bytes: AtomicU64,
}
impl GlobalStats {
	fn new() -> Self {
		Self {
			otlp: None,
			requests: AtomicU64::new(0),
			errors: AtomicU64::new(0),
			cache_hits: AtomicU64::new(0),
			cache_misses: AtomicU64::new(0),
			cache_evicted_reaccesses: AtomicU64::new(0),
			ferr_connect: AtomicU64::new(0),
			ferr_timeout: AtomicU64::new(0),
			ferr_dns: AtomicU64::new(0),
			ferr_reset: AtomicU64::new(0),
			ferr_body: AtomicU64::new(0),
			ferr_throttle: AtomicU64::new(0),
			ferr_other: AtomicU64::new(0),
			throttle_passed: AtomicU64::new(0),
			throttle_waited: AtomicU64::new(0),
			throttle_rejected: AtomicU64::new(0),
			throttle_wait_ms: AtomicU64::new(0),
			throttle_wait_max_ms: AtomicU64::new(0),
			retry_attempts: AtomicU64::new(0),
			retry_saved: AtomicU64::new(0),
			http1_responses: AtomicU64::new(0),
			http2_responses: AtomicU64::new(0),
			cache_stale_served: AtomicU64::new(0),
			upstream_2xx: AtomicU64::new(0),
			upstream_3xx: AtomicU64::new(0),
			upstream_404: AtomicU64::new(0),
			upstream_410: AtomicU64::new(0),
			upstream_429: AtomicU64::new(0),
			upstream_other_4xx: AtomicU64::new(0),
			upstream_5xx: AtomicU64::new(0),
			dl_wait_ms: AtomicU64::new(0),
			dl_wait_max_ms: AtomicU64::new(0),
			cpu_wait_ms: AtomicU64::new(0),
			cpu_wait_max_ms: AtomicU64::new(0),
			ttfb_max_ms: AtomicU64::new(0),
			body_max_ms: AtomicU64::new(0),
			decode_max_ms: AtomicU64::new(0),
			encode_max_ms: AtomicU64::new(0),
			dl_active_max: AtomicU64::new(0),
			cpu_active_max: AtomicU64::new(0),
			buf_used_max_bytes: AtomicU64::new(0),
			perr_upstream_status: AtomicU64::new(0),
			decode_error: AtomicU64::new(0),
			encode_error: AtomicU64::new(0),
			size_reject: AtomicU64::new(0),
			policy_reject: AtomicU64::new(0),
			internal_error: AtomicU64::new(0),
			passthrough_requests: AtomicU64::new(0),
			output_jpeg: AtomicU64::new(0),
			output_png: AtomicU64::new(0),
			output_webp: AtomicU64::new(0),
			output_avif: AtomicU64::new(0),
			output_other: AtomicU64::new(0),
			processed_input_bytes: AtomicU64::new(0),
			processed_output_bytes: AtomicU64::new(0),
			static_requests: AtomicU64::new(0),
			static_hits: AtomicU64::new(0),
			static_misses: AtomicU64::new(0),
			static_joined: AtomicU64::new(0),
			static_cache_insertions: AtomicU64::new(0),
			static_output_bytes: AtomicU64::new(0),
		}
	}
	fn with_otlp(provider: Option<&SdkMeterProvider>) -> Self {
		let mut stats = Self::new();
		stats.otlp = provider.map(|provider| Arc::new(OtlpMetrics::new(provider)));
		stats
	}
	/// カウンタをリセットし、リセット前の値を返す。
	fn swap_reset(&self) -> (u64, u64, u64, u64) {
		(
			self.requests.swap(0, Ordering::Relaxed),
			self.errors.swap(0, Ordering::Relaxed),
			self.cache_hits.swap(0, Ordering::Relaxed),
			self.cache_misses.swap(0, Ordering::Relaxed),
		)
	}
	/// fetch_err カウンタをリセットし値を返す。
	fn swap_reset_ferr(&self) -> (u64, u64, u64, u64, u64, u64, u64) {
		(
			self.ferr_connect.swap(0, Ordering::Relaxed),
			self.ferr_timeout.swap(0, Ordering::Relaxed),
			self.ferr_dns.swap(0, Ordering::Relaxed),
			self.ferr_reset.swap(0, Ordering::Relaxed),
			self.ferr_body.swap(0, Ordering::Relaxed),
			self.ferr_throttle.swap(0, Ordering::Relaxed),
			self.ferr_other.swap(0, Ordering::Relaxed),
		)
	}
	/// retry カウンタをリセットし値を返す。
	fn swap_reset_retry(&self) -> (u64, u64) {
		(
			self.retry_attempts.swap(0, Ordering::Relaxed),
			self.retry_saved.swap(0, Ordering::Relaxed),
		)
	}
	/// HTTP バージョンカウンタをリセットし値を返す。
	fn swap_reset_http(&self) -> (u64, u64) {
		(
			self.http1_responses.swap(0, Ordering::Relaxed),
			self.http2_responses.swap(0, Ordering::Relaxed),
		)
	}
	/// fetch_err 分類に応じたカウンタをインクリメントする。
	fn inc_ferr(&self, cat: &str) {
		match cat {
			"connect" => {
				self.ferr_connect.fetch_add(1, Ordering::Relaxed);
			}
			"timeout" => {
				self.ferr_timeout.fetch_add(1, Ordering::Relaxed);
			}
			"dns" => {
				self.ferr_dns.fetch_add(1, Ordering::Relaxed);
			}
			"reset" => {
				self.ferr_reset.fetch_add(1, Ordering::Relaxed);
			}
			"body" => {
				self.ferr_body.fetch_add(1, Ordering::Relaxed);
			}
			"throttle" => {
				self.ferr_throttle.fetch_add(1, Ordering::Relaxed);
			}
			_ => {
				self.ferr_other.fetch_add(1, Ordering::Relaxed);
			}
		}
	}
	fn observe_host_throttle(&self, result: &Result<HostThrottlePermit, HostThrottleRejection>) {
		let (outcome, wait) = match result {
			Ok(permit) if !permit.throttled => {
				self.throttle_passed.fetch_add(1, Ordering::Relaxed);
				("passed", permit.waited)
			}
			Ok(permit) => {
				self.throttle_waited.fetch_add(1, Ordering::Relaxed);
				("waited", permit.waited)
			}
			Err(rejection) => {
				self.throttle_rejected.fetch_add(1, Ordering::Relaxed);
				("rejected", rejection.waited)
			}
		};
		let wait_ms = wait.as_millis() as u64;
		self.throttle_wait_ms.fetch_add(wait_ms, Ordering::Relaxed);
		self.throttle_wait_max_ms
			.fetch_max(wait_ms, Ordering::Relaxed);
		if let Some(metrics) = &self.otlp {
			metrics
				.host_throttle_requests
				.add(1, &[KeyValue::new("result", outcome)]);
			metrics
				.host_throttle_wait_duration
				.record(wait.as_secs_f64(), &[KeyValue::new("result", outcome)]);
		}
	}
	fn observe_request(&self, timings: &PhaseTimings, is_static_path: bool) {
		if matches!(timings.cache_result, Some(CacheResult::Evicted)) {
			self.cache_evicted_reaccesses
				.fetch_add(1, Ordering::Relaxed);
		}
		let dl_wait_ms = timings.dl_wait.as_millis() as u64;
		let cpu_wait_ms = timings.cpu_wait.as_millis() as u64;
		self.dl_wait_ms.fetch_add(dl_wait_ms, Ordering::Relaxed);
		self.dl_wait_max_ms.fetch_max(dl_wait_ms, Ordering::Relaxed);
		self.cpu_wait_ms.fetch_add(cpu_wait_ms, Ordering::Relaxed);
		self.cpu_wait_max_ms
			.fetch_max(cpu_wait_ms, Ordering::Relaxed);
		self.ttfb_max_ms
			.fetch_max(timings.ttfb.as_millis() as u64, Ordering::Relaxed);
		self.body_max_ms
			.fetch_max(timings.body.as_millis() as u64, Ordering::Relaxed);
		self.decode_max_ms
			.fetch_max(timings.decode.as_millis() as u64, Ordering::Relaxed);
		self.encode_max_ms
			.fetch_max(timings.encode.as_millis() as u64, Ordering::Relaxed);
		if timings.passthrough {
			self.passthrough_requests.fetch_add(1, Ordering::Relaxed);
		}
		if is_static_path {
			self.static_requests.fetch_add(1, Ordering::Relaxed);
			match timings.cache_result {
				Some(CacheResult::Hit) => {
					self.static_hits.fetch_add(1, Ordering::Relaxed);
				}
				Some(CacheResult::Miss | CacheResult::Evicted) => {
					self.static_misses.fetch_add(1, Ordering::Relaxed);
				}
				Some(CacheResult::Joined) => {
					self.static_joined.fetch_add(1, Ordering::Relaxed);
				}
				_ => {}
			}
		}
		if let Some(status) = timings.upstream_status {
			match status {
				200..=299 => &self.upstream_2xx,
				300..=399 => &self.upstream_3xx,
				404 => &self.upstream_404,
				410 => &self.upstream_410,
				429 => &self.upstream_429,
				400..=499 => &self.upstream_other_4xx,
				500..=599 => &self.upstream_5xx,
				_ => return,
			}
			.fetch_add(1, Ordering::Relaxed);
		}
	}
	fn observe_dl_active(&self, active: usize) {
		self.dl_active_max
			.fetch_max(active as u64, Ordering::Relaxed);
	}
	fn observe_cpu_active(&self, active: usize) {
		self.cpu_active_max
			.fetch_max(active as u64, Ordering::Relaxed);
	}
	fn observe_buf_used(&self, bytes: usize) {
		self.buf_used_max_bytes
			.fetch_max(bytes as u64, Ordering::Relaxed);
	}
	fn inc_proxy_error(&self, timings: &PhaseTimings, detail: Option<&str>) {
		if timings.fetch_err.is_some() {
			return;
		}
		if detail
			.unwrap_or_default()
			.to_ascii_lowercase()
			.starts_with("status:")
		{
			self.perr_upstream_status.fetch_add(1, Ordering::Relaxed);
			return;
		}
		let counter = match proxy_error_category(detail) {
			Some("decode") => &self.decode_error,
			Some("encode") => &self.encode_error,
			Some("size") => &self.size_reject,
			Some("policy") => &self.policy_reject,
			_ => &self.internal_error,
		};
		counter.fetch_add(1, Ordering::Relaxed);
	}
	fn record_processed_output(
		&self,
		content_type: Option<&str>,
		input_bytes: usize,
		output_bytes: usize,
	) {
		if let Some(metrics) = &self.otlp {
			metrics.record_processed_output(content_type, input_bytes, output_bytes);
		}
		let counter = match content_type
			.unwrap_or_default()
			.split(';')
			.next()
			.unwrap_or_default()
			.trim()
		{
			"image/jpeg" => &self.output_jpeg,
			"image/png" => &self.output_png,
			"image/webp" => &self.output_webp,
			"image/avif" => &self.output_avif,
			_ => &self.output_other,
		};
		counter.fetch_add(1, Ordering::Relaxed);
		self.processed_input_bytes
			.fetch_add(input_bytes as u64, Ordering::Relaxed);
		self.processed_output_bytes
			.fetch_add(output_bytes as u64, Ordering::Relaxed);
	}
	fn record_static_insertion(&self, output_bytes: usize) {
		self.static_cache_insertions.fetch_add(1, Ordering::Relaxed);
		self.static_output_bytes
			.fetch_add(output_bytes as u64, Ordering::Relaxed);
	}
}

type AppState = (
	reqwest::Client,
	Arc<ConfigFile>,
	Arc<Vec<u8>>,
	Arc<resvg::usvg::fontdb::Database>,
	Arc<Semaphore>,
	Arc<NetworkPolicy>,
	Arc<DnsCache>,
	Arc<ResponseCache>,
	Arc<Semaphore>,
	Arc<Semaphore>,
	Arc<GlobalStats>,
	Option<Arc<HostThrottle>>,
	Arc<cache::NegativeCache>,
);

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigFile {
	bind_addr: String,
	timeout: u64,
	user_agent: String,
	max_size: u64,
	proxy: Option<String>,
	filter_type: FilterType,
	max_pixels: u32,
	append_headers: Vec<String>,
	load_system_fonts: bool,
	webp_quality: f32,
	encode_avif: bool,
	allowed_networks: Option<Vec<String>>,
	blocked_networks: Option<Vec<String>>,
	blocked_hosts: Option<Vec<String>>,
	/// Octal permission bits for a Unix-domain `bind_addr`, e.g. "0660".
	#[serde(default)]
	unix_socket_permissions: Option<String>,
	/// 正常完了かつ全フェーズ合計がこのms未満のリクエストは、
	/// アクセスログを INFO ではなく DEBUG に落とす(ヘルスチェック等でログが埋まるのを防ぐ)。
	#[serde(default = "default_slow_log_ms")]
	slow_log_ms: u64,
	#[serde(default = "default_true")]
	enable_cache: bool,
	/// キャッシュ合計バイト数上限(既定128MB)。
	#[serde(default = "default_cache_max_bytes")]
	cache_max_bytes: u64,
	/// 1エントリのバイト数上限(既定5MB)。超過するレスポンスはキャッシュしない。
	#[serde(default = "default_cache_entry_max_bytes")]
	cache_entry_max_bytes: u64,
	/// キャッシュTTL(秒、既定3600)。
	#[serde(default = "default_cache_ttl_secs")]
	cache_ttl_secs: u64,
	/// 上流404のネガティブキャッシュTTL(秒、既定3600)。0で無効。
	#[serde(default = "default_negative_cache_404_ttl_secs")]
	negative_cache_404_ttl_secs: u64,
	/// 上流410のネガティブキャッシュTTL(秒、既定86400)。0で無効。
	#[serde(default = "default_negative_cache_410_ttl_secs")]
	negative_cache_410_ttl_secs: u64,
	/// 上流404/410ネガティブキャッシュの最大URL数(既定16384)。
	#[serde(default = "default_negative_cache_max_entries")]
	negative_cache_max_entries: usize,
	/// パススルー対象の最大バイトサイズ(既定1MB)。
	/// webp/png/jpeg/gif かつ badge/static 未指定かつ寸法が目標以下かつこのサイズ以下なら
	/// デコード・再エンコードせず元バイト列を返す。
	#[serde(default = "default_passthrough_max_bytes")]
	passthrough_max_bytes: u64,
	/// DNS解決失敗のネガティブキャッシュTTL(秒、既定10)。
	/// 落ちているドメインへの連続リクエストが毎回1.5秒のDNSタイムアウトを踏むのを防ぐ。
	#[serde(default = "default_dns_negative_ttl_secs")]
	dns_negative_ttl_secs: u64,
	/// DNS解決のタイムアウト(ms、既定4000)。タイムアウト時は1回リトライする(合計最大約8秒)。
	#[serde(default = "default_dns_timeout_ms")]
	dns_timeout_ms: u64,
	/// DNSキャッシュのTTL(秒、既定300)。
	#[serde(default = "default_dns_ttl_secs")]
	dns_ttl_secs: u64,
	/// DNSキャッシュの最大エントリ数(既定1024)。
	#[serde(default = "default_dns_cache_max_entries")]
	dns_cache_max_entries: usize,
	/// JPEG出力用の品質(0-100、既定85)。webp_qualityの流用をやめる。
	#[serde(default = "default_jpeg_quality")]
	jpeg_quality: i32,
	/// WebPエンコードのmethod(0-6、既定4)。
	/// 値が小さいほどエンコードが速いが圧縮率が下がる。Pi等の低性能環境ではmethod=2を推奨。
	#[serde(default = "default_webp_method")]
	webp_method: i32,
	/// ダウンロードの最大同時接続数(既定24)。バースト時にオリジンへの同時接続が無制限にならないようにする。
	#[serde(default = "default_max_concurrent_downloads")]
	max_concurrent_downloads: usize,
	/// 同時ダウンロードの合計バイト予算(既定256MB)。
	/// load_all前に予約し、エンコード完了後に解放する。
	#[serde(default = "default_inflight_buffer_budget")]
	inflight_buffer_budget_bytes: u64,
	/// TCP接続タイムアウト(ms、既定3000)。全体タイムアウト(timeout)より小さく設定すること。
	#[serde(default = "default_connect_timeout_ms")]
	connect_timeout_ms: u64,
	/// 接続段階失敗時のリトライ前待機(ms、既定500)。SYN再送で瞬断窓を跨ぐ効果を狙う。
	#[serde(default = "default_fetch_retry_delay_ms")]
	fetch_retry_delay_ms: u64,
	/// 特定ホストへの要求を平滑化し、429後はRetry-Afterまで送信を止める。
	#[serde(default)]
	host_throttle: Option<HostThrottleConfig>,
	/// stale-if-error の保持上限(秒、既定86400=24時間)。
	/// TTL切れ後もこの期間はstaleとして保持し、フェッチ失敗時に返す。0で無効。
	#[serde(default = "default_cache_stale_max_secs")]
	cache_stale_max_secs: u64,
	/// OTLP/HTTP metrics の送信先。未設定ならメトリクス送信を無効化する。
	/// シグナルパスを含む完全なURLを指定する。
	#[serde(default)]
	otlp_metrics_endpoint: Option<String>,
	/// OTLP metrics の送信間隔(ms、既定5000)。
	#[serde(default = "default_otlp_export_interval_ms")]
	otlp_export_interval_ms: u64,
	/// OpenTelemetry resource の service.name。
	#[serde(default = "default_otlp_service_name")]
	otlp_service_name: String,
}
fn default_slow_log_ms() -> u64 {
	50
}
fn default_true() -> bool {
	true
}
fn default_cache_max_bytes() -> u64 {
	128 * 1024 * 1024
}
fn default_cache_entry_max_bytes() -> u64 {
	5 * 1024 * 1024
}
fn default_cache_ttl_secs() -> u64 {
	3600
}
fn default_negative_cache_404_ttl_secs() -> u64 {
	3600
}
fn default_negative_cache_410_ttl_secs() -> u64 {
	86400
}
fn default_negative_cache_max_entries() -> usize {
	16384
}
fn default_passthrough_max_bytes() -> u64 {
	1024 * 1024
}
fn default_dns_negative_ttl_secs() -> u64 {
	10
}
fn default_dns_timeout_ms() -> u64 {
	4000
}
fn default_dns_ttl_secs() -> u64 {
	300
}
fn default_dns_cache_max_entries() -> usize {
	1024
}
fn default_jpeg_quality() -> i32 {
	85
}
fn default_webp_method() -> i32 {
	4
}
fn default_max_concurrent_downloads() -> usize {
	24
}
fn default_inflight_buffer_budget() -> u64 {
	256 * 1024 * 1024
}
fn default_connect_timeout_ms() -> u64 {
	3000
}
fn default_fetch_retry_delay_ms() -> u64 {
	500
}
fn default_cache_stale_max_secs() -> u64 {
	86400
}
fn default_otlp_export_interval_ms() -> u64 {
	5000
}
fn default_otlp_service_name() -> String {
	env!("CARGO_PKG_NAME").to_owned()
}
#[derive(Debug, Deserialize)]
pub struct RequestParams {
	url: String,
	//#[serde(rename = "static")]
	r#static: Option<String>,
	emoji: Option<String>,
	avatar: Option<String>,
	preview: Option<String>,
	badge: Option<String>,
	fallback: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum FilterType {
	Nearest,
	Triangle,
	CatmullRom,
	Gaussian,
	Lanczos3,
}
impl From<FilterType> for image::imageops::FilterType {
	fn from(val: FilterType) -> Self {
		match val {
			FilterType::Nearest => image::imageops::Nearest,
			FilterType::Triangle => image::imageops::Triangle,
			FilterType::CatmullRom => image::imageops::CatmullRom,
			FilterType::Gaussian => image::imageops::Gaussian,
			FilterType::Lanczos3 => image::imageops::Lanczos3,
		}
	}
}
impl From<FilterType> for fast_image_resize::FilterType {
	fn from(val: FilterType) -> Self {
		match val {
			FilterType::Nearest => fast_image_resize::FilterType::Box,
			FilterType::Triangle => fast_image_resize::FilterType::Bilinear,
			FilterType::CatmullRom => fast_image_resize::FilterType::CatmullRom,
			FilterType::Gaussian => fast_image_resize::FilterType::Mitchell,
			FilterType::Lanczos3 => fast_image_resize::FilterType::Lanczos3,
		}
	}
}
async fn shutdown_signal() {
	use futures::{future::FutureExt, pin_mut};
	use tokio::signal;
	let ctrl_c = async {
		signal::ctrl_c()
			.await
			.expect("failed to install Ctrl+C handler");
	}
	.fuse();

	#[cfg(unix)]
	let terminate = async {
		signal::unix::signal(signal::unix::SignalKind::terminate())
			.expect("failed to install signal handler")
			.recv()
			.await;
	}
	.fuse();
	#[cfg(not(unix))]
	let terminate = std::future::pending::<()>().fuse();
	pin_mut!(ctrl_c, terminate);
	futures::select! {
		_ = ctrl_c => {},
		_ = terminate => {},
	}
}
fn init_otlp_metrics(config: &ConfigFile) -> Result<Option<SdkMeterProvider>, String> {
	let Some(endpoint) = config
		.otlp_metrics_endpoint
		.as_deref()
		.filter(|endpoint| !endpoint.trim().is_empty())
	else {
		return Ok(None);
	};
	if config.otlp_export_interval_ms == 0 {
		return Err("otlp_export_interval_ms must be greater than 0".to_owned());
	}
	if config.otlp_service_name.trim().is_empty() {
		return Err("otlp_service_name must not be empty".to_owned());
	}

	let exporter = opentelemetry_otlp::MetricExporter::builder()
		.with_http()
		.with_protocol(Protocol::HttpBinary)
		.with_endpoint(endpoint)
		.with_timeout(Duration::from_secs(3))
		.build()
		.map_err(|error| format!("failed to build OTLP metrics exporter: {error}"))?;
	let reader = PeriodicReader::builder(exporter)
		.with_interval(Duration::from_millis(config.otlp_export_interval_ms))
		.build();
	let resource = opentelemetry_sdk::Resource::builder()
		.with_service_name(config.otlp_service_name.clone())
		.build();
	let provider = SdkMeterProvider::builder()
		.with_resource(resource)
		.with_reader(reader)
		.build();

	Ok(Some(provider))
}
fn main() {
	let process_started = Instant::now();
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
		)
		.init();
	let config_path = match std::env::var("MEDIA_PROXY_CONFIG_PATH") {
		Ok(path) => {
			if path.is_empty() {
				"config.json".to_owned()
			} else {
				path
			}
		}
		Err(_) => "config.json".to_owned(),
	};
	if !std::path::Path::new(&config_path).exists() {
		let default_config=ConfigFile{
			bind_addr: "0.0.0.0:12766".to_owned(),
			timeout:10000,
			user_agent: "https://github.com/yojo-art/media-proxy-rs".to_owned(),
			max_size:32*1024*1024,
			proxy:None,
			filter_type:FilterType::Triangle,
			max_pixels:2048,
			append_headers:[
				"Content-Security-Policy:default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'".to_owned(),
				"Access-Control-Allow-Origin:*".to_owned(),
			].to_vec(),
			load_system_fonts:true,
			webp_quality: 75f32,
			encode_avif:false,
			allowed_networks:None,
			blocked_networks:None,
			blocked_hosts:None,
            unix_socket_permissions:None,
			slow_log_ms:default_slow_log_ms(),
			enable_cache:default_true(),
			cache_max_bytes:default_cache_max_bytes(),
			cache_entry_max_bytes:default_cache_entry_max_bytes(),
			cache_ttl_secs:default_cache_ttl_secs(),
			negative_cache_404_ttl_secs:default_negative_cache_404_ttl_secs(),
			negative_cache_410_ttl_secs:default_negative_cache_410_ttl_secs(),
			negative_cache_max_entries:default_negative_cache_max_entries(),
			passthrough_max_bytes:default_passthrough_max_bytes(),
			dns_negative_ttl_secs:default_dns_negative_ttl_secs(),
			dns_timeout_ms:default_dns_timeout_ms(),
			dns_ttl_secs:default_dns_ttl_secs(),
			dns_cache_max_entries:default_dns_cache_max_entries(),
			jpeg_quality:default_jpeg_quality(),
			webp_method:default_webp_method(),
			max_concurrent_downloads:default_max_concurrent_downloads(),
			inflight_buffer_budget_bytes:default_inflight_buffer_budget(),
			connect_timeout_ms:default_connect_timeout_ms(),
			fetch_retry_delay_ms:default_fetch_retry_delay_ms(),
			host_throttle:None,
			cache_stale_max_secs:default_cache_stale_max_secs(),
            otlp_metrics_endpoint:None,
            otlp_export_interval_ms:default_otlp_export_interval_ms(),
            otlp_service_name:default_otlp_service_name(),
        };
		let default_config = serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path)
			.expect("create default config.json")
			.write_all(default_config.as_bytes())
			.unwrap();
	}
	let mut config: ConfigFile =
		serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();
	if let Ok(networks) = std::env::var("MEDIA_PROXY_ALLOWED_NETWORKS") {
		let mut allowed_networks = config.allowed_networks.take().unwrap_or_default();
		for networks in networks.split(",") {
			allowed_networks.push(networks.to_owned());
		}
		config.allowed_networks.replace(allowed_networks);
	}
	if let Ok(networks) = std::env::var("MEDIA_PROXY_BLOCKED_NETWORKS") {
		let mut blocked_networks = config.blocked_networks.take().unwrap_or_default();
		for networks in networks.split(",") {
			blocked_networks.push(networks.to_owned());
		}
		config.blocked_networks.replace(blocked_networks);
	}
	if let Ok(networks) = std::env::var("MEDIA_PROXY_BLOCKED_HOSTS") {
		let mut blocked_hosts = config.blocked_hosts.take().unwrap_or_default();
		for networks in networks.split(",") {
			blocked_hosts.push(networks.to_owned());
		}
		config.blocked_hosts.replace(blocked_hosts);
	}
	let dummy_png = Arc::new(include_bytes!("../asset/dummy.png").to_vec());
	let config = Arc::new(config);
	let meter_provider = match init_otlp_metrics(&config) {
		Ok(provider) => provider,
		Err(error) => {
			tracing::error!(%error, "設定エラー(OTLP metrics)");
			std::process::exit(1);
		}
	};
	if let Some(provider) = &meter_provider {
		opentelemetry::global::set_meter_provider(provider.clone());
		tracing::info!(
			endpoint = %config.otlp_metrics_endpoint.as_deref().unwrap_or_default(),
			interval_ms = config.otlp_export_interval_ms,
			service_name = %config.otlp_service_name,
			"OTLP metrics exporter enabled"
		);
	}
	// allowed_networks / blocked_networks / blocked_hosts のパースは起動時に1回だけ行う。
	// 不正な設定値はここで明確なエラーメッセージ付きに失敗させる。
	let network_policy = match NetworkPolicy::from_config(&config) {
		Ok(p) => Arc::new(p),
		Err(e) => {
			tracing::error!("設定エラー(network): {}", e);
			std::process::exit(1);
		}
	};
	let unix_socket_mode = match &config.unix_socket_permissions {
		Some(value) => match parse_unix_socket_mode(value) {
			Ok(mode) => mode,
			Err(error) => {
				tracing::error!(%error, "不正なunix_socket_permissions設定");
				std::process::exit(1);
			}
		},
		None => 0o666,
	};
	let dns_cache = Arc::new(DnsCache::new(
		Duration::from_secs(config.dns_ttl_secs),
		Duration::from_secs(config.dns_negative_ttl_secs),
		Duration::from_millis(config.dns_timeout_ms),
		config.dns_cache_max_entries,
	));
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let client = reqwest::ClientBuilder::new();
	let client = match &config.proxy {
		Some(url) => {
			tracing::warn!(
                "プロキシ利用時は接続先に対するSSRF再検証が適用されません。プロキシ側で同等のSSRF対策を行ってください"
            );
			let proxy = match reqwest::Proxy::all(url) {
				Ok(proxy) => proxy,
				Err(error) => {
					tracing::error!(proxy = ?url, %error, "不正なプロキシ設定");
					std::process::exit(1);
				}
			};
			client.proxy(proxy)
		}
		None => client,
	};
	// reqwestのDNS解決をDnsCacheに一本化する。
	// check_urlと実フェッチが同じキャッシュを共有し、1リクエストあたりのDNS解決を実質1回にする。
	let client = client.dns_resolver(Arc::new(ssrf::ValidatingResolver::new(
		dns_cache.clone(),
		network_policy.clone(),
		config.proxy.as_deref(),
	)));
	let client = client
		.connect_timeout(Duration::from_millis(config.connect_timeout_ms))
		.read_timeout(Duration::from_millis(config.timeout));
	if config.connect_timeout_ms >= config.timeout {
		tracing::warn!(
			connect_timeout_ms = config.connect_timeout_ms,
			timeout = config.timeout,
			"connect_timeout_ms >= timeout: リトライの余地がありません"
		);
	}
	let client = client
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.unwrap();
	let mut fontdb = resvg::usvg::fontdb::Database::new();
	if config.load_system_fonts {
		fontdb.load_system_fonts();
	}
	if std::path::Path::new("asset/font/").exists() {
		fontdb.load_fonts_dir("asset/font/");
	}
	fontdb.load_font_source(resvg::usvg::fontdb::Source::Binary(Arc::new(
		include_bytes!("../asset/font/Aileron-Light.otf"),
	)));
	let fontdb = Arc::new(fontdb);
	// let arg_tup=(client,config,dummy_png,fontdb);

	// CPU保護用セマフォ: decode/encodeのspawn_blocking区間のみ保持。
	let max_concurrent_encode = num_cpus::get().max(2);
	let encode_semaphore = Arc::new(Semaphore::new(max_concurrent_encode));
	// ダウンロード並列数制限: req.send()前〜 load_all完了まで保持。
	let download_semaphore = Arc::new(Semaphore::new(config.max_concurrent_downloads));
	// バイト予算: load_all前に予約、エンコード完了後に解放。
	let buffer_budget = Arc::new(Semaphore::new(config.inflight_buffer_budget_bytes as usize));
	let response_cache = Arc::new(ResponseCache::new(cache::CacheConfig {
		enabled: config.enable_cache,
		max_bytes: config.cache_max_bytes as usize,
		entry_max_bytes: config.cache_entry_max_bytes as usize,
		ttl: Duration::from_secs(config.cache_ttl_secs),
		stale_max: Duration::from_secs(config.cache_stale_max_secs),
	}));
	let negative_cache = Arc::new(cache::NegativeCache::new(cache::NegativeCacheConfig {
		enabled: config.enable_cache,
		max_entries: config.negative_cache_max_entries,
		not_found_ttl: Duration::from_secs(config.negative_cache_404_ttl_secs),
		gone_ttl: Duration::from_secs(config.negative_cache_410_ttl_secs),
	}));
	let global_stats = Arc::new(GlobalStats::with_otlp(meter_provider.as_ref()));
	let host_throttle = match config
		.host_throttle
		.as_ref()
		.map(HostThrottle::new)
		.transpose()
	{
		Ok(throttle) => throttle.map(Arc::new),
		Err(error) => {
			tracing::error!(%error, "設定エラー(host_throttle)");
			std::process::exit(1);
		}
	};
	let arg_tup = (
		client,
		config,
		dummy_png,
		fontdb,
		encode_semaphore,
		network_policy,
		dns_cache,
		response_cache,
		download_semaphore,
		buffer_budget,
		global_stats,
		host_throttle,
		negative_cache,
	);
	rt.block_on(async {
		if let Some(metrics) = arg_tup.10.otlp.clone() {
			let response_cache = arg_tup.7.clone();
			let dns_cache = arg_tup.6.clone();
			let download_semaphore = arg_tup.8.clone();
			let cpu_semaphore = arg_tup.4.clone();
			let buffer_budget = arg_tup.9.clone();
			let download_limit = arg_tup.1.max_concurrent_downloads;
			let cpu_limit = max_concurrent_encode;
			let buffer_limit = arg_tup.1.inflight_buffer_budget_bytes as usize;
			let cache_capacity_bytes = arg_tup.1.cache_max_bytes;
			let interval_ms = arg_tup.1.otlp_export_interval_ms;
			tokio::spawn(async move {
				let mut previous_evictions = (0, 0);
				let mut previous_dns_retries = (0, 0);
				let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
				loop {
					interval.tick().await;
					record_resource_metrics(
						&metrics,
						&response_cache,
						&dns_cache,
						&download_semaphore,
						download_limit,
						&cpu_semaphore,
						cpu_limit,
						&buffer_budget,
						buffer_limit,
						cache_capacity_bytes,
						process_started,
						&mut previous_evictions,
						&mut previous_dns_retries,
					);
				}
			});
		}
		// --- 60秒ごとの定期統計ログ ---
		{
			let stats = arg_tup.10.clone();
			let encode_sem = arg_tup.4.clone();
			let dl_sem = arg_tup.8.clone();
			let buf_sem = arg_tup.9.clone();
			let resp_cache = arg_tup.7.clone();
			let negative_cache = arg_tup.12.clone();
			let dns = arg_tup.6.clone();
			let max_encode = max_concurrent_encode;
			let max_dl = arg_tup.1.max_concurrent_downloads;
			let max_buf = arg_tup.1.inflight_buffer_budget_bytes as usize;
			tokio::spawn(async move {
				let mut interval = tokio::time::interval(Duration::from_secs(60));
				interval.tick().await; // 最初の tick は即時発火するのでスキップ
				loop {
					interval.tick().await;
					let (reqs, errs, hits, misses) = stats.swap_reset();
					let cache_evicted_reaccesses =
						stats.cache_evicted_reaccesses.swap(0, Ordering::Relaxed);
					let (fc, ft, fd, fr, fb, fth, fo) = stats.swap_reset_ferr();
					let (retry_att, retry_sav) = stats.swap_reset_retry();
					let (http1, http2) = stats.swap_reset_http();
					let stale_served = stats.cache_stale_served.swap(0, Ordering::Relaxed);
					let throttle_passed = stats.throttle_passed.swap(0, Ordering::Relaxed);
					let throttle_waited = stats.throttle_waited.swap(0, Ordering::Relaxed);
					let throttle_rejected = stats.throttle_rejected.swap(0, Ordering::Relaxed);
					let throttle_wait_ms = stats.throttle_wait_ms.swap(0, Ordering::Relaxed);
					let throttle_wait_max_ms =
						stats.throttle_wait_max_ms.swap(0, Ordering::Relaxed);
					let upstream_2xx = stats.upstream_2xx.swap(0, Ordering::Relaxed);
					let upstream_3xx = stats.upstream_3xx.swap(0, Ordering::Relaxed);
					let upstream_404 = stats.upstream_404.swap(0, Ordering::Relaxed);
					let upstream_410 = stats.upstream_410.swap(0, Ordering::Relaxed);
					let upstream_429 = stats.upstream_429.swap(0, Ordering::Relaxed);
					let upstream_other_4xx = stats.upstream_other_4xx.swap(0, Ordering::Relaxed);
					let upstream_5xx = stats.upstream_5xx.swap(0, Ordering::Relaxed);
					let dl_wait_ms = stats.dl_wait_ms.swap(0, Ordering::Relaxed);
					let dl_wait_max_ms = stats.dl_wait_max_ms.swap(0, Ordering::Relaxed);
					let cpu_wait_ms = stats.cpu_wait_ms.swap(0, Ordering::Relaxed);
					let cpu_wait_max_ms = stats.cpu_wait_max_ms.swap(0, Ordering::Relaxed);
					let ttfb_max_ms = stats.ttfb_max_ms.swap(0, Ordering::Relaxed);
					let body_max_ms = stats.body_max_ms.swap(0, Ordering::Relaxed);
					let decode_max_ms = stats.decode_max_ms.swap(0, Ordering::Relaxed);
					let encode_max_ms = stats.encode_max_ms.swap(0, Ordering::Relaxed);
					let dl_active_max = stats.dl_active_max.swap(0, Ordering::Relaxed);
					let cpu_active_max = stats.cpu_active_max.swap(0, Ordering::Relaxed);
					let buf_used_max_bytes = stats.buf_used_max_bytes.swap(0, Ordering::Relaxed);
					let perr_upstream_status =
						stats.perr_upstream_status.swap(0, Ordering::Relaxed);
					let decode_error = stats.decode_error.swap(0, Ordering::Relaxed);
					let encode_error = stats.encode_error.swap(0, Ordering::Relaxed);
					let size_reject = stats.size_reject.swap(0, Ordering::Relaxed);
					let policy_reject = stats.policy_reject.swap(0, Ordering::Relaxed);
					let internal_error = stats.internal_error.swap(0, Ordering::Relaxed);
					let passthrough_requests =
						stats.passthrough_requests.swap(0, Ordering::Relaxed);
					let output_jpeg = stats.output_jpeg.swap(0, Ordering::Relaxed);
					let output_png = stats.output_png.swap(0, Ordering::Relaxed);
					let output_webp = stats.output_webp.swap(0, Ordering::Relaxed);
					let output_avif = stats.output_avif.swap(0, Ordering::Relaxed);
					let output_other = stats.output_other.swap(0, Ordering::Relaxed);
					let processed_input_bytes =
						stats.processed_input_bytes.swap(0, Ordering::Relaxed);
					let processed_output_bytes =
						stats.processed_output_bytes.swap(0, Ordering::Relaxed);
					let static_requests = stats.static_requests.swap(0, Ordering::Relaxed);
					let static_hits = stats.static_hits.swap(0, Ordering::Relaxed);
					let static_misses = stats.static_misses.swap(0, Ordering::Relaxed);
					let static_joined = stats.static_joined.swap(0, Ordering::Relaxed);
					let static_cache_insertions =
						stats.static_cache_insertions.swap(0, Ordering::Relaxed);
					let static_output_bytes = stats.static_output_bytes.swap(0, Ordering::Relaxed);
					let dl_active = max_dl - dl_sem.available_permits();
					let cpu_active = max_encode - encode_sem.available_permits();
					let buf_used = max_buf - buf_sem.available_permits();
					let (cache_entries, cache_bytes) = resp_cache.stats();
					let negative_cache_entries = negative_cache.len();
					let (cache_capacity_evictions, cache_expired_evictions) =
						resp_cache.swap_reset_evictions();
					let dns_entries = dns.len();
					let (dns_retry_attempts, dns_retry_saved) = dns.swap_reset_retries();
					tracing::info!(
						requests = reqs,
						errors = errs,
						cache_hits = hits,
						cache_misses = misses,
						cache_evicted_reaccesses,
						dl_active = dl_active as u64,
						dl_max = max_dl as u64,
						cpu_active = cpu_active as u64,
						cpu_max = max_encode as u64,
						buf_used_mb = (buf_used / (1024 * 1024)) as u64,
						buf_max_mb = (max_buf / (1024 * 1024)) as u64,
						cache_entries = cache_entries as u64,
						cache_bytes = cache_bytes as u64,
						cache_capacity_evictions,
						cache_expired_evictions,
						negative_cache_entries = negative_cache_entries as u64,
						dns_entries = dns_entries as u64,
						dns_retry_attempts,
						dns_retry_saved,
						ferr_connect = fc,
						ferr_timeout = ft,
						ferr_dns = fd,
						ferr_reset = fr,
						ferr_body = fb,
						ferr_throttle = fth,
						ferr_other = fo,
						throttle_passed,
						throttle_waited,
						throttle_rejected,
						throttle_wait_ms,
						throttle_wait_max_ms,
						retry_attempts = retry_att,
						retry_saved = retry_sav,
						http1_responses = http1,
						http2_responses = http2,
						cache_stale_served = stale_served,
						upstream_2xx,
						upstream_3xx,
						upstream_404,
						upstream_410,
						upstream_429,
						upstream_other_4xx,
						upstream_5xx,
						dl_wait_ms,
						dl_wait_max_ms,
						cpu_wait_ms,
						cpu_wait_max_ms,
						upstream_ttfb_max_ms = ttfb_max_ms,
						body_max_ms,
						decode_max_ms,
						encode_max_ms,
						dl_active_max,
						cpu_active_max,
						buf_used_max_mb = buf_used_max_bytes / (1024 * 1024),
						perr_upstream_status,
						decode_error,
						encode_error,
						size_reject,
						policy_reject,
						internal_error,
						passthrough_requests,
						output_jpeg,
						output_png,
						output_webp,
						output_avif,
						output_other,
						processed_input_bytes,
						processed_output_bytes,
						static_requests,
						static_hits,
						static_misses,
						static_joined,
						static_cache_insertions,
						static_output_bytes,
						"periodic_stats"
					);
				}
			});
		}
		let bind_addr = arg_tup.1.bind_addr.clone();
		let app = Router::new();
		let app = app.route(
			"/healthz",
			axum::routing::get(|| async { (axum::http::StatusCode::OK, "ok") }),
		);
		let arg_tup0 = arg_tup.clone();
		let app = app.route(
			"/",
			axum::routing::get(move |uri, headers, parms| {
				get_file(None, uri, headers, arg_tup0.clone(), parms)
			}),
		);
		let app = app.route(
			"/{*path}",
			axum::routing::get(move |path, uri, headers, parms| {
				get_file(Some(path), uri, headers, arg_tup.clone(), parms)
			}),
		);
		let app = app.layer(tower_http::catch_panic::CatchPanicLayer::new());
		match parse_bind_addr(&bind_addr) {
			Ok(BindTarget::Tcp(address)) => {
				let listener = match tokio::net::TcpListener::bind(address).await {
					Ok(listener) => listener,
					Err(error) => {
						tracing::error!(%address, %error, "TCPソケットのbindに失敗");
						std::process::exit(1);
					}
				};
				tracing::info!(%address, "listening");
				axum::serve(listener, app.into_make_service())
					.with_graceful_shutdown(shutdown_signal())
					.await
					.unwrap();
			}
			Ok(BindTarget::Unix(path)) => {
				#[cfg(not(unix))]
				{
					let _ = path;
					tracing::error!("Unix domain socketはUnix環境でのみ利用できます");
					std::process::exit(1);
				}
				#[cfg(unix)]
				serve_on_unix_socket(app, &path, unix_socket_mode).await;
			}
			Err(error) => {
				tracing::error!(%error, "不正なbind_addr設定");
				std::process::exit(1);
			}
		}
	});
	if let Some(provider) = meter_provider {
		if let Err(error) = provider.shutdown_with_timeout(Duration::from_secs(3)) {
			tracing::warn!(%error, "OTLP metrics exporter shutdown failed");
		}
	}
}

enum BindTarget {
	Tcp(SocketAddr),
	Unix(PathBuf),
}

fn parse_bind_addr(value: &str) -> Result<BindTarget, String> {
	let value = value.trim();
	if let Some(path) = value.strip_prefix("unix://") {
		if path.is_empty() {
			return Err(format!("invalid bind_addr {value:?}: empty socket path"));
		}
		return Ok(BindTarget::Unix(PathBuf::from(path)));
	}
	if let Some(path) = value.strip_prefix("unix:") {
		if path.is_empty() {
			return Err(format!("invalid bind_addr {value:?}: empty socket path"));
		}
		return Ok(BindTarget::Unix(PathBuf::from(path)));
	}
	if let Ok(address) = value.parse::<SocketAddr>() {
		return Ok(BindTarget::Tcp(address));
	}
	if value.contains('/') || value.ends_with(".sock") {
		tracing::warn!(bind_addr = ?value, "schemeなしのbind_addrをUnix domain socketとして扱います。unix://を指定してください");
		return Ok(BindTarget::Unix(PathBuf::from(value)));
	}
	Err(format!(
		"invalid bind_addr {value:?}: expected \"IP:port\" or \"unix:///path/to.sock\""
	))
}

fn parse_unix_socket_mode(value: &str) -> Result<u32, String> {
	let value = value.trim();
	let digits = value.strip_prefix("0o").unwrap_or(value);
	if digits.is_empty() || !digits.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
		return Err(format!("{value:?}: expected octal permission bits"));
	}
	let mode = u32::from_str_radix(digits, 8).map_err(|error| format!("{value:?}: {error}"))?;
	if mode > 0o777 {
		return Err(format!("{value:?}: out of range (expected 0..=0777)"));
	}
	Ok(mode)
}

#[cfg(unix)]
async fn serve_on_unix_socket(app: Router, path: &Path, mode: u32) {
	use std::os::unix::fs::PermissionsExt;

	if let Some(parent) = path.parent() {
		if !parent.as_os_str().is_empty() {
			if let Err(error) = tokio::fs::create_dir_all(parent).await {
				tracing::error!(path = %parent.display(), %error, "Unix socketディレクトリの作成に失敗");
				std::process::exit(1);
			}
		}
	}
	match tokio::fs::symlink_metadata(path).await {
		Ok(_) => {
			if let Err(error) = tokio::fs::remove_file(path).await {
				tracing::error!(path = %path.display(), %error, "既存Unix socketの削除に失敗");
				std::process::exit(1);
			}
		}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
		Err(error) => {
			tracing::error!(path = %path.display(), %error, "Unix socketパスの確認に失敗");
			std::process::exit(1);
		}
	}
	let listener = match tokio::net::UnixListener::bind(path) {
		Ok(listener) => listener,
		Err(error) => {
			tracing::error!(path = %path.display(), %error, "Unix socketのbindに失敗");
			std::process::exit(1);
		}
	};
	if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
		tracing::error!(path = %path.display(), mode = format_args!("{mode:o}"), %error, "Unix socketの権限設定に失敗");
		std::process::exit(1);
	}
	tracing::info!(path = %path.display(), "listening on Unix domain socket");
	axum::serve(listener, app.into_make_service())
		.with_graceful_shutdown(shutdown_signal())
		.await
		.unwrap();
}
/// タイムアウト由来のネガティブキャッシュの短いTTL。
const DNS_TIMEOUT_NEGATIVE_TTL: Duration = Duration::from_secs(2);
const RESOURCE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// stale-while-error の上限倍率(元TTLのこの倍までstaleエントリを使う)。
const DNS_STALE_FACTOR: u32 = 10;

/// 起動時に1度だけパースするネットワークポリシー。
/// check_url には Arc の参照として渡す(リクエストごとの再パースをしない)。
pub struct NetworkPolicy {
	/// 組み込み遮断レンジ(allowedが無ければ遮断)。
	ipv4_blocked_default: IpRange<Ipv4Net>,
	allowed_networks: Option<IpRange<Ipv4Net>>,
	blocked_networks: Option<IpRange<Ipv4Net>>,
	allowed_networks_v6: Option<IpRange<Ipv6Net>>,
	blocked_networks_v6: Option<IpRange<Ipv6Net>>,
	/// 小文字化済みの遮断ホスト集合。
	blocked_hosts: HashSet<String>,
}
impl NetworkPolicy {
	fn normalize_host(host: &str) -> String {
		let host = host.trim_end_matches('.');
		let host = host
			.strip_prefix('[')
			.and_then(|host| host.strip_suffix(']'))
			.unwrap_or(host);
		host.to_lowercase()
	}

	fn parse_ranges(
		list: &[String],
		label: &str,
	) -> Result<(IpRange<Ipv4Net>, IpRange<Ipv6Net>), String> {
		let mut v4_ranges = IpRange::new();
		let mut v6_ranges = IpRange::new();
		for s in list {
			let net: IpNet = s
				.parse()
				.map_err(|e| format!("{} の不正な値 {:?}: {}", label, s, e))?;
			match net {
				IpNet::V4(net) => {
					v4_ranges.add(net);
				}
				IpNet::V6(net) => {
					v6_ranges.add(net);
				}
			}
		}
		v4_ranges.simplify();
		v6_ranges.simplify();
		Ok((v4_ranges, v6_ranges))
	}
	pub fn from_config(config: &ConfigFile) -> Result<Self, String> {
		let (ipv4_blocked_default, _) = Self::parse_ranges(
			&[
				"10.0.0.0/8".to_owned(),
				"172.16.0.0/12".to_owned(),
				"192.168.0.0/16".to_owned(),
				"127.0.0.0/8".to_owned(),
				"169.254.0.0/16".to_owned(),
				"100.64.0.0/10".to_owned(),
				"0.0.0.0/8".to_owned(),
				"192.0.0.0/24".to_owned(),
				"192.0.2.0/24".to_owned(),
				"198.18.0.0/15".to_owned(),
				"198.51.100.0/24".to_owned(),
				"203.0.113.0/24".to_owned(),
				"224.0.0.0/4".to_owned(),
				"240.0.0.0/4".to_owned(),
			],
			"builtin blocked range",
		)?;
		let (allowed_networks, allowed_networks_v6) = match &config.allowed_networks {
			Some(list) => {
				let (v4, v6) = Self::parse_ranges(list, "allowed_networks")?;
				(Some(v4), Some(v6))
			}
			None => (None, None),
		};
		let (blocked_networks, blocked_networks_v6) = match &config.blocked_networks {
			Some(list) => {
				let (v4, v6) = Self::parse_ranges(list, "blocked_networks")?;
				(Some(v4), Some(v6))
			}
			None => (None, None),
		};
		let blocked_hosts = config
			.blocked_hosts
			.as_ref()
			.map(|hosts| hosts.iter().map(|h| Self::normalize_host(h)).collect())
			.unwrap_or_default();
		Ok(Self {
			ipv4_blocked_default,
			allowed_networks,
			blocked_networks,
			allowed_networks_v6,
			blocked_networks_v6,
			blocked_hosts,
		})
	}
	pub(crate) fn is_host_blocked(&self, host: &str) -> bool {
		let host = Self::normalize_host(host);
		self.blocked_hosts.iter().any(|entry| {
			if let Some(suffix) = entry.strip_prefix('.') {
				host.ends_with(&format!(".{}", suffix))
			} else {
				host == *entry || host.ends_with(&format!(".{}", entry))
			}
		})
	}
	/// IPv4アドレスの遮断判定。allowed_networks は遮断より優先。
	fn check_ipv4(&self, ip: &std::net::Ipv4Addr) -> Result<(), String> {
		if let Some(block) = &self.blocked_networks {
			if block.contains(ip) {
				return Err("Blocked address".to_owned());
			}
		}
		if self.ipv4_blocked_default.contains(ip) {
			let allow = self
				.allowed_networks
				.as_ref()
				.is_some_and(|a| a.contains(ip));
			if !allow {
				return Err("Blocked address".to_owned());
			}
		}
		Ok(())
	}
	pub(crate) fn check_ip(&self, ip: IpAddr) -> Result<(), String> {
		match ip {
			IpAddr::V4(v4) => self.check_ipv4(&v4),
			IpAddr::V6(v6) => {
				if self
					.blocked_networks_v6
					.as_ref()
					.is_some_and(|ranges| ranges.contains(&v6))
				{
					return Err("Blocked address".to_owned());
				}
				let segments = v6.segments();
				if segments[0] == 0x2001 && segments[1] == 0 {
					return Err("Blocked address".to_owned());
				}
				if let Some(v4) = ipv6_embedded_ipv4(&v6) {
					return self.check_ipv4(&v4);
				}
				if v6.is_multicast()
					|| v6.is_unicast_link_local()
					|| v6.is_loopback()
					|| v6.is_unspecified()
					|| v6.is_unique_local()
				{
					if self
						.allowed_networks_v6
						.as_ref()
						.is_some_and(|ranges| ranges.contains(&v6))
					{
						return Ok(());
					}
					return Err("Blocked address".to_owned());
				}
				Ok(())
			}
		}
	}
}

fn ipv6_embedded_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
	if let Some(v4) = v6.to_ipv4() {
		return Some(v4);
	}
	let segments = v6.segments();
	if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
		return Some(std::net::Ipv4Addr::new(
			(segments[6] >> 8) as u8,
			segments[6] as u8,
			(segments[7] >> 8) as u8,
			segments[7] as u8,
		));
	}
	if segments[0] == 0x2002 {
		return Some(std::net::Ipv4Addr::new(
			(segments[1] >> 8) as u8,
			segments[1] as u8,
			(segments[2] >> 8) as u8,
			segments[2] as u8,
		));
	}
	if segments[..6] == [0, 0, 0, 0, 0xffff, 0] {
		return Some(std::net::Ipv4Addr::new(
			(segments[6] >> 8) as u8,
			segments[6] as u8,
			(segments[7] >> 8) as u8,
			segments[7] as u8,
		));
	}
	if (segments[4] == 0 || segments[4] == 0x0200) && segments[5] == 0x5efe {
		return Some(std::net::Ipv4Addr::new(
			(segments[6] >> 8) as u8,
			segments[6] as u8,
			(segments[7] >> 8) as u8,
			segments[7] as u8,
		));
	}
	None
}

struct DnsCacheEntry {
	resolved_at: Instant,
	ips: Result<Vec<IpAddr>, String>,
	/// タイムアウト由来の失敗かどうか(ネガティブキャッシュTTLの選別に使う)。
	is_timeout: bool,
}

/// DNS解決結果のキャッシュ状態(ログ用)。
#[derive(Clone, Copy)]
pub enum DnsHitStatus {
	/// キャッシュヒット(TTL内)。
	Hit,
	/// TTL切れだが再解決失敗のため期限切れの値を使用。
	Stale,
	/// キャッシュミス(新規解決)。
	Miss,
}
impl std::fmt::Display for DnsHitStatus {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			DnsHitStatus::Hit => write!(f, "hit"),
			DnsHitStatus::Stale => write!(f, "stale"),
			DnsHitStatus::Miss => write!(f, "miss"),
		}
	}
}

/// DNS singleflight の broadcast で送受信する型。
type DnsResult = Result<Vec<IpAddr>, String>;

/// host -> 解決済みIPの簡易キャッシュ(TTL付き・上限付き・singleflight・stale-while-error)。
pub struct DnsCache {
	inner: RwLock<HashMap<String, DnsCacheEntry>>,
	ttl: Duration,
	negative_ttl: Duration,
	dns_timeout: Duration,
	max_entries: usize,
	/// singleflight: 進行中のDNS解決。同一ホストへの並列lookup_hostを1本に束ねる。
	inflight: Mutex<HashMap<String, tokio::sync::broadcast::Sender<DnsResult>>>,
	lookup_semaphore: Arc<Semaphore>,
	retry_attempts: AtomicU64,
	retry_saved: AtomicU64,
	retry_attempts_total: AtomicU64,
	retry_saved_total: AtomicU64,
}

struct DnsFlightGuard<'a> {
	cache: &'a DnsCache,
	host: String,
	tx: tokio::sync::broadcast::Sender<DnsResult>,
	active: bool,
}
impl DnsFlightGuard<'_> {
	fn complete(mut self) {
		self.remove_if_current();
		self.active = false;
	}
	fn remove_if_current(&self) {
		let mut inflight = self
			.cache
			.inflight
			.lock()
			.unwrap_or_else(|e| e.into_inner());
		let is_current = inflight
			.get(&self.host)
			.is_some_and(|tx| tx.same_channel(&self.tx));
		if is_current {
			inflight.remove(&self.host);
		}
	}
}
impl Drop for DnsFlightGuard<'_> {
	fn drop(&mut self) {
		if self.active {
			self.remove_if_current();
		}
	}
}
impl DnsCache {
	pub fn new(
		ttl: Duration,
		negative_ttl: Duration,
		dns_timeout: Duration,
		max_entries: usize,
	) -> Self {
		Self {
			inner: RwLock::new(HashMap::new()),
			ttl,
			negative_ttl,
			dns_timeout,
			max_entries,
			inflight: Mutex::new(HashMap::new()),
			lookup_semaphore: Arc::new(Semaphore::new(128)),
			retry_attempts: AtomicU64::new(0),
			retry_saved: AtomicU64::new(0),
			retry_attempts_total: AtomicU64::new(0),
			retry_saved_total: AtomicU64::new(0),
		}
	}
	/// エントリのTTLを返す。タイムアウト由来の失敗は短いTTL(2秒)、確定的失敗はnegative_ttl。
	fn entry_ttl(&self, entry: &DnsCacheEntry) -> Duration {
		match &entry.ips {
			Ok(_) => self.ttl,
			Err(_) => {
				if entry.is_timeout {
					DNS_TIMEOUT_NEGATIVE_TTL
				} else {
					self.negative_ttl
				}
			}
		}
	}
	/// stale-while-error の上限判定。成功エントリが元TTLのDNS_STALE_FACTOR倍以内なら stale として使える。
	fn is_stale_usable(&self, entry: &DnsCacheEntry) -> bool {
		entry.ips.is_ok() && entry.resolved_at.elapsed() < self.ttl * DNS_STALE_FACTOR
	}

	/// DNSキャッシュのエントリ数を返す(統計ログ用)。
	pub(crate) fn len(&self) -> usize {
		self.inner.try_read().map(|m| m.len()).unwrap_or(0)
	}
	pub(crate) fn swap_reset_retries(&self) -> (u64, u64) {
		(
			self.retry_attempts.swap(0, Ordering::Relaxed),
			self.retry_saved.swap(0, Ordering::Relaxed),
		)
	}
	pub(crate) fn cumulative_retries(&self) -> (u64, u64) {
		(
			self.retry_attempts_total.load(Ordering::Relaxed),
			self.retry_saved_total.load(Ordering::Relaxed),
		)
	}

	/// hostを非同期解決する。TTL内はキャッシュを返す。singleflight付き。
	/// タイムアウト時は1回リトライする。
	pub async fn resolve(
		&self,
		host: &str,
		port: u16,
	) -> Result<(Vec<IpAddr>, DnsHitStatus), String> {
		// --- キャッシュヒット判定 ---
		{
			let map = self.inner.read().await;
			if let Some(entry) = map.get(host) {
				let ttl = self.entry_ttl(entry);
				if entry.resolved_at.elapsed() < ttl {
					match &entry.ips {
						Ok(ips) => return Ok((ips.clone(), DnsHitStatus::Hit)),
						Err(e) => return Err(e.clone()),
					}
				}
			}
		}
		// --- singleflight: 既に進行中なら合流、そうでなければ自分が処理開始 ---
		let rx_opt = self.try_subscribe(host);
		if let Some(mut rx) = rx_opt {
			if let Ok(Ok(result)) = tokio::time::timeout(self.dns_timeout, rx.recv()).await {
				match result {
					Ok(ips) => return Ok((ips, DnsHitStatus::Miss)),
					Err(e) => return self.try_stale_or_err(host, e).await,
				}
			}
			self.remove_inflight_if_current(host, &rx);
			// broadcast側がdropまたは応答しない→自分で解決にフォールスルー
		}
		// --- singleflight: 処理開始を登録(race check込み) ---
		let tx = match self.register_or_subscribe(host) {
			Err(mut rx) => {
				// 別タスクが先に登録した→合流
				if let Ok(Ok(result)) = tokio::time::timeout(self.dns_timeout, rx.recv()).await {
					match result {
						Ok(ips) => return Ok((ips, DnsHitStatus::Miss)),
						Err(e) => return self.try_stale_or_err(host, e).await,
					}
				}
				self.remove_inflight_if_current(host, &rx);
				// broadcast側がdropまたは応答しない→自分で解決にフォールスルー(txを登録)
				self.force_register(host)
			}
			Ok(tx) => tx,
		};
		let flight_guard = DnsFlightGuard {
			cache: self,
			host: host.to_owned(),
			tx: tx.clone(),
			active: true,
		};
		// --- 実際のDNS解決(リトライ1回付き) ---
		let result = self.do_lookup(host, port).await;
		let result = match &result {
			Err(e) if e.contains("timeout") => {
				// タイムアウト→1回リトライ
				self.retry_attempts.fetch_add(1, Ordering::Relaxed);
				self.retry_attempts_total.fetch_add(1, Ordering::Relaxed);
				let retry = self.do_lookup(host, port).await;
				if retry.is_ok() {
					self.retry_saved.fetch_add(1, Ordering::Relaxed);
					self.retry_saved_total.fetch_add(1, Ordering::Relaxed);
				}
				retry
			}
			_ => result,
		};
		let is_timeout = matches!(&result,Err(e) if e.contains("timeout"));
		// --- キャッシュ格納 ---
		{
			let mut map = self.inner.write().await;
			if map.len() >= self.max_entries && !map.contains_key(host) {
				map.retain(|_, e| {
					let ttl = self.entry_ttl(e);
					// stale-while-error用に成功エントリは長めに保持
					if e.ips.is_ok() {
						e.resolved_at.elapsed() < self.ttl * DNS_STALE_FACTOR
					} else {
						e.resolved_at.elapsed() < ttl
					}
				});
				if map.len() >= self.max_entries {
					if let Some(k) = map.keys().next().cloned() {
						map.remove(&k);
					}
				}
			}
			map.insert(
				host.to_owned(),
				DnsCacheEntry {
					resolved_at: Instant::now(),
					ips: result.clone(),
					is_timeout,
				},
			);
		}
		// --- singleflight: 結果を通知して inflight から削除 ---
		let _ = tx.send(result.clone());
		flight_guard.complete();
		// --- 結果を返す(失敗時はstaleを試す) ---
		match result {
			Ok(addrs) => Ok((addrs, DnsHitStatus::Miss)),
			Err(e) => self.try_stale_or_err(host, e).await,
		}
	}

	/// 1回のlookup_host実行(タイムアウト付き)。
	async fn do_lookup(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
		let permit = self
			.lookup_semaphore
			.clone()
			.acquire_owned()
			.await
			.map_err(|_| "dns semaphore closed".to_owned())?;
		let host_port = format!("{}:{}", host, port);
		let mut lookup_task = tokio::spawn(tokio::net::lookup_host(host_port));
		let outcome = tokio::select! {
			result = &mut lookup_task => Some(result),
			_ = tokio::time::sleep(self.dns_timeout) => None,
		};
		match outcome {
			Some(Ok(Ok(iter))) => {
				let addrs: Vec<_> = iter.map(|sa| sa.ip()).collect();
				if addrs.is_empty() {
					Err("dns lookup: no address".to_owned())
				} else {
					Ok(addrs)
				}
			}
			Some(Ok(Err(error))) => Err(format!("dns lookup error: {}", error)),
			Some(Err(_)) => Err("dns lookup task failed".to_owned()),
			None => {
				tokio::spawn(async move {
					let _ = lookup_task.await;
					drop(permit);
				});
				Err("dns lookup timeout".to_owned())
			}
		}
	}

	/// singleflight: 進行中の解決があればsubscribeする(同期・MutexGuardがawaitをまたがない)。
	fn try_subscribe(&self, host: &str) -> Option<tokio::sync::broadcast::Receiver<DnsResult>> {
		let inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
		inflight.get(host).map(|tx| tx.subscribe())
	}
	/// singleflight: 自分が処理開始を登録する。既に別タスクが登録済みならそのrxを返す。
	fn register_or_subscribe(
		&self,
		host: &str,
	) -> Result<
		tokio::sync::broadcast::Sender<DnsResult>,
		tokio::sync::broadcast::Receiver<DnsResult>,
	> {
		let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
		if let Some(tx) = inflight.get(host) {
			Err(tx.subscribe())
		} else {
			let (tx, _) = tokio::sync::broadcast::channel(1);
			inflight.insert(host.to_owned(), tx.clone());
			Ok(tx)
		}
	}
	/// singleflight: 強制的にtxを登録する(raceで合流が全て失敗した場合のフォールバック)。
	fn force_register(&self, host: &str) -> tokio::sync::broadcast::Sender<DnsResult> {
		let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
		let (tx, _) = tokio::sync::broadcast::channel(1);
		inflight.insert(host.to_owned(), tx.clone());
		tx
	}
	fn remove_inflight_if_current(
		&self,
		host: &str,
		rx: &tokio::sync::broadcast::Receiver<DnsResult>,
	) {
		let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
		let is_current = inflight
			.get(host)
			.is_some_and(|tx| rx.same_channel(&tx.subscribe()));
		if is_current {
			inflight.remove(host);
		}
	}

	/// 解決失敗時にstaleエントリがあればそれを返す。なければエラー。
	async fn try_stale_or_err(
		&self,
		host: &str,
		err: String,
	) -> Result<(Vec<IpAddr>, DnsHitStatus), String> {
		let map = self.inner.read().await;
		if let Some(entry) = map.get(host) {
			if self.is_stale_usable(entry) {
				if let Ok(ips) = &entry.ips {
					return Ok((ips.clone(), DnsHitStatus::Stale));
				}
			}
		}
		Err(err)
	}
}
enum CheckUrlError {
	InvalidUrl(String),
	UnsupportedScheme(String),
	PolicyDenied(String),
	ResolveFailed(String),
}
impl CheckUrlError {
	fn as_header(&self) -> &'static str {
		match self {
			Self::InvalidUrl(_) => "InvalidUrl",
			Self::UnsupportedScheme(_) => "UnsupportedScheme",
			Self::PolicyDenied(_) => "PolicyDenied",
			Self::ResolveFailed(_) => "ResolveFailed",
		}
	}
	fn detail(&self) -> &str {
		match self {
			Self::InvalidUrl(detail)
			| Self::UnsupportedScheme(detail)
			| Self::PolicyDenied(detail)
			| Self::ResolveFailed(detail) => detail,
		}
	}
	fn is_resolve_failed(&self) -> bool {
		matches!(self, Self::ResolveFailed(_))
	}
}

async fn check_url(
	policy: &NetworkPolicy,
	dns_cache: &DnsCache,
	url: impl AsRef<str>,
) -> Result<(DnsHitStatus, u16, u16), CheckUrlError> {
	let u = reqwest::Url::from_str(url.as_ref()).map_err(|error| {
		tracing::warn!(url = ?url.as_ref(), %error, "URL validation failed");
		CheckUrlError::InvalidUrl(error.to_string())
	})?;
	match u.scheme().to_lowercase().as_str() {
		"http" | "https" => {}
		scheme => {
			tracing::warn!(url = ?url.as_ref(), scheme, "unsupported URL scheme");
			return Err(CheckUrlError::UnsupportedScheme(scheme.to_owned()));
		}
	}
	let host = u
		.host_str()
		.ok_or_else(|| CheckUrlError::InvalidUrl("no host".to_owned()))?;
	if policy.is_host_blocked(host) {
		return Err(CheckUrlError::PolicyDenied("Blocked address".to_owned()));
	}
	let host = NetworkPolicy::normalize_host(host);
	let port = u
		.port_or_known_default()
		.ok_or_else(|| CheckUrlError::InvalidUrl("no port".to_owned()))?;
	// 同期DNS(to_socket_addrs)を廃止し、非同期解決+独自タイムアウトに置き換え。
	let (ips, dns_cache_hit) = dns_cache
		.resolve(&host, port)
		.await
		.map_err(CheckUrlError::ResolveFailed)?;
	let mut v4_count: u16 = 0;
	let mut v6_count: u16 = 0;
	for ip in &ips {
		match ip {
			IpAddr::V4(_) => {
				v4_count += 1;
			}
			IpAddr::V6(_) => {
				v6_count += 1;
			}
		}
	}
	if ips.is_empty() {
		return Err(CheckUrlError::PolicyDenied("Blocked address".to_owned()));
	}
	for ip in ips {
		policy.check_ip(ip).map_err(CheckUrlError::PolicyDenied)?;
	}
	Ok((dns_cache_hit, v4_count, v6_count))
}
/// 1リクエストの各フェーズ所要時間と付随情報。Arc<Mutex<>> で get_file と
/// RequestContext(spawn_blocking 内も含む)で共有する。
#[derive(Default)]
struct PhaseTimings {
	dns_hit: Option<DnsHitStatus>,
	cache_result: Option<CacheResult>,
	passthrough: bool,
	check: Duration,
	/// permit待ちの合計。
	wait: Duration,
	/// ダウンロードセマフォの待機時間。
	dl_wait: Duration,
	/// CPUセマフォの待機時間。
	cpu_wait: Duration,
	/// バッファ予算セマフォの待機時間。
	buffer_wait: Duration,
	/// TTFB(送信開始〜レスポンスヘッダ受信)。
	ttfb: Duration,
	/// ボディ受信時間。
	body: Duration,
	decode: Duration,
	encode: Duration,
	anim: bool,
	anim_frames: u32,
	anim_in_bytes: usize,
	anim_out_bytes: usize,
	/// fetchエラーの分類+詳細(正常時はNone)。
	fetch_err: Option<String>,
	/// DNS解決結果のIPv4アドレス数。
	dns_v4: u16,
	/// DNS解決結果のIPv6アドレス数。
	dns_v6: u16,
	/// 接続リトライが実行された。
	retried: bool,
	/// 接続リトライ後の上流sendが成功した。
	fetch_retry_succeeded: bool,
	/// レスポンスのHTTPバージョン(例: "1.1", "2")。
	http_version: Option<&'static str>,
	/// 上流が返したHTTPステータス。上流応答前の失敗ではNone。
	upstream_status: Option<u16>,
}
/// 計測対象フェーズ(decode/encode は複数の return を持つ関数が多いため Drop で計測する)。
pub(crate) enum Phase {
	Decode,
	Encode,
}
/// スコープ離脱時に経過時間を該当フェーズへ加算するガード。
/// self を借用せず Arc を複製して保持するため、&mut self のメソッド内でも使える。
pub(crate) struct PhaseGuard {
	timings: Arc<Mutex<PhaseTimings>>,
	start: Instant,
	phase: Phase,
}
impl Drop for PhaseGuard {
	fn drop(&mut self) {
		if let Ok(mut t) = self.timings.lock() {
			let e = self.start.elapsed();
			match self.phase {
				Phase::Decode => t.decode += e,
				Phase::Encode => t.encode += e,
			}
		}
	}
}

fn domain_from_url(value: &str, fallback: &str) -> String {
	reqwest::Url::parse(value)
		.ok()
		.and_then(|url| url.host_str().map(str::to_owned))
		.map(|host| {
			let host = host.trim_end_matches('.').to_ascii_lowercase();
			let address = host.trim_start_matches('[').trim_end_matches(']');
			if address.parse::<IpAddr>().is_ok() {
				"ip".to_owned()
			} else {
				host
			}
		})
		.filter(|host| !host.is_empty())
		.unwrap_or_else(|| fallback.to_owned())
}

fn caller_domain(headers: &HeaderMap) -> String {
	let value = if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
		origin.to_str().ok()
	} else {
		headers
			.get(axum::http::header::REFERER)
			.and_then(|referer| referer.to_str().ok())
	};
	value
		.map(|value| domain_from_url(value, "unknown"))
		.unwrap_or_else(|| "unknown".to_owned())
}

/// アクセスログ用に、消費(move)される前の RequestParams から必要な値だけ控えておく。
struct ReqSummary {
	path: String,
	url: String,
	caller_domain: String,
	target_domain: String,
	final_target_domain: String,
	request_uri_hash: String,
	cache_key_hash: String,
	is_static_path: bool,
	is_static: bool,
	emoji: bool,
	avatar: bool,
	preview: bool,
	badge: bool,
	fallback: bool,
}
impl ReqSummary {
	fn new(
		q: &RequestParams,
		uri: &axum::http::Uri,
		headers: &HeaderMap,
		cache_key: &CacheKey,
	) -> Self {
		let path = uri.path().to_owned();
		let target_domain = domain_from_url(&q.url, "invalid");
		Self {
			is_static_path: path == "/static.webp",
			path,
			url: q.url.clone(),
			caller_domain: caller_domain(headers),
			final_target_domain: target_domain.clone(),
			target_domain,
			request_uri_hash: cache::fingerprint_bytes(uri.to_string().as_bytes()),
			cache_key_hash: cache_key.fingerprint(),
			is_static: q.r#static.is_some(),
			emoji: q.emoji.is_some(),
			avatar: q.avatar.is_some(),
			preview: q.preview.is_some(),
			badge: q.badge.is_some(),
			fallback: q.fallback.is_some(),
		}
	}
}
/// reqwest::Error を分類し、`"category:detail"` 形式の文字列を返す。
/// カテゴリ: connect / timeout / dns / reset / body / other
fn classify_reqwest_error(e: &reqwest::Error) -> String {
	// io::Error を source チェーンから探す
	let io_kind = {
		let mut source: Option<&(dyn std::error::Error + 'static)> = e.source();
		let mut found = None;
		while let Some(s) = source {
			if let Some(io) = s.downcast_ref::<std::io::Error>() {
				found = Some(io.kind());
				break;
			}
			source = s.source();
		}
		found
	};
	let detail = io_kind.map(|k| format!("{:?}", k)).unwrap_or_default();

	let category = if e.is_timeout() {
		"timeout"
	} else if e.is_connect() {
		match io_kind {
			Some(std::io::ErrorKind::ConnectionReset) => "reset",
			_ => "connect",
		}
	} else if e.is_body() {
		match io_kind {
			Some(std::io::ErrorKind::ConnectionReset) => "reset",
			_ => "body",
		}
	} else {
		// DNS解決失敗は reqwest では is_connect() に分類されることが多いが
		// source文字列で判別する
		let msg = format!("{}", e);
		if msg.contains("dns error") || msg.contains("resolve") || msg.contains("lookup") {
			"dns"
		} else {
			"other"
		}
	};

	if detail.is_empty() {
		category.to_owned()
	} else {
		format!("{}:{}", category, detail)
	}
}

/// 1リクエスト1行のサマリを出力する。正常かつ高速(slow_log_ms未満)なら DEBUG に落とす。
fn emit_summary(
	cfg: &ConfigFile,
	s: &ReqSummary,
	t: &PhaseTimings,
	status: u16,
	has_error: bool,
	error_detail: Option<&str>,
	stats: &GlobalStats,
) {
	if let Some(metrics) = &stats.otlp {
		metrics.record_phases(t, s.is_static_path);
		metrics.record_outcome(t, status, has_error, error_detail);
		metrics.record_domains(
			&s.caller_domain,
			&s.target_domain,
			status >= 400 || has_error,
		);
	}
	let is_error = status >= 400 || has_error;
	stats.requests.fetch_add(1, Ordering::Relaxed);
	stats.observe_request(t, s.is_static_path);
	if is_error {
		stats.errors.fetch_add(1, Ordering::Relaxed);
		if has_error {
			stats.inc_proxy_error(t, error_detail);
		}
	}
	match t.cache_result {
		Some(CacheResult::Hit | CacheResult::Joined) => {
			stats.cache_hits.fetch_add(1, Ordering::Relaxed);
		}
		Some(CacheResult::Miss) => {
			stats.cache_misses.fetch_add(1, Ordering::Relaxed);
		}
		Some(CacheResult::Evicted) => {
			stats.cache_misses.fetch_add(1, Ordering::Relaxed);
		}
		Some(CacheResult::Stale) => {
			stats.cache_stale_served.fetch_add(1, Ordering::Relaxed);
		}
		_ => {}
	}
	// fetch_err カウンタ
	if let Some(ref fe) = t.fetch_err {
		let cat = fe.split(':').next().unwrap_or("other");
		stats.inc_ferr(cat);
	}
	// retry_saved: リトライを実行し最終的にステータス200で応答できた数
	if t.retried && status == 200 {
		stats.retry_saved.fetch_add(1, Ordering::Relaxed);
	}
	let check_ms = t.check.as_millis() as u64;
	let wait_ms = t.wait.as_millis() as u64;
	let dl_wait_ms = t.dl_wait.as_millis() as u64;
	let cpu_wait_ms = t.cpu_wait.as_millis() as u64;
	let ttfb_ms = t.ttfb.as_millis() as u64;
	let body_ms = t.body.as_millis() as u64;
	let decode_ms = t.decode.as_millis() as u64;
	let encode_ms = t.encode.as_millis() as u64;
	let total_ms = check_ms + wait_ms + ttfb_ms + body_ms + decode_ms + encode_ms;
	let mut params = String::new();
	if s.is_static {
		params.push_str("static,");
	}
	if s.emoji {
		params.push_str("emoji,");
	}
	if s.avatar {
		params.push_str("avatar,");
	}
	if s.preview {
		params.push_str("preview,");
	}
	if s.badge {
		params.push_str("badge,");
	}
	if s.fallback {
		params.push_str("fallback,");
	}
	let dns_str = t
		.dns_hit
		.map(|d| d.to_string())
		.unwrap_or_else(|| "-".to_owned());
	let cache_str = t
		.cache_result
		.map(|c| c.to_string())
		.unwrap_or_else(|| "-".to_owned());
	let fetch_err_str = t.fetch_err.as_deref().unwrap_or("-");
	let http_str = t.http_version.unwrap_or("-");
	let fast = !is_error && !s.is_static_path && total_ms < cfg.slow_log_ms;
	if fast {
		tracing::debug!(
			path=%s.path,url=%s.url,caller_domain=%s.caller_domain,
			target_domain=%s.target_domain,final_target_domain=%s.final_target_domain,
			request_uri_hash=%s.request_uri_hash,
			cache_key_hash=%s.cache_key_hash,params=%params,dns_hit=%dns_str,cache=%cache_str,
			passthrough=t.passthrough,fetch_err=%fetch_err_str,retried=t.retried,
			http=%http_str,dns_v4=t.dns_v4,dns_v6=t.dns_v6,
			check_ms,wait_ms,dl_wait_ms,cpu_wait_ms,ttfb_ms,body_ms,decode_ms,encode_ms,
			status=status as u64,error=is_error,anim=t.anim,
			anim_frames=t.anim_frames as u64,
			anim_in=t.anim_in_bytes as u64,anim_out=t.anim_out_bytes as u64,
			"request"
		);
	} else {
		tracing::info!(
			path=%s.path,url=%s.url,caller_domain=%s.caller_domain,
			target_domain=%s.target_domain,final_target_domain=%s.final_target_domain,
			request_uri_hash=%s.request_uri_hash,
			cache_key_hash=%s.cache_key_hash,params=%params,dns_hit=%dns_str,cache=%cache_str,
			passthrough=t.passthrough,fetch_err=%fetch_err_str,retried=t.retried,
			http=%http_str,dns_v4=t.dns_v4,dns_v6=t.dns_v6,
			check_ms,wait_ms,dl_wait_ms,cpu_wait_ms,ttfb_ms,body_ms,decode_ms,encode_ms,
			status=status as u64,error=is_error,anim=t.anim,
			anim_frames=t.anim_frames as u64,
			anim_in=t.anim_in_bytes as u64,anim_out=t.anim_out_bytes as u64,
			"request"
		);
	}
}
/// staleキャッシュエントリからレスポンスを構築する。
fn build_stale_response(
	cached: &cache::CacheEntry,
	config: &ConfigFile,
	timings: &Arc<Mutex<PhaseTimings>>,
) -> axum::response::Response {
	if let Ok(mut t) = timings.lock() {
		t.cache_result = Some(CacheResult::Stale);
	}
	let mut headers = HeaderMap::new();
	if let Some(ct) = &cached.content_type {
		if let Ok(v) = ct.parse() {
			headers.append("Content-Type", v);
		}
	}
	if let Some(cd) = &cached.content_disposition {
		if let Ok(v) = cd.parse() {
			headers.append("Content-Disposition", v);
		}
	}
	headers.append("Cache-Control", "max-age=300".parse().unwrap());
	headers.append("X-Proxy-Stale", "1".parse().unwrap());
	headers.append("X-Content-Type-Options", "nosniff".parse().unwrap());
	headers.append(
		"Vary",
		if config.encode_avif {
			"Accept,Range".parse().unwrap()
		} else {
			"Range".parse().unwrap()
		},
	);
	for line in config.append_headers.iter() {
		if let Some(idx) = line.find(':') {
			if idx + 1 >= line.len() {
				continue;
			}
			if let Ok(k) = axum::http::HeaderName::from_str(&line[0..idx]) {
				if let Ok(v) = line[idx + 1..].parse() {
					headers.append(k, v);
				}
			}
		}
	}
	(axum::http::StatusCode::OK, headers, cached.body.clone()).into_response()
}

fn build_negative_response(
	status: u16,
	params: &RequestParams,
	config: &ConfigFile,
	dummy_img: &Arc<Vec<u8>>,
) -> axum::response::Response {
	let mut headers = HeaderMap::new();
	if let Ok(value) = params.url.parse() {
		headers.append("X-Remote-Url", value);
	}
	headers.append("X-Proxy-Error", format!("status:{status}").parse().unwrap());
	headers.append("Cache-Control", "no-store".parse().unwrap());
	headers.append("X-Content-Type-Options", "nosniff".parse().unwrap());
	headers.append(
		"Vary",
		if config.encode_avif {
			"Accept,Range".parse().unwrap()
		} else {
			"Range".parse().unwrap()
		},
	);
	for line in &config.append_headers {
		if let Some(idx) = line.find(':') {
			if idx + 1 < line.len() {
				if let Ok(name) = axum::http::HeaderName::from_str(&line[..idx]) {
					if let Ok(value) = line[idx + 1..].parse() {
						headers.append(name, value);
					}
				}
			}
		}
	}
	if params.fallback.is_some() {
		headers.append("Content-Type", "image/png".parse().unwrap());
		(axum::http::StatusCode::OK, headers, (**dummy_img).clone()).into_response()
	} else {
		let status =
			axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
		(status, headers).into_response()
	}
}
struct ActiveRequestGuard {
	metrics: Arc<OtlpMetrics>,
	started: Instant,
}
impl ActiveRequestGuard {
	fn new(metrics: Arc<OtlpMetrics>) -> Self {
		metrics.requests_active.add(1, &[]);
		Self {
			metrics,
			started: Instant::now(),
		}
	}
	fn record_completion(&self, status: u16, has_error: bool) {
		self.metrics
			.record_completion(status, has_error, self.started.elapsed());
	}
}
impl Drop for ActiveRequestGuard {
	fn drop(&mut self) {
		self.metrics.requests_active.add(-1, &[]);
	}
}

async fn get_file(
	path: Option<axum::extract::Path<String>>,
	original_uri: axum::extract::OriginalUri,
	client_headers: axum::http::HeaderMap,
	state: AppState,
	query: axum::extract::Query<RequestParams>,
) -> Result<(axum::http::StatusCode, HeaderMap, axum::body::Body), axum::response::Response> {
	let active_request = state.10.otlp.clone().map(ActiveRequestGuard::new);
	let result = get_file_inner(path, original_uri, client_headers, state, query).await;
	if let Some(active_request) = &active_request {
		let (status, has_error) = match &result {
			Ok((status, headers, _)) => (status.as_u16(), headers.contains_key("X-Proxy-Error")),
			Err(response) => (
				response.status().as_u16(),
				response.headers().contains_key("X-Proxy-Error"),
			),
		};
		active_request.record_completion(status, has_error);
	}
	result
}

async fn get_file_inner(
	_path: Option<axum::extract::Path<String>>,
	axum::extract::OriginalUri(original_uri): axum::extract::OriginalUri,
	client_headers: axum::http::HeaderMap,
	(
		client,
		config,
		dummy_img,
		fontdb,
		encode_semaphore,
		network_policy,
		dns_cache,
		response_cache,
		download_semaphore,
		buffer_budget,
		global_stats,
		host_throttle,
		negative_cache,
	): AppState,
	axum::extract::Query(q): axum::extract::Query<RequestParams>,
) -> Result<(axum::http::StatusCode, HeaderMap, axum::body::Body), axum::response::Response> {
	let request_started = Instant::now();
	let timings = Arc::new(Mutex::new(PhaseTimings::default()));

	// Range リクエストはキャッシュ対象外
	let has_range = client_headers.contains_key("Range");

	// avif 判定(キャッシュキー生成にも使う)
	let mut is_accept_avif = false;
	if config.encode_avif {
		if let Some(accept) = client_headers.get("Accept") {
			if let Ok(accept) = std::str::from_utf8(accept.as_bytes()) {
				for e in accept.split(",") {
					if e.trim() == "image/avif" {
						is_accept_avif = true;
					}
				}
			}
		}
	}

	let cache_key = CacheKey {
		url: q.url.clone(),
		is_static: q.r#static.is_some(),
		emoji: q.emoji.is_some(),
		avatar: q.avatar.is_some(),
		preview: q.preview.is_some(),
		badge: q.badge.is_some(),
		accept_avif: is_accept_avif,
	};
	let mut summary = ReqSummary::new(&q, &original_uri, &client_headers, &cache_key);

	// --- キャッシュヒット ---
	if !has_range {
		if let Some(cached) = response_cache.get(&cache_key) {
			if let Ok(mut t) = timings.lock() {
				t.cache_result = Some(CacheResult::Hit);
			}
			let mut headers = HeaderMap::new();
			headers.append("X-Content-Type-Options", "nosniff".parse().unwrap());
			if let Some(ct) = &cached.content_type {
				if let Ok(v) = ct.parse() {
					headers.append("Content-Type", v);
				}
			}
			if let Some(cd) = &cached.content_disposition {
				if let Ok(v) = cd.parse() {
					headers.append("Content-Disposition", v);
				}
			}
			if let Some(cc) = &cached.cache_control {
				if let Ok(v) = cc.parse() {
					headers.append("Cache-Control", v);
				}
			}
			headers.append(
				"Vary",
				if config.encode_avif {
					"Accept,Range".parse().unwrap()
				} else {
					"Range".parse().unwrap()
				},
			);
			for line in config.append_headers.iter() {
				if let Some(idx) = line.find(":") {
					if idx + 1 >= line.len() {
						continue;
					}
					if let Ok(k) = axum::http::HeaderName::from_str(&line[0..idx]) {
						if let Ok(v) = line[idx + 1..].parse() {
							headers.append(k, v);
						}
					}
				}
			}
			if let Ok(t) = timings.lock() {
				emit_summary(
					&config,
					&summary,
					&t,
					cached.status,
					false,
					None,
					&global_stats,
				);
			}
			let status = axum::http::StatusCode::from_u16(cached.status)
				.unwrap_or(axum::http::StatusCode::OK);
			return Err((status, headers, cached.body).into_response());
		}
	}

	// --- singleflight: 合流 ---
	if !has_range {
		if let Some(mut rx) = response_cache.try_join(&cache_key) {
			if let Ok(mut t) = timings.lock() {
				t.cache_result = Some(CacheResult::Joined);
			}
			match rx.recv().await {
				Ok(Some(cached)) => {
					let mut headers = HeaderMap::new();
					headers.append("X-Content-Type-Options", "nosniff".parse().unwrap());
					if let Some(ct) = &cached.content_type {
						if let Ok(v) = ct.parse() {
							headers.append("Content-Type", v);
						}
					}
					if let Some(cd) = &cached.content_disposition {
						if let Ok(v) = cd.parse() {
							headers.append("Content-Disposition", v);
						}
					}
					if let Some(cc) = &cached.cache_control {
						if let Ok(v) = cc.parse() {
							headers.append("Cache-Control", v);
						}
					}
					headers.append(
						"Vary",
						if config.encode_avif {
							"Accept,Range".parse().unwrap()
						} else {
							"Range".parse().unwrap()
						},
					);
					for line in config.append_headers.iter() {
						if let Some(idx) = line.find(":") {
							if idx + 1 >= line.len() {
								continue;
							}
							if let Ok(k) = axum::http::HeaderName::from_str(&line[0..idx]) {
								if let Ok(v) = line[idx + 1..].parse() {
									headers.append(k, v);
								}
							}
						}
					}
					if let Ok(t) = timings.lock() {
						emit_summary(
							&config,
							&summary,
							&t,
							cached.status,
							false,
							None,
							&global_stats,
						);
					}
					let status = axum::http::StatusCode::from_u16(cached.status)
						.unwrap_or(axum::http::StatusCode::OK);
					return Err((status, headers, cached.body).into_response());
				}
				_ => {
					// 元処理が失敗 → staleがあればそれを返す、なければフォールスルーして自分で処理
					if let Some(stale) = response_cache.get_stale(&cache_key) {
						let resp = build_stale_response(&stale, &config, &timings);
						if let Ok(t) = timings.lock() {
							emit_summary(&config, &summary, &t, 200, false, None, &global_stats);
						}
						return Err(resp);
					}
				}
			}
		}
	}

	// --- singleflight: 処理開始を登録 ---
	let mut flight_guard = if !has_range {
		response_cache.start_flight(cache_key.clone())
	} else {
		None
	};

	let was_capacity_evicted = !has_range && response_cache.was_capacity_evicted(&cache_key);
	if let Ok(mut t) = timings.lock() {
		if t.cache_result.is_none() {
			t.cache_result = Some(if has_range {
				CacheResult::Bypass
			} else {
				CacheResult::Miss
			});
		}
	}

	let mut headers = HeaderMap::new();
	if let Ok(url) = q.url.parse() {
		headers.append("X-Remote-Url", url);
	}
	headers.append(
		"Vary",
		if config.encode_avif {
			"Accept,Range".parse().unwrap()
		} else {
			"Range".parse().unwrap()
		},
	);
	let check_start = Instant::now();
	match check_url(&network_policy, &dns_cache, &q.url).await {
		Ok((hit, v4_count, v6_count)) => {
			if let Ok(mut t) = timings.lock() {
				t.check = check_start.elapsed();
				t.dns_hit = Some(hit);
				t.dns_v4 = v4_count;
				t.dns_v6 = v6_count;
			}
		}
		Err(error) => {
			if let Ok(mut t) = timings.lock() {
				t.check = check_start.elapsed();
				if error.is_resolve_failed() {
					t.fetch_err = Some(format!(
						"dns:{}",
						error.detail().chars().take(60).collect::<String>()
					));
				}
			}
			headers.append(
				"X-Proxy-Error",
				reqwest::header::HeaderValue::from_static(error.as_header()),
			);
			// stale-if-error: DNS失敗(ポリシー拒否以外)ならstaleを試みる
			if error.is_resolve_failed() && !has_range {
				if let Some(stale) = response_cache.get_stale(&cache_key) {
					let resp = build_stale_response(&stale, &config, &timings);
					if let Ok(t) = timings.lock() {
						emit_summary(&config, &summary, &t, 200, true, None, &global_stats);
					}
					return Err(resp);
				}
			}
			let is_fallback = q.fallback.is_some();
			if let Ok(t) = timings.lock() {
				emit_summary(
					&config,
					&summary,
					&t,
					if is_fallback { 200 } else { 400 },
					true,
					Some(error.detail()),
					&global_stats,
				);
			}
			if is_fallback {
				headers.append("Cache-Control", "no-store".parse().unwrap());
				headers.append("Content-Type", "image/png".parse().unwrap());
				return Err(
					(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
				);
			}
			headers.append("Cache-Control", "no-store".parse().unwrap());
			return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
		}
	};
	let negative_key = reqwest::Url::parse(&q.url)
		.map(|url| url.to_string())
		.unwrap_or_else(|_| q.url.clone());
	if !has_range {
		if let Some(entry) = negative_cache.get(&negative_key) {
			if let Ok(mut t) = timings.lock() {
				t.cache_result = Some(CacheResult::Negative);
			}
			let response = build_negative_response(entry.status, &q, &config, &dummy_img);
			if let Ok(t) = timings.lock() {
				emit_summary(
					&config,
					&summary,
					&t,
					response.status().as_u16(),
					true,
					Some(if entry.status == 404 {
						"status:404"
					} else {
						"status:410"
					}),
					&global_stats,
				);
			}
			return Err(response);
		}
	}

	// --- ダウンロードpermit取得(取得順序: DL permit → バイト予算 → CPU permit) ---
	let wait_start = Instant::now();
	let dl_permit = match tokio::time::timeout(
		RESOURCE_WAIT_TIMEOUT,
		download_semaphore.clone().acquire_owned(),
	)
	.await
	{
		Ok(Ok(permit)) => permit,
		_ => {
			if let Ok(mut t) = timings.lock() {
				let elapsed = wait_start.elapsed();
				t.wait += elapsed;
				t.dl_wait += elapsed;
			}
			let mut h = HeaderMap::new();
			h.append("X-Proxy-Error", "DownloadSemaphoreError".parse().unwrap());
			if let Ok(t) = timings.lock() {
				emit_summary(
					&config,
					&summary,
					&t,
					503,
					true,
					Some("DownloadSemaphoreError"),
					&global_stats,
				);
			}
			return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, h).into_response());
		}
	};
	global_stats.observe_dl_active(
		config.max_concurrent_downloads - download_semaphore.available_permits(),
	);
	if let Ok(mut t) = timings.lock() {
		let elapsed = wait_start.elapsed();
		t.wait += elapsed;
		t.dl_wait += elapsed;
	}
	let send_start = Instant::now();
	// Direct fetches are revalidated by ValidatingResolver at connection time.
	// With config.proxy, the proxy resolves the target, so only the URL pre-check
	// applies and DNS-rebinding TOCTOU remains possible.
	const MAX_REDIRECTS: u8 = 5;
	let mut current_url = match reqwest::Url::from_str(&q.url) {
		Ok(url) => url,
		Err(error) => {
			tracing::warn!(url = ?q.url, %error, "URL parsing failed after validation");
			headers.append("X-Proxy-Error", "InvalidUrl".parse().unwrap());
			return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
		}
	};
	let mut redirects = 0;
	let mut forward_range = has_range;
	let mut partial_image_response = None;
	let mut throttle_wait_total = Duration::ZERO;
	let resp = loop {
		let mut throttle_permit = if let Some(throttle) = &host_throttle {
			let result = throttle
				.acquire(&current_url, throttle_wait_budget(&config, request_started))
				.await;
			global_stats.observe_host_throttle(&result);
			match result {
				Ok(permit) => Some(permit),
				Err(rejection) => {
					if let Ok(mut t) = timings.lock() {
						t.wait += rejection.waited;
						t.fetch_err = Some("throttle:wait_timeout".to_owned());
					}
					headers.append("X-Proxy-Error", "HostThrottleTimeout".parse().unwrap());
					headers.append("Retry-After", response_retry_after(rejection.retry_after));
					if let Ok(t) = timings.lock() {
						emit_summary(
							&config,
							&summary,
							&t,
							503,
							true,
							Some("HostThrottleTimeout"),
							&global_stats,
						);
					}
					return Err(
						(axum::http::StatusCode::SERVICE_UNAVAILABLE, headers).into_response()
					);
				}
			}
		} else {
			None
		};
		let throttle_wait = throttle_permit
			.as_ref()
			.map(|permit| permit.waited)
			.unwrap_or_default();
		throttle_wait_total += throttle_wait;
		if let Ok(mut t) = timings.lock() {
			t.wait += throttle_wait;
		}
		let build_req = || {
			let req = client.get(current_url.as_str());
			let req = req.header("User-Agent", config.user_agent.clone());
			if forward_range {
				let range = client_headers.get("Range").expect("range header");
				req.header("Range", range.as_bytes())
			} else {
				req
			}
		};
		if was_capacity_evicted {
			if let Ok(mut t) = timings.lock() {
				t.cache_result = Some(CacheResult::Evicted);
			}
		}
		let resp = match build_req().send().await {
			Ok(resp) => {
				if let Ok(mut t) = timings.lock() {
					t.ttfb = send_start.elapsed().saturating_sub(throttle_wait_total);
				}
				resp
			}
			Err(e) if !forward_range && partial_image_response.is_some() => {
				tracing::warn!(url = %current_url, %e, "full image refetch failed; using original partial response");
				partial_image_response.take().unwrap()
			}
			Err(e) => {
				drop(throttle_permit.take());
				let first_err = classify_reqwest_error(&e);
				let is_connect_phase = e.is_connect() || e.is_timeout();
				// 接続段階の失敗かつRangeリクエスト以外かつ残り時間がある場合のみ1回リトライ
				let remaining_ms = config
					.timeout
					.saturating_sub(send_start.elapsed().as_millis() as u64);
				if is_connect_phase && !has_range && remaining_ms > config.fetch_retry_delay_ms {
					global_stats.retry_attempts.fetch_add(1, Ordering::Relaxed);
					if let Some(metrics) = &global_stats.otlp {
						metrics.fetch_retry_attempts.add(1, &[]);
					}
					tokio::time::sleep(Duration::from_millis(config.fetch_retry_delay_ms)).await;
					if let Some(throttle) = &host_throttle {
						let result = throttle
							.acquire(&current_url, throttle_wait_budget(&config, request_started))
							.await;
						global_stats.observe_host_throttle(&result);
						throttle_permit = match result {
							Ok(permit) => Some(permit),
							Err(rejection) => {
								if let Ok(mut t) = timings.lock() {
									t.wait += rejection.waited;
									t.fetch_err = Some("throttle:wait_timeout".to_owned());
									t.retried = true;
								}
								headers.append(
									"X-Proxy-Error",
									"HostThrottleTimeout".parse().unwrap(),
								);
								headers.append(
									"Retry-After",
									response_retry_after(rejection.retry_after),
								);
								if let Ok(t) = timings.lock() {
									emit_summary(
										&config,
										&summary,
										&t,
										503,
										true,
										Some("HostThrottleTimeout"),
										&global_stats,
									);
								}
								return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, headers)
									.into_response());
							}
						};
					}
					let retry_throttle_wait = throttle_permit
						.as_ref()
						.map(|permit| permit.waited)
						.unwrap_or_default();
					throttle_wait_total += retry_throttle_wait;
					if let Ok(mut t) = timings.lock() {
						t.wait += retry_throttle_wait;
					}
					match build_req().send().await {
						Ok(resp) => {
							if let Ok(mut t) = timings.lock() {
								t.ttfb = send_start.elapsed().saturating_sub(throttle_wait_total);
								t.retried = true;
								t.fetch_retry_succeeded = true;
							}
							resp
						}
						Err(e2) => {
							let fetch_err = classify_reqwest_error(&e2);
							let is_fallback = q.fallback.is_some();
							if let Ok(mut t) = timings.lock() {
								t.ttfb = send_start.elapsed();
								t.fetch_err = Some(fetch_err.clone());
								t.retried = true;
							}
							headers.append("X-Proxy-Error", "FetchFailed".parse().unwrap());
							// stale-if-error
							if !has_range {
								if let Some(stale) = response_cache.get_stale(&cache_key) {
									let resp = build_stale_response(&stale, &config, &timings);
									if let Ok(t) = timings.lock() {
										emit_summary(
											&config,
											&summary,
											&t,
											200,
											true,
											None,
											&global_stats,
										);
									}
									return Err(resp);
								}
							}
							if let Ok(t) = timings.lock() {
								emit_summary(
									&config,
									&summary,
									&t,
									if is_fallback { 200 } else { 400 },
									true,
									Some(&fetch_err),
									&global_stats,
								);
							}
							if is_fallback {
								headers.append("Cache-Control", "no-store".parse().unwrap());
								headers.append("Content-Type", "image/png".parse().unwrap());
								return Err((
									axum::http::StatusCode::OK,
									headers,
									(*dummy_img).clone(),
								)
									.into_response());
							}
							headers.append("Cache-Control", "no-store".parse().unwrap());
							return Err(
								(axum::http::StatusCode::BAD_REQUEST, headers).into_response()
							);
						}
					}
				} else {
					// リトライ不可(接続段階以外 or 残り時間不足 or Rangeリクエスト)
					let is_fallback = q.fallback.is_some();
					if let Ok(mut t) = timings.lock() {
						t.ttfb = send_start.elapsed();
						t.fetch_err = Some(first_err.clone());
					}
					headers.append("X-Proxy-Error", "FetchFailed".parse().unwrap());
					// stale-if-error
					if !has_range {
						if let Some(stale) = response_cache.get_stale(&cache_key) {
							let resp = build_stale_response(&stale, &config, &timings);
							if let Ok(t) = timings.lock() {
								emit_summary(&config, &summary, &t, 200, true, None, &global_stats);
							}
							return Err(resp);
						}
					}
					if let Ok(t) = timings.lock() {
						emit_summary(
							&config,
							&summary,
							&t,
							if is_fallback { 200 } else { 400 },
							true,
							Some(&first_err),
							&global_stats,
						);
					}
					if is_fallback {
						headers.append("Cache-Control", "no-store".parse().unwrap());
						headers.append("Content-Type", "image/png".parse().unwrap());
						return Err((axum::http::StatusCode::OK, headers, (*dummy_img).clone())
							.into_response());
					}
					headers.append("Cache-Control", "no-store".parse().unwrap());
					return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
				}
			}
		};
		if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
			log_upstream_429(&current_url, resp.headers());
		}
		if let Some(throttle) = &host_throttle {
			let was_throttled = throttle_permit
				.as_ref()
				.is_some_and(|permit| permit.throttled);
			throttle
				.observe_response(&current_url, resp.status(), resp.headers(), was_throttled)
				.await;
		}
		drop(throttle_permit.take());
		let is_image_response = resp
			.headers()
			.get(reqwest::header::CONTENT_TYPE)
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.split(';').next())
			.is_some_and(|media_type| media_type.trim().to_ascii_lowercase().starts_with("image/"));
		if forward_range
			&& resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
			&& is_image_response
		{
			forward_range = false;
			partial_image_response = Some(resp);
			continue;
		}
		if !resp.status().is_redirection() {
			break resp;
		}
		if redirects >= MAX_REDIRECTS {
			headers.append("X-Proxy-Error", "TooManyRedirects".parse().unwrap());
			if let Ok(t) = timings.lock() {
				emit_summary(
					&config,
					&summary,
					&t,
					502,
					true,
					Some("TooManyRedirects"),
					&global_stats,
				);
			}
			return Err((axum::http::StatusCode::BAD_GATEWAY, headers).into_response());
		}
		let Some(location) = resp
			.headers()
			.get(axum::http::header::LOCATION)
			.and_then(|value| value.to_str().ok())
		else {
			break resp;
		};
		let next_url = match current_url.join(location) {
			Ok(url) => url,
			Err(error) => {
				tracing::warn!(location, %error, "invalid redirect URL");
				if let Ok(t) = timings.lock() {
					emit_summary(
						&config,
						&summary,
						&t,
						if q.fallback.is_some() { 200 } else { 400 },
						true,
						Some("InvalidRedirect"),
						&global_stats,
					);
				}
				headers.append("X-Proxy-Error", "InvalidRedirect".parse().unwrap());
				headers.append("Cache-Control", "no-store".parse().unwrap());
				if q.fallback.is_some() {
					headers.append("Content-Type", "image/png".parse().unwrap());
					return Err(
						(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
					);
				}
				return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
			}
		};
		summary.final_target_domain = domain_from_url(next_url.as_str(), "invalid");
		const MAX_REDIRECT_DRAIN: usize = 64 * 1024;
		let mut stream = resp.bytes_stream();
		let mut drained = 0;
		while let Some(Ok(chunk)) = stream.next().await {
			drained += chunk.len();
			if drained >= MAX_REDIRECT_DRAIN {
				break;
			}
		}
		drop(stream);
		let check_start = Instant::now();
		match check_url(&network_policy, &dns_cache, next_url.as_str()).await {
			Ok((_hit, v4_count, v6_count)) => {
				if let Ok(mut t) = timings.lock() {
					t.check += check_start.elapsed();
					t.dns_v4 = t.dns_v4.saturating_add(v4_count);
					t.dns_v6 = t.dns_v6.saturating_add(v6_count);
				}
			}
			Err(error) => {
				if let Ok(mut t) = timings.lock() {
					t.check += check_start.elapsed();
					if error.is_resolve_failed() {
						t.fetch_err = Some(format!(
							"dns:{}",
							error.detail().chars().take(60).collect::<String>()
						));
					}
				}
				headers.append(
					"X-Proxy-Error",
					reqwest::header::HeaderValue::from_static(error.as_header()),
				);
				if let Ok(t) = timings.lock() {
					emit_summary(
						&config,
						&summary,
						&t,
						if q.fallback.is_some() { 200 } else { 400 },
						true,
						Some(error.detail()),
						&global_stats,
					);
				}
				headers.append("Cache-Control", "no-store".parse().unwrap());
				if q.fallback.is_some() {
					headers.append("Content-Type", "image/png".parse().unwrap());
					return Err(
						(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
					);
				}
				return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
			}
		}
		current_url = next_url;
		redirects += 1;
	};
	drop(partial_image_response);
	fn add_remote_header(
		key: &'static str,
		headers: &mut HeaderMap,
		remote_headers: &reqwest::header::HeaderMap,
	) {
		if key.eq_ignore_ascii_case("Content-Type") {
			if let Some(v) = remote_headers.get(key) {
				if let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
					headers.append(key, value);
				}
			}
			return;
		}
		for v in remote_headers.get_all(key) {
			if let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
				headers.append(key, value);
			}
		}
	}
	// HTTPバージョンを記録
	{
		let ver = match resp.version() {
			reqwest::Version::HTTP_2 => {
				global_stats.http2_responses.fetch_add(1, Ordering::Relaxed);
				"2"
			}
			reqwest::Version::HTTP_11 => {
				global_stats.http1_responses.fetch_add(1, Ordering::Relaxed);
				"1.1"
			}
			reqwest::Version::HTTP_10 => {
				global_stats.http1_responses.fetch_add(1, Ordering::Relaxed);
				"1.0"
			}
			_ => "?",
		};
		if let Ok(mut t) = timings.lock() {
			t.http_version = Some(ver);
		}
	}
	let remote_headers = resp.headers();
	if let Ok(mut t) = timings.lock() {
		t.upstream_status = Some(resp.status().as_u16());
	}
	if !has_range {
		negative_cache.put(negative_key, resp.status().as_u16());
	}
	add_remote_header("Content-Disposition", &mut headers, remote_headers);
	add_remote_header("Content-Type", &mut headers, remote_headers);
	let is_img = if let Some(media) = headers.get("Content-Type") {
		let s = String::from_utf8_lossy(media.as_bytes());
		s.starts_with("image/")
	} else {
		false
	};
	if !is_img {
		add_remote_header("Content-Length", &mut headers, remote_headers);
		add_remote_header("Content-Range", &mut headers, remote_headers);
		add_remote_header("Accept-Ranges", &mut headers, remote_headers);
	}
	headers.append("Cache-Control", "no-store".parse().unwrap());
	headers.append("X-Content-Type-Options", "nosniff".parse().unwrap());
	for line in config.append_headers.iter() {
		if let Some(idx) = line.find(":") {
			if idx + 1 >= line.len() {
				continue;
			}
			if let Ok(k) = axum::http::HeaderName::from_str(&line[0..idx]) {
				if let Ok(v) = line[idx + 1..].parse() {
					headers.append(k, v);
				}
			}
		}
	}
	let result = RequestContext {
		is_accept_avif,
		headers,
		parms: q,
		src_bytes: Vec::new(),
		config: config.clone(),
		codec: Err(None),
		dummy_img,
		fontdb,
		encode_semaphore,
		buffer_budget,
		dl_permit: Some(dl_permit),
		timings: timings.clone(),
		response_cache: response_cache.clone(),
		cache_key: cache_key.clone(),
		is_static_path: summary.is_static_path,
		global_stats: global_stats.clone(),
	}
	.encode(resp, is_img)
	.await;

	// --- singleflight 完了通知 ---
	if let Some(ref mut guard) = flight_guard {
		// encode 結果が 200 ならキャッシュから取得してflight完了
		let entry = response_cache.get(&cache_key);
		response_cache.complete_flight(guard, entry);
	}

	// --- stale-if-error: フェッチ失敗時にstaleエントリで救済 ---
	let result = if result.is_err() {
		let has_fetch_err = timings
			.lock()
			.ok()
			.map(|t| t.fetch_err.is_some())
			.unwrap_or(false);
		if has_fetch_err {
			if let Some(stale) = response_cache.get_stale(&cache_key) {
				Err(build_stale_response(&stale, &config, &timings))
			} else {
				result
			}
		} else {
			result
		}
	} else {
		result
	};

	let (status, error_detail) = match &result {
		Ok((sc, h, _)) => (
			sc.as_u16(),
			h.get("X-Proxy-Error").and_then(|v| v.to_str().ok()),
		),
		Err(r) => (
			r.status().as_u16(),
			r.headers()
				.get("X-Proxy-Error")
				.and_then(|v| v.to_str().ok()),
		),
	};
	if let Ok(t) = timings.lock() {
		emit_summary(
			&config,
			&summary,
			&t,
			status,
			error_detail.is_some(),
			error_detail,
			&global_stats,
		);
	}
	result
}
struct RequestContext {
	is_accept_avif: bool,
	headers: HeaderMap,
	parms: RequestParams,
	src_bytes: Vec<u8>,
	config: Arc<ConfigFile>,
	codec: Result<image::ImageFormat, Option<image::ImageError>>,
	dummy_img: Arc<Vec<u8>>,
	fontdb: Arc<resvg::usvg::fontdb::Database>,
	encode_semaphore: Arc<Semaphore>,
	buffer_budget: Arc<Semaphore>,
	dl_permit: Option<tokio::sync::OwnedSemaphorePermit>,
	timings: Arc<Mutex<PhaseTimings>>,
	response_cache: Arc<ResponseCache>,
	cache_key: CacheKey,
	is_static_path: bool,
	global_stats: Arc<GlobalStats>,
}
impl RequestContext {
	/// フェーズ計測ガードを生成する(Arcを複製して保持するため self を借用し続けない)。
	pub(crate) fn phase_guard(&self, phase: Phase) -> PhaseGuard {
		PhaseGuard {
			timings: self.timings.clone(),
			start: Instant::now(),
			phase,
		}
	}
	/// ボディ受信完了時の計測を記録する。
	pub(crate) fn mark_body_done(&self, body_duration: Duration) {
		if let Ok(mut t) = self.timings.lock() {
			t.body = body_duration;
		}
	}
	/// encode_anim のフレーム数・入出力バイト数を記録する。
	pub(crate) fn record_anim(&self, frames: u32, in_bytes: usize, out_bytes: usize) {
		if let Ok(mut t) = self.timings.lock() {
			t.anim = true;
			t.anim_frames = frames;
			t.anim_in_bytes = in_bytes;
			t.anim_out_bytes = out_bytes;
		}
	}
	/// 成功レスポンスをキャッシュに格納する。
	pub(crate) fn cache_response(&self, status: u16, headers: &HeaderMap, body: &[u8]) {
		let ct = headers
			.get("Content-Type")
			.and_then(|v| v.to_str().ok())
			.map(|s| s.to_owned());
		let cd = headers
			.get("Content-Disposition")
			.and_then(|v| v.to_str().ok())
			.map(|s| s.to_owned());
		let cc = headers
			.get("Cache-Control")
			.and_then(|v| v.to_str().ok())
			.map(|s| s.to_owned());
		self.global_stats
			.record_processed_output(ct.as_deref(), self.src_bytes.len(), body.len());
		let entry = cache::CacheEntry::new(status, ct, cd, cc, body.to_vec());
		if self.response_cache.put(self.cache_key.clone(), entry) && self.is_static_path {
			self.global_stats.record_static_insertion(body.len());
		}
	}
}
impl RequestContext {
	pub fn disposition_ext(headers: &mut HeaderMap, ext: &str) {
		let k = "Content-Disposition";
		if let Some(cd) = headers.get(k) {
			let s = std::str::from_utf8(cd.as_bytes());
			if let Ok(s) = s {
				let cd = mailparse::parse_content_disposition(s);
				let cd_utf8 = cd.params.get("filename*");
				let mut name = None;
				if let Some(cd_utf8) = cd_utf8 {
					if cd_utf8.len() > 7 && cd_utf8.as_bytes()[..7].eq_ignore_ascii_case(b"UTF-8''")
					{
						name = urlencoding::decode(&cd_utf8[7..])
							.map(|s| s.to_string())
							.ok();
					}
				}
				if name.is_none() {
					if let Some(filename) = cd.params.get("filename") {
						let m_filename = format!("_:{}", filename);
						let parsed = mailparse::parse_header(m_filename.as_bytes());
						if let Ok((parsed, _)) = &parsed {
							name = Some(parsed.get_value());
						} else if !cd.params.contains_key("name") {
							name = Some(filename.clone());
						}
					}
				}
				let name = name.unwrap_or_else(|| {
					cd.params
						.get("name")
						.cloned()
						.unwrap_or_else(|| "null".to_owned())
				});
				let mut name_arr: Vec<&str> = name.split('.').collect();
				name_arr.pop();
				let name = name_arr.join(".") + ext;
				let name = urlencoding::encode(&name);
				let content_disposition =
					format!("inline; filename=\"{}\";filename*=UTF-8''{};", name, name);
				headers.remove(k);
				if let Ok(value) = content_disposition.parse() {
					headers.append(k, value);
				}
			}
		}
	}
}
impl RequestContext {
	async fn encode(
		mut self,
		resp: reqwest::Response,
		mut is_img: bool,
	) -> Result<(axum::http::StatusCode, HeaderMap, axum::body::Body), axum::response::Response> {
		let mut is_svg = false;
		let mut content_type = None;
		if let Some(media) = self.headers.get("Content-Type") {
			let s = String::from_utf8_lossy(media.as_bytes());
			if s.split(';')
				.next()
				.is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("image/svg+xml"))
			{
				is_svg = true;
			} else {
				content_type = Some(s);
			}
		}
		let status = resp.status();
		if !status.is_success() {
			return Err(self.remote_error_response(status));
		}
		let resp = PreDataStream::new(resp).await;
		if let Some(Ok(head)) = resp.head.as_ref() {
			//utf8にパースできて空白文字を削除した後の先頭部分が<svgの場合はsvg
			if std::str::from_utf8(head)
				.map(|s| s.trim().starts_with("<svg"))
				.unwrap_or(false)
			{
				is_svg = true;
			} else {
				self.codec = image::guess_format(head).map_err(Some);
				if self.codec.is_err() {
					if let Some(content_type) = content_type.as_ref() {
						match content_type.as_ref() {
							"image/x-targa" | "image/x-tga" => {
								self.codec = Ok(image::ImageFormat::Tga)
							}
							_ => {}
						}
					}
					if head.starts_with(&[0xFF, 0x0A])
						|| head.starts_with(&[
							0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
						]) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/jxl".parse().unwrap());
					}
					if head.starts_with(&[0xFF, 0x4F, 0xFF, 0x51])
						|| head.starts_with(&[
							0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
						]) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/jp2".parse().unwrap());
					}
					if head.starts_with(&[0x49, 0x49, 0xBC]) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/jxr".parse().unwrap());
					}
					if head.starts_with(b"%PDF") {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "application/pdf".parse().unwrap());
					}
					if libvips_dep::is_vips(head) {
						is_img = true;
					}
					if crate::mng::is_mng_or_jng(head) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/x-mng".parse().unwrap());
					}
					#[cfg(feature = "avif-decoder")]
					if crate::avif_seq::is_avif_sequence(head) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/avif".parse().unwrap());
					}
				}
			}
		}
		if is_svg {
			// バイト予算を取得(仮予約8MB)。Content-Length不明のため固定値。
			let budget_bytes = 8 * 1024 * 1024_u32;
			let budget_sem = self.buffer_budget.clone();
			let wait_start = Instant::now();
			let _budget_permit = match tokio::time::timeout(
				RESOURCE_WAIT_TIMEOUT,
				budget_sem.acquire_many(budget_bytes),
			)
			.await
			{
				Ok(Ok(permit)) => permit,
				_ => {
					if let Ok(mut t) = self.timings.lock() {
						let elapsed = wait_start.elapsed();
						t.wait += elapsed;
						t.buffer_wait += elapsed;
					}
					let mut h = self.headers.clone();
					h.append("X-Proxy-Error", "BufferBudgetError".parse().unwrap());
					return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, h).into_response());
				}
			};
			self.global_stats.observe_buf_used(
				self.config.inflight_buffer_budget_bytes as usize - budget_sem.available_permits(),
			);
			if let Ok(mut t) = self.timings.lock() {
				let elapsed = wait_start.elapsed();
				t.wait += elapsed;
				t.buffer_wait += elapsed;
			}
			self.load_all(resp).await?;
			drop(self.dl_permit.take()); // ダウンロード完了 → DL permit 解放
								// CPU permit を取得してエンコード
			let cpu_sem = self.encode_semaphore.clone();
			let wait_start = Instant::now();
			let _cpu_permit = match tokio::time::timeout(RESOURCE_WAIT_TIMEOUT, cpu_sem.acquire())
				.await
			{
				Ok(Ok(permit)) => permit,
				_ => {
					if let Ok(mut t) = self.timings.lock() {
						let elapsed = wait_start.elapsed();
						t.wait += elapsed;
						t.cpu_wait += elapsed;
					}
					let mut h = self.headers.clone();
					h.append("X-Proxy-Error", "CpuSemaphoreError".parse().unwrap());
					return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, h).into_response());
				}
			};
			self.global_stats
				.observe_cpu_active(num_cpus::get().max(2) - cpu_sem.available_permits());
			if let Ok(mut t) = self.timings.lock() {
				let elapsed = wait_start.elapsed();
				t.wait += elapsed;
				t.cpu_wait += elapsed;
			}
			let _decode_guard = self.phase_guard(Phase::Decode);
			let src_bytes = std::mem::take(&mut self.src_bytes);
			let fontdb = self.fontdb.clone();
			let size_hint = self.image_size_hint();
			let max_decode_pixels = (self.config.max_size / 4).max(1);
			let timeout_ms = self.config.timeout;
			if let Ok(img) = crate::svg::render_svg_blocking(
				src_bytes,
				fontdb,
				size_hint,
				max_decode_pixels,
				timeout_ms,
			)
			.await
			{
				self.headers.remove("Content-Length");
				self.headers.remove("Content-Range");
				self.headers.remove("Accept-Ranges");
				self.headers.remove("Cache-Control");
				self.headers.append(
					"Cache-Control",
					"max-age=31536000, immutable".parse().unwrap(),
				);
				return Err(self.response_img(img));
			} else {
				self.headers.remove("Content-Type");
				self.headers.remove("Content-Length");
				self.headers.remove("Content-Range");
				self.headers.remove("Accept-Ranges");
				if self.parms.fallback.is_some() {
					self.headers
						.append("Content-Type", "image/png".parse().unwrap());
					return Err((
						axum::http::StatusCode::OK,
						self.headers.clone(),
						(*self.dummy_img).clone(),
					)
						.into_response());
				}
				self.headers
					.append("X-Proxy-Error", "SvgEncodeError".parse().unwrap());
				return Err(
					(axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response()
				);
			}
		} else if is_img || self.codec.is_ok() {
			self.headers.remove("Content-Length");
			self.headers.remove("Content-Range");
			self.headers.remove("Accept-Ranges");
			let dummy_img = self.dummy_img.clone();
			let is_fallback = self.parms.fallback.is_some();
			let mut header = self.headers.clone();
			// バイト予算を取得(Content-Length or 仮予約8MB)
			let budget_hint = resp.content_length.unwrap_or(8 * 1024 * 1024);
			let budget_bytes = (budget_hint.min(self.config.max_size) as u32).max(1);
			let budget_sem = self.buffer_budget.clone();
			let wait_start = Instant::now();
			let _budget_permit = match tokio::time::timeout(
				RESOURCE_WAIT_TIMEOUT,
				budget_sem.acquire_many(budget_bytes),
			)
			.await
			{
				Ok(Ok(permit)) => permit,
				_ => {
					if let Ok(mut t) = self.timings.lock() {
						let elapsed = wait_start.elapsed();
						t.wait += elapsed;
						t.buffer_wait += elapsed;
					}
					header.append("X-Proxy-Error", "BufferBudgetError".parse().unwrap());
					return Err(
						(axum::http::StatusCode::SERVICE_UNAVAILABLE, header.clone())
							.into_response(),
					);
				}
			};
			self.global_stats.observe_buf_used(
				self.config.inflight_buffer_budget_bytes as usize - budget_sem.available_permits(),
			);
			if let Ok(mut t) = self.timings.lock() {
				let elapsed = wait_start.elapsed();
				t.wait += elapsed;
				t.buffer_wait += elapsed;
			}
			self.load_all(resp).await?;
			drop(self.dl_permit.take()); // ダウンロード完了 → DL permit 解放
								// --- パススルー判定 ---
								// webp/png/jpeg/gif かつ badge/static 無 かつ寸法が目標以下かつサイズが閾値以下なら
								// デコード・再エンコードせず元バイト列をそのまま返す。
			if self.parms.badge.is_none() && self.parms.r#static.is_none() {
				if let Ok(codec) = &self.codec {
					let is_passthrough_format = matches!(
						codec,
						image::ImageFormat::WebP
							| image::ImageFormat::Png
							| image::ImageFormat::Jpeg
							| image::ImageFormat::Gif
					);
					if is_passthrough_format
						&& self.src_bytes.len() <= self.config.passthrough_max_bytes as usize
					{
						// ヘッダ読みで寸法を取得(全デコードしない)
						let reader = image::ImageReader::new(std::io::Cursor::new(&self.src_bytes))
							.with_guessed_format();
						let dims = reader.ok().and_then(|r| r.into_dimensions().ok());
						if let Some((w, h)) = dims {
							let (max_w, max_h) = self.image_size_hint();
							if w <= max_w && h <= max_h {
								// パススルー: 元バイト列をそのまま返す
								if let Ok(mut t) = self.timings.lock() {
									t.passthrough = true;
								}
								self.headers.remove("Cache-Control");
								self.headers.append(
									"Cache-Control",
									"max-age=31536000, immutable".parse().unwrap(),
								);
								// Content-Disposition の拡張子を元フォーマットに合わせる
								let ext = match codec {
									image::ImageFormat::WebP => ".webp",
									image::ImageFormat::Png => ".png",
									image::ImageFormat::Jpeg => ".jpeg",
									image::ImageFormat::Gif => ".gif",
									_ => ".bin",
								};
								Self::disposition_ext(&mut self.headers, ext);
								self.cache_response(200, &self.headers, &self.src_bytes);
								let body = std::mem::take(&mut self.src_bytes);
								return Err((
									axum::http::StatusCode::OK,
									self.headers.clone(),
									body,
								)
									.into_response());
							}
						}
					}
				}
			}
			// CPU permit を取得してからエンコード
			let cpu_sem = self.encode_semaphore.clone();
			let wait_start = Instant::now();
			let cpu_permit =
				match tokio::time::timeout(RESOURCE_WAIT_TIMEOUT, cpu_sem.clone().acquire_owned())
					.await
				{
					Ok(Ok(permit)) => permit,
					_ => {
						if let Ok(mut t) = self.timings.lock() {
							let elapsed = wait_start.elapsed();
							t.wait += elapsed;
							t.cpu_wait += elapsed;
						}
						header.append("X-Proxy-Error", "CpuSemaphoreError".parse().unwrap());
						return Err(
							(axum::http::StatusCode::SERVICE_UNAVAILABLE, header.clone())
								.into_response(),
						);
					}
				};
			self.global_stats
				.observe_cpu_active(num_cpus::get().max(2) - cpu_sem.available_permits());
			if let Ok(mut t) = self.timings.lock() {
				let elapsed = wait_start.elapsed();
				t.wait += elapsed;
				t.cpu_wait += elapsed;
			}
			let timeout_ms = self.config.timeout;
			let mut handle = self;
			let task = tokio::runtime::Handle::current().spawn_blocking(move || {
				let _cpu_permit = cpu_permit;
				handle.encode_img()
			});
			let abort_handle = task.abort_handle();
			let resp = match tokio::time::timeout(
				std::time::Duration::from_millis(timeout_ms.max(1)),
				task,
			)
			.await
			{
				Ok(Ok(resp)) => resp,
				Ok(Err(_)) => {
					header.append("X-Proxy-Error", "ImageEncodeThread".parse().unwrap());
					return Err(if is_fallback {
						header.remove("Content-Type");
						header.append("Content-Type", "image/png".parse().unwrap());
						(axum::http::StatusCode::OK, header, (*dummy_img).clone()).into_response()
					} else {
						(axum::http::StatusCode::INTERNAL_SERVER_ERROR, header).into_response()
					});
				}
				Err(_) => {
					abort_handle.abort();
					header.append("X-Proxy-Error", "ImageEncodeTimeout".parse().unwrap());
					return Err(if is_fallback {
						header.remove("Content-Type");
						header.append("Content-Type", "image/png".parse().unwrap());
						(axum::http::StatusCode::OK, header, (*dummy_img).clone()).into_response()
					} else {
						(axum::http::StatusCode::GATEWAY_TIMEOUT, header).into_response()
					});
				}
			};
			if is_fallback {
				return Err(if resp.status() == axum::http::StatusCode::OK {
					resp
				} else {
					let mut fallback_headers = resp.headers().clone();
					fallback_headers.remove("Content-Type");
					fallback_headers.remove("Content-Length");
					fallback_headers.append("Content-Type", "image/png".parse().unwrap());
					(
						axum::http::StatusCode::OK,
						fallback_headers,
						(*dummy_img).clone(),
					)
						.into_response()
				});
			}
			return Err(resp);
		}
		let is_browsersafe = self.headers.get("Content-Type").is_some_and(|media| {
			let content_type = String::from_utf8_lossy(media.as_bytes());
			crate::browsersafe::FILE_TYPE_BROWSERSAFE.contains(&content_type.as_ref())
		});
		if !is_browsersafe {
			self.headers.remove("Content-Type");
			self.headers.remove("Content-Length");
			self.headers.remove("Content-Range");
			self.headers.remove("Accept-Ranges");
			self.headers
				.append("Content-Type", "image/png".parse().unwrap());
			self.headers
				.append("X-Proxy-Error", "NonBrowsersafeType".parse().unwrap());
			return Err((
				axum::http::StatusCode::OK,
				self.headers.clone(),
				(*self.dummy_img).clone(),
			)
				.into_response());
		}
		let body = axum::body::Body::from_stream(resp);
		// ストリーミングパス: ボディ計測は行わない(パススルー)
		self.headers.remove("Cache-Control");
		self.headers.append(
			"Cache-Control",
			"max-age=31536000, immutable".parse().unwrap(),
		);
		if status == reqwest::StatusCode::PARTIAL_CONTENT {
			Ok((
				axum::http::StatusCode::PARTIAL_CONTENT,
				self.headers.clone(),
				body,
			))
		} else {
			Ok((axum::http::StatusCode::OK, self.headers.clone(), body))
		}
	}
	/// リモートからのエラーはエラーとして返す
	/// Content-Length/Content-Range を残すと空ボディと矛盾して壊れるから消す
	fn remote_error_response(mut self, status: reqwest::StatusCode) -> axum::response::Response {
		self.headers.remove("Content-Length");
		self.headers.remove("Content-Range");
		self.headers.remove("Accept-Ranges");
		self.headers.append(
			"X-Proxy-Error",
			format!("status:{}", status.as_u16()).parse().unwrap(),
		);
		if self.parms.fallback.is_some() {
			self.headers.remove("Content-Type");
			self.headers
				.append("Content-Type", "image/png".parse().unwrap());
			(
				axum::http::StatusCode::OK,
				self.headers.clone(),
				(*self.dummy_img).clone(),
			)
				.into_response()
		} else {
			let status = match status {
				reqwest::StatusCode::BAD_REQUEST => axum::http::StatusCode::BAD_REQUEST,
				reqwest::StatusCode::FORBIDDEN => axum::http::StatusCode::FORBIDDEN,
				reqwest::StatusCode::NOT_FOUND => axum::http::StatusCode::NOT_FOUND,
				reqwest::StatusCode::REQUEST_TIMEOUT => axum::http::StatusCode::GATEWAY_TIMEOUT,
				reqwest::StatusCode::GONE => axum::http::StatusCode::GONE,
				reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS => {
					axum::http::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
				}
				_ => axum::http::StatusCode::BAD_GATEWAY,
			};
			(status, self.headers.clone()).into_response()
		}
	}
	async fn load_all(&mut self, mut resp: PreDataStream) -> Result<(), axum::response::Response> {
		let len_hint = resp
			.content_length
			.unwrap_or(2048.min(self.config.max_size));
		if len_hint > self.config.max_size {
			self.headers
				.append("X-Proxy-Error", "ResponseTooLarge".parse().unwrap());
			return Err((axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response());
		}
		// Never trust the remote Content-Length hint for pre-allocation
		// (finding #5): cap the initial reservation and let the buffer grow
		// as bytes actually arrive (still bounded by max_size below).
		const INITIAL_CAP: u64 = 16 * 1024;
		let mut response_bytes = Vec::with_capacity(len_hint.min(INITIAL_CAP) as usize);
		let body_start = Instant::now();
		while let Some(x) = resp.next().await {
			match x {
				Ok(b) => {
					if response_bytes.len() + b.len() > self.config.max_size as usize {
						self.headers
							.append("X-Proxy-Error", "ResponseTooLarge".parse().unwrap());
						return Err((axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
							.into_response());
					}
					response_bytes.extend_from_slice(&b);
				}
				Err(e) => {
					let fetch_err = classify_reqwest_error(&e);
					if let Ok(mut t) = self.timings.lock() {
						t.fetch_err = Some(fetch_err.clone());
					}
					self.headers
						.append("X-Proxy-Error", "BodyReadFailed".parse().unwrap());
					return Err(
						(axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response()
					);
				}
			}
		}
		self.src_bytes = response_bytes;
		self.mark_body_done(body_start.elapsed());
		Ok(())
	}
}
struct PreDataStream {
	content_length: Option<u64>,
	head: Option<Result<axum::body::Bytes, reqwest::Error>>,
	last: Pin<
		Box<
			dyn futures::stream::Stream<Item = Result<axum::body::Bytes, reqwest::Error>>
				+ Send
				+ Sync,
		>,
	>,
}
impl PreDataStream {
	async fn new(value: reqwest::Response) -> Self {
		let content_length = value.content_length();
		let mut stream = value.bytes_stream();
		let head = stream.next().await;
		Self {
			content_length,
			head,
			last: Box::pin(stream),
		}
	}
}
impl futures::stream::Stream for PreDataStream {
	type Item = Result<axum::body::Bytes, reqwest::Error>;

	fn poll_next(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Option<Self::Item>> {
		let mut r = self.as_mut();
		if let Some(d) = r.head.take() {
			return std::task::Poll::Ready(Some(d));
		}
		r.last.as_mut().poll_next(cx)
	}
}

#[cfg(test)]
mod metrics_tests {
	use super::*;
	use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
	use opentelemetry_sdk::metrics::InMemoryMetricExporter;

	#[test]
	fn structured_log_domains_prefer_origin_and_avoid_ip_values() {
		let mut headers = HeaderMap::new();
		headers.insert(
			axum::http::header::REFERER,
			"https://referer.example/path?secret=1".parse().unwrap(),
		);
		assert_eq!(caller_domain(&headers), "referer.example");

		headers.insert(
			axum::http::header::ORIGIN,
			"https://Origin.Example:8443".parse().unwrap(),
		);
		assert_eq!(caller_domain(&headers), "origin.example");
		assert_eq!(
			domain_from_url("https://192.0.2.1/image.png", "invalid"),
			"ip"
		);
		assert_eq!(
			domain_from_url("https://[2001:db8::1]/image.png", "invalid"),
			"ip"
		);
		assert_eq!(domain_from_url("not a url", "invalid"), "invalid");
		assert_eq!(caller_domain(&HeaderMap::new()), "unknown");
	}

	#[test]
	fn request_metrics_classify_upstream_status_and_keep_interval_maxima() {
		let stats = GlobalStats::new();
		for (status, wait_ms) in [
			(200, 3),
			(302, 4),
			(403, 6),
			(404, 8),
			(410, 5),
			(429, 13),
			(503, 2),
		] {
			let timings = PhaseTimings {
				upstream_status: Some(status),
				dl_wait: Duration::from_millis(wait_ms),
				cpu_wait: Duration::from_millis(wait_ms / 2),
				..Default::default()
			};
			stats.observe_request(&timings, false);
		}
		assert_eq!(stats.upstream_2xx.load(Ordering::Relaxed), 1);
		assert_eq!(stats.upstream_3xx.load(Ordering::Relaxed), 1);
		assert_eq!(stats.upstream_other_4xx.load(Ordering::Relaxed), 1);
		assert_eq!(stats.upstream_404.load(Ordering::Relaxed), 1);
		assert_eq!(stats.upstream_410.load(Ordering::Relaxed), 1);
		assert_eq!(stats.upstream_429.load(Ordering::Relaxed), 1);
		assert_eq!(stats.upstream_5xx.load(Ordering::Relaxed), 1);
		assert_eq!(stats.dl_wait_ms.load(Ordering::Relaxed), 41);
		assert_eq!(stats.dl_wait_max_ms.load(Ordering::Relaxed), 13);
		assert_eq!(stats.cpu_wait_ms.load(Ordering::Relaxed), 19);
		assert_eq!(stats.cpu_wait_max_ms.load(Ordering::Relaxed), 6);
		stats.observe_dl_active(7);
		stats.observe_dl_active(3);
		stats.observe_cpu_active(2);
		stats.observe_buf_used(9 * 1024 * 1024);
		assert_eq!(stats.dl_active_max.load(Ordering::Relaxed), 7);
		assert_eq!(stats.cpu_active_max.load(Ordering::Relaxed), 2);
		assert_eq!(
			stats.buf_used_max_bytes.load(Ordering::Relaxed),
			9 * 1024 * 1024
		);
		stats.inc_proxy_error(&PhaseTimings::default(), Some("status:404"));
		stats.inc_proxy_error(&PhaseTimings::default(), Some("DecodeError_invalid"));
		stats.inc_proxy_error(&PhaseTimings::default(), Some("EncodeError_failed"));
		stats.inc_proxy_error(&PhaseTimings::default(), Some("length:20>10"));
		stats.inc_proxy_error(&PhaseTimings::default(), Some("Blocked address"));
		assert_eq!(stats.perr_upstream_status.load(Ordering::Relaxed), 1);
		assert_eq!(stats.decode_error.load(Ordering::Relaxed), 1);
		assert_eq!(stats.encode_error.load(Ordering::Relaxed), 1);
		assert_eq!(stats.size_reject.load(Ordering::Relaxed), 1);
		assert_eq!(stats.policy_reject.load(Ordering::Relaxed), 1);
		let fetch_error = PhaseTimings {
			fetch_err: Some("timeout".to_owned()),
			..Default::default()
		};
		stats.inc_proxy_error(&fetch_error, Some("Send:timeout"));
		assert_eq!(stats.internal_error.load(Ordering::Relaxed), 0);
		stats.record_processed_output(Some("image/webp"), 1000, 400);
		stats.record_processed_output(Some("image/jpeg; charset=binary"), 800, 500);
		assert_eq!(stats.output_webp.load(Ordering::Relaxed), 1);
		assert_eq!(stats.output_jpeg.load(Ordering::Relaxed), 1);
		assert_eq!(stats.processed_input_bytes.load(Ordering::Relaxed), 1800);
		assert_eq!(stats.processed_output_bytes.load(Ordering::Relaxed), 900);
		for result in [
			CacheResult::Hit,
			CacheResult::Miss,
			CacheResult::Joined,
			CacheResult::Evicted,
		] {
			stats.observe_request(
				&PhaseTimings {
					cache_result: Some(result),
					..Default::default()
				},
				true,
			);
		}
		assert_eq!(stats.static_requests.load(Ordering::Relaxed), 4);
		assert_eq!(stats.static_hits.load(Ordering::Relaxed), 1);
		assert_eq!(stats.static_misses.load(Ordering::Relaxed), 2);
		assert_eq!(stats.static_joined.load(Ordering::Relaxed), 1);
		assert_eq!(stats.cache_evicted_reaccesses.load(Ordering::Relaxed), 1);
		stats.record_static_insertion(1234);
		assert_eq!(stats.static_cache_insertions.load(Ordering::Relaxed), 1);
		assert_eq!(stats.static_output_bytes.load(Ordering::Relaxed), 1234);
	}

	#[test]
	fn otlp_metrics_are_cumulative_across_periodic_stats_reset() {
		let exporter = InMemoryMetricExporter::default();
		let provider = SdkMeterProvider::builder()
			.with_periodic_exporter(exporter.clone())
			.build();
		let stats = Arc::new(GlobalStats::with_otlp(Some(&provider)));
		let timings = PhaseTimings {
			dns_hit: Some(DnsHitStatus::Hit),
			upstream_status: Some(503),
			cache_result: Some(CacheResult::Stale),
			passthrough: true,
			check: Duration::from_millis(1),
			dl_wait: Duration::from_millis(2),
			ttfb: Duration::from_millis(3),
			body: Duration::from_millis(4),
			cpu_wait: Duration::from_millis(5),
			buffer_wait: Duration::from_millis(5),
			decode: Duration::from_millis(6),
			encode: Duration::from_millis(7),
			anim: true,
			anim_frames: 3,
			anim_in_bytes: 1000,
			anim_out_bytes: 400,
			fetch_err: Some("timeout".to_owned()),
			retried: true,
			fetch_retry_succeeded: true,
			..Default::default()
		};

		stats.requests.fetch_add(1, Ordering::Relaxed);
		stats
			.otlp
			.as_ref()
			.unwrap()
			.record_completion(200, false, Duration::from_millis(20));
		stats
			.otlp
			.as_ref()
			.unwrap()
			.record_domains("caller.example", "target.example", false);
		assert_eq!(stats.swap_reset(), (1, 0, 0, 0));
		stats.requests.fetch_add(1, Ordering::Relaxed);
		stats.errors.fetch_add(1, Ordering::Relaxed);
		stats.otlp.as_ref().unwrap().record_phases(&timings, true);
		stats
			.otlp
			.as_ref()
			.unwrap()
			.record_outcome(&timings, 200, false, None);
		stats.otlp.as_ref().unwrap().record_outcome(
			&PhaseTimings::default(),
			502,
			true,
			Some("DecodeError_invalid"),
		);
		stats
			.otlp
			.as_ref()
			.unwrap()
			.record_processed_output(Some("image/webp"), 1000, 400);
		stats
			.otlp
			.as_ref()
			.unwrap()
			.fetch_retry_attempts
			.add(1, &[]);
		stats.otlp.as_ref().unwrap().record_resource_snapshot(
			ResourceSnapshot {
				cache_entries: 2,
				cache_bytes: 1024,
				cache_capacity_bytes: 4096,
				singleflight_active: 1,
				downloads_active: 2,
				downloads_limit: 4,
				cpu_active: 1,
				cpu_limit: 2,
				buffer_used_bytes: 2048,
				buffer_limit_bytes: 8192,
				dns_cache_entries: 8,
				dns_cache_capacity_entries: 1024,
				uptime_seconds: 1.0,
			},
			(1, 1),
			(1, 1),
		);
		stats
			.otlp
			.as_ref()
			.unwrap()
			.record_completion(503, true, Duration::from_millis(30));
		stats.observe_host_throttle(&Ok(HostThrottlePermit {
			waited: Duration::from_millis(10),
			throttled: true,
			_gate: None,
		}));
		stats.observe_host_throttle(&Ok(HostThrottlePermit {
			waited: Duration::ZERO,
			throttled: false,
			_gate: None,
		}));
		assert_eq!(stats.throttle_passed.load(Ordering::Relaxed), 1);
		assert_eq!(stats.throttle_waited.load(Ordering::Relaxed), 1);
		{
			let _active = ActiveRequestGuard::new(stats.otlp.as_ref().unwrap().clone());
		}
		assert_eq!(stats.swap_reset(), (1, 1, 0, 0));

		provider.force_flush().expect("metrics should flush");
		let exports = exporter
			.get_finished_metrics()
			.expect("metrics should be exported");
		let metrics = exports
			.last()
			.expect("one metrics export")
			.scope_metrics()
			.flat_map(|scope| scope.metrics())
			.collect::<Vec<_>>();
		let names = metrics
			.iter()
			.map(|metric| metric.name())
			.collect::<HashSet<_>>();
		for name in [
			"media_proxy_requests_total",
			"media_proxy_errors_total",
			"media_proxy_domain_requests_total",
			"media_proxy_requests_active",
			"media_proxy_upstream_responses_total",
			"media_proxy_request_duration",
			"media_proxy_url_check_duration",
			"media_proxy_download_wait_duration",
			"media_proxy_upstream_ttfb_duration",
			"media_proxy_upstream_body_duration",
			"media_proxy_cpu_wait_duration",
			"media_proxy_decode_duration",
			"media_proxy_encode_duration",
			"media_proxy_cache_requests_total",
			"media_proxy_cache_entries",
			"media_proxy_cache_bytes",
			"media_proxy_cache_capacity_bytes",
			"media_proxy_cache_capacity_evictions_total",
			"media_proxy_cache_expired_evictions_total",
			"media_proxy_singleflight_active",
			"media_proxy_static_requests_total",
			"media_proxy_downloads_active",
			"media_proxy_downloads_limit",
			"media_proxy_cpu_active",
			"media_proxy_cpu_limit",
			"media_proxy_buffer_used_bytes",
			"media_proxy_buffer_limit_bytes",
			"media_proxy_buffer_wait_duration",
			"media_proxy_outputs_total",
			"media_proxy_input_bytes_total",
			"media_proxy_output_bytes_total",
			"media_proxy_passthrough_total",
			"media_proxy_processing_errors_total",
			"media_proxy_fetch_errors_total",
			"media_proxy_host_throttle_requests_total",
			"media_proxy_host_throttle_wait_duration",
			"media_proxy_fetch_retry_attempts_total",
			"media_proxy_fetch_retry_successes_total",
			"media_proxy_dns_cache_requests_total",
			"media_proxy_dns_cache_entries",
			"media_proxy_dns_cache_capacity_entries",
			"media_proxy_dns_retry_attempts_total",
			"media_proxy_dns_retry_successes_total",
			"media_proxy_stale_served_total",
			"media_proxy_animations_total",
			"media_proxy_animation_frames_total",
			"media_proxy_animation_input_bytes_total",
			"media_proxy_animation_output_bytes_total",
			"media_proxy_uptime",
		] {
			assert!(names.contains(name), "missing metric: {name}");
		}
		let requests = metrics
			.iter()
			.find(|metric| metric.name() == "media_proxy_requests_total")
			.expect("request counter");
		let AggregatedMetrics::U64(MetricData::Sum(requests)) = requests.data() else {
			panic!("request counter should export as a u64 sum");
		};
		assert_eq!(
			requests
				.data_points()
				.map(|data_point| data_point.value())
				.sum::<u64>(),
			2
		);
		drop(stats);
		provider.shutdown().expect("provider should shut down");
	}

	#[test]
	fn status_classes_are_low_cardinality() {
		assert_eq!(status_class(200), "2xx");
		assert_eq!(status_class(404), "4xx");
		assert_eq!(status_class(503), "5xx");
		assert_eq!(status_class(999), "other");
		assert_eq!(output_format(Some("image/jpeg")), "jpeg");
		assert_eq!(output_format(Some("image/png; charset=binary")), "png");
		assert_eq!(output_format(Some("image/webp")), "webp");
		assert_eq!(output_format(Some("image/avif")), "avif");
		assert_eq!(output_format(Some("image/gif")), "other");
		assert_eq!(proxy_error_category(Some("DecodeError")), Some("decode"));
		assert_eq!(proxy_error_category(Some("EncodeError")), Some("encode"));
		assert_eq!(proxy_error_category(Some("length:2>1")), Some("size"));
		assert_eq!(
			proxy_error_category(Some("Blocked address")),
			Some("policy")
		);
		assert_eq!(
			proxy_error_category(Some("RelativeUrlWithoutBase")),
			Some("policy")
		);
		assert_eq!(proxy_error_category(Some("unexpected")), Some("internal"));
		assert_eq!(proxy_error_category(Some("status:404")), None);
		assert_eq!(fetch_error_category("dns:lookup failed"), "dns");
		assert_eq!(fetch_error_category("connect"), "connect");
		assert_eq!(fetch_error_category("timeout"), "timeout");
		assert_eq!(fetch_error_category("reset"), "reset");
		assert_eq!(fetch_error_category("body"), "body");
		assert_eq!(fetch_error_category("throttle:wait_timeout"), "throttle");
		assert_eq!(fetch_error_category("unexpected:detail"), "other");
	}
}

#[cfg(test)]
mod network_policy_tests {
	use super::*;

	#[test]
	fn host_throttle_bypasses_until_429_then_waits_and_recovers() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_time()
			.build()
			.unwrap();
		runtime.block_on(async {
			let throttle = HostThrottle::with_limits(1.0, 2, Duration::from_secs(2), 16, 8);
			let target = reqwest::Url::parse("https://throttled.example/image.webp").unwrap();
			let other = reqwest::Url::parse("https://example.com/image.webp").unwrap();

			for _ in 0..10 {
				let permit = throttle
					.acquire(&target, Duration::from_secs(2))
					.await
					.unwrap();
				assert!(!permit.throttled);
			}

			let mut headers = reqwest::header::HeaderMap::new();
			headers.insert(reqwest::header::RETRY_AFTER, "1".parse().unwrap());
			throttle
				.observe_response(
					&target,
					reqwest::StatusCode::TOO_MANY_REQUESTS,
					&headers,
					false,
				)
				.await;
			assert!(throttle.is_recovering(&target).await);
			let started = Instant::now();
			let permit = throttle
				.acquire(&target, Duration::from_secs(2))
				.await
				.unwrap();
			assert!(permit.throttled);
			assert!(started.elapsed() >= Duration::from_millis(900));
			let unaffected = throttle
				.acquire(&other, Duration::from_millis(1))
				.await
				.unwrap();
			assert!(!unaffected.throttled);
			throttle
				.observe_response(&target, reqwest::StatusCode::OK, &headers, true)
				.await;
			drop(permit);
		});
	}

	#[test]
	fn retry_after_supports_seconds_and_http_dates() {
		let mut headers = reqwest::header::HeaderMap::new();
		headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
		assert_eq!(parse_retry_after(&headers), None);

		headers.insert(reqwest::header::RETRY_AFTER, "12".parse().unwrap());
		assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));

		let retry_at = SystemTime::now() + Duration::from_secs(30);
		headers.insert(
			reqwest::header::RETRY_AFTER,
			httpdate::fmt_http_date(retry_at).parse().unwrap(),
		);
		let parsed = parse_retry_after(&headers).unwrap();
		assert!(parsed >= Duration::from_secs(29));
		assert!(parsed <= Duration::from_secs(30));
	}

	#[test]
	fn rate_limit_header_snapshot_captures_diagnostic_headers() {
		let mut headers = reqwest::header::HeaderMap::new();
		headers.insert(reqwest::header::RETRY_AFTER, "12".parse().unwrap());
		headers.insert("ratelimit-limit", "120".parse().unwrap());
		headers.insert("ratelimit-remaining", "0".parse().unwrap());
		headers.insert("ratelimit-reset", "9".parse().unwrap());
		headers.insert("ratelimit-policy", "120;w=60".parse().unwrap());
		headers.insert("x-ratelimit-limit", "120".parse().unwrap());
		headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
		headers.insert("x-ratelimit-reset", "9".parse().unwrap());
		headers.insert("server", "cloudflare".parse().unwrap());
		headers.insert("cf-ray", "test-ray-NRT".parse().unwrap());

		let snapshot = RateLimitHeaderSnapshot::from_headers(&headers);
		assert_eq!(snapshot.retry_after.as_deref(), Some("12"));
		assert_eq!(snapshot.retry_after_ms, Some(12_000));
		assert_eq!(snapshot.ratelimit_limit.as_deref(), Some("120"));
		assert_eq!(snapshot.ratelimit_remaining.as_deref(), Some("0"));
		assert_eq!(snapshot.ratelimit_reset.as_deref(), Some("9"));
		assert_eq!(snapshot.ratelimit_policy.as_deref(), Some("120;w=60"));
		assert_eq!(snapshot.x_ratelimit_limit.as_deref(), Some("120"));
		assert_eq!(snapshot.x_ratelimit_remaining.as_deref(), Some("0"));
		assert_eq!(snapshot.x_ratelimit_reset.as_deref(), Some("9"));
		assert_eq!(snapshot.server.as_deref(), Some("cloudflare"));
		assert_eq!(snapshot.cf_ray.as_deref(), Some("test-ray-NRT"));
	}

	#[test]
	fn negative_cache_response_preserves_status_and_disables_client_caching() {
		let config = base_config();
		let params = RequestParams {
			url: "https://example.com/missing.png".to_owned(),
			r#static: None,
			emoji: None,
			avatar: None,
			preview: None,
			badge: None,
			fallback: None,
		};
		let response = build_negative_response(404, &params, &config, &Arc::new(vec![1, 2, 3]));
		assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
		assert_eq!(
			response.headers().get("X-Proxy-Error").unwrap(),
			"status:404"
		);
		assert_eq!(response.headers().get("Cache-Control").unwrap(), "no-store");
	}

	#[test]
	fn negative_cache_response_honors_fallback() {
		let config = base_config();
		let params = RequestParams {
			url: "https://example.com/gone.png".to_owned(),
			r#static: None,
			emoji: None,
			avatar: None,
			preview: None,
			badge: None,
			fallback: Some("1".to_owned()),
		};
		let response = build_negative_response(410, &params, &config, &Arc::new(vec![1, 2, 3]));
		assert_eq!(response.status(), axum::http::StatusCode::OK);
		assert_eq!(response.headers().get("Content-Type").unwrap(), "image/png");
		assert_eq!(
			response.headers().get("X-Proxy-Error").unwrap(),
			"status:410"
		);
	}

	#[test]
	fn host_throttle_reduces_rate_on_429_and_recovers_on_success() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_time()
			.build()
			.unwrap();
		runtime.block_on(async {
			let throttle = HostThrottle::with_limits(2.0, 30, Duration::from_secs(5), 16, 64);
			let target = reqwest::Url::parse("https://adaptive.example/image.webp").unwrap();
			let headers = reqwest::header::HeaderMap::new();

			throttle
				.observe_response(
					&target,
					reqwest::StatusCode::TOO_MANY_REQUESTS,
					&headers,
					false,
				)
				.await;
			assert_eq!(throttle.rate_for(&target).await, Some(1.0));
			throttle
				.observe_response(&target, reqwest::StatusCode::OK, &headers, false)
				.await;
			assert_eq!(throttle.rate_for(&target).await, Some(1.0));
			assert!(throttle.is_recovering(&target).await);
			throttle
				.observe_response(
					&target,
					reqwest::StatusCode::TOO_MANY_REQUESTS,
					&headers,
					true,
				)
				.await;
			assert_eq!(throttle.rate_for(&target).await, Some(0.5));

			for _ in 0..120 {
				throttle
					.observe_response(&target, reqwest::StatusCode::OK, &headers, true)
					.await;
			}
			assert_eq!(throttle.rate_for(&target).await, Some(2.0));
			assert!(!throttle.is_recovering(&target).await);
		});
	}

	#[test]
	fn host_throttle_fetches_image_after_cooldown() {
		use std::io::Read as _;

		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let server = std::thread::spawn(move || {
			for status in [429, 200] {
				let (mut stream, _) = listener.accept().unwrap();
				let mut request = [0_u8; 1024];
				let _ = stream.read(&mut request).unwrap();
				let response = if status == 429 {
					"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
				} else {
					"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 5\r\nConnection: close\r\n\r\nimage"
				};
				stream.write_all(response.as_bytes()).unwrap();
			}
		});

		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		runtime.block_on(async {
			let throttle = HostThrottle::with_limits(1.0, 2, Duration::from_secs(2), 16, 8);
			let target = reqwest::Url::parse(&format!("http://{address}/image.png")).unwrap();
			let client = reqwest::Client::new();

			let first_permit = throttle
				.acquire(&target, Duration::from_secs(2))
				.await
				.unwrap();
			let first = client.get(target.clone()).send().await.unwrap();
			assert_eq!(first.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
			throttle
				.observe_response(&target, first.status(), first.headers(), false)
				.await;
			drop(first_permit);

			let started = Instant::now();
			let second_permit = throttle
				.acquire(&target, Duration::from_secs(2))
				.await
				.unwrap();
			assert!(started.elapsed() >= Duration::from_millis(900));
			let second = client.get(target.clone()).send().await.unwrap();
			assert_eq!(second.status(), reqwest::StatusCode::OK);
			throttle
				.observe_response(&target, second.status(), second.headers(), true)
				.await;
			assert_eq!(second.bytes().await.unwrap().as_ref(), b"image");
			drop(second_permit);
		});
		server.join().unwrap();
	}

	#[test]
	fn host_throttle_config_defaults_for_existing_config() {
		let mut value = serde_json::to_value(base_config()).unwrap();
		value.as_object_mut().unwrap().remove("host_throttle");
		let config: ConfigFile = serde_json::from_value(value).unwrap();
		assert!(config.host_throttle.is_none());
	}

	fn test_dns_cache(timeout: Duration) -> DnsCache {
		DnsCache::new(Duration::from_secs(60), Duration::from_secs(1), timeout, 16)
	}
	fn base_config() -> ConfigFile {
		ConfigFile {
			bind_addr: "0.0.0.0:12766".to_owned(),
			timeout: 10000,
			user_agent: "test".to_owned(),
			max_size: 1024,
			proxy: None,
			filter_type: FilterType::Triangle,
			max_pixels: 2048,
			append_headers: vec![],
			load_system_fonts: false,
			webp_quality: 75.0,
			encode_avif: false,
			allowed_networks: None,
			blocked_networks: None,
			blocked_hosts: None,
			unix_socket_permissions: None,
			slow_log_ms: default_slow_log_ms(),
			enable_cache: false,
			cache_max_bytes: default_cache_max_bytes(),
			cache_entry_max_bytes: default_cache_entry_max_bytes(),
			cache_ttl_secs: default_cache_ttl_secs(),
			negative_cache_404_ttl_secs: default_negative_cache_404_ttl_secs(),
			negative_cache_410_ttl_secs: default_negative_cache_410_ttl_secs(),
			negative_cache_max_entries: default_negative_cache_max_entries(),
			passthrough_max_bytes: default_passthrough_max_bytes(),
			dns_negative_ttl_secs: default_dns_negative_ttl_secs(),
			dns_timeout_ms: default_dns_timeout_ms(),
			dns_ttl_secs: default_dns_ttl_secs(),
			dns_cache_max_entries: default_dns_cache_max_entries(),
			jpeg_quality: default_jpeg_quality(),
			webp_method: default_webp_method(),
			max_concurrent_downloads: default_max_concurrent_downloads(),
			inflight_buffer_budget_bytes: default_inflight_buffer_budget(),
			connect_timeout_ms: default_connect_timeout_ms(),
			fetch_retry_delay_ms: default_fetch_retry_delay_ms(),
			host_throttle: None,
			cache_stale_max_secs: default_cache_stale_max_secs(),
			otlp_metrics_endpoint: None,
			otlp_export_interval_ms: default_otlp_export_interval_ms(),
			otlp_service_name: default_otlp_service_name(),
		}
	}
	#[test]
	fn dns_cache_max_entries_defaults_for_existing_config() {
		let mut value = serde_json::to_value(base_config()).unwrap();
		value
			.as_object_mut()
			.unwrap()
			.remove("dns_cache_max_entries");
		let config: ConfigFile = serde_json::from_value(value).unwrap();
		assert_eq!(config.dns_cache_max_entries, 1024);
	}
	#[test]
	fn otlp_metrics_defaults_for_existing_config() {
		let mut value = serde_json::to_value(base_config()).unwrap();
		let object = value.as_object_mut().unwrap();
		object.remove("otlp_metrics_endpoint");
		object.remove("otlp_export_interval_ms");
		object.remove("otlp_service_name");
		let config: ConfigFile = serde_json::from_value(value).unwrap();
		assert_eq!(config.otlp_metrics_endpoint, None);
		assert_eq!(config.otlp_export_interval_ms, 5000);
		assert_eq!(config.otlp_service_name, "media-proxy-rs");
	}
	#[test]
	fn unavailable_otlp_collector_does_not_block_initialization() {
		let mut config = base_config();
		config.otlp_metrics_endpoint = Some("http://127.0.0.1:9/v1/metrics".to_owned());
		let start = Instant::now();
		let provider = init_otlp_metrics(&config)
			.expect("valid endpoint should initialize")
			.expect("configured endpoint should enable exporter");
		assert!(start.elapsed() < Duration::from_secs(1));
		provider
			.shutdown_with_timeout(Duration::from_secs(1))
			.expect("empty provider should shut down");
	}
	#[test]
	fn invalid_otlp_settings_are_rejected() {
		let mut config = base_config();
		config.otlp_metrics_endpoint = Some("http://127.0.0.1:4318/v1/metrics".to_owned());
		config.otlp_export_interval_ms = 0;
		assert!(init_otlp_metrics(&config).is_err());
		config.otlp_export_interval_ms = 5000;
		config.otlp_service_name.clear();
		assert!(init_otlp_metrics(&config).is_err());
	}
	#[test]
	fn dns_retry_counters_reset_as_an_interval() {
		let cache = test_dns_cache(Duration::from_millis(20));
		cache.retry_attempts.fetch_add(2, Ordering::Relaxed);
		cache.retry_saved.fetch_add(1, Ordering::Relaxed);
		cache.retry_attempts_total.fetch_add(2, Ordering::Relaxed);
		cache.retry_saved_total.fetch_add(1, Ordering::Relaxed);
		assert_eq!(cache.cumulative_retries(), (2, 1));
		assert_eq!(cache.swap_reset_retries(), (2, 1));
		assert_eq!(cache.swap_reset_retries(), (0, 0));
		assert_eq!(cache.cumulative_retries(), (2, 1));
	}
	#[test]
	fn parse_valid_config() {
		let mut c = base_config();
		c.allowed_networks = Some(vec!["127.0.0.1/32".to_owned()]);
		c.blocked_networks = Some(vec!["10.0.0.0/8".to_owned()]);
		c.blocked_hosts = Some(vec!["Example.COM".to_owned()]);
		let policy = NetworkPolicy::from_config(&c).expect("valid config should parse");
		assert!(policy.blocked_hosts.contains("example.com"));
	}
	#[test]
	fn parse_invalid_network_fails() {
		let mut c = base_config();
		c.blocked_networks = Some(vec!["not-a-cidr".to_owned()]);
		assert!(NetworkPolicy::from_config(&c).is_err());
	}
	#[test]
	fn check_url_error_headers_do_not_expose_details() {
		let errors = [
			CheckUrlError::InvalidUrl("attacker\r\nvalue".to_owned()),
			CheckUrlError::UnsupportedScheme("internal-scheme".to_owned()),
			CheckUrlError::PolicyDenied("10.0.0.1".to_owned()),
			CheckUrlError::ResolveFailed("private-dns-error".to_owned()),
		];
		assert_eq!(
			errors.map(|error| error.as_header()),
			[
				"InvalidUrl",
				"UnsupportedScheme",
				"PolicyDenied",
				"ResolveFailed"
			]
		);
	}
	#[test]
	fn disposition_filename_star_preserves_original_case() {
		let mut headers = HeaderMap::new();
		headers.insert(
			"Content-Disposition",
			"attachment; filename*=UTF-8''Mixed%20Case.PNG"
				.parse()
				.unwrap(),
		);
		RequestContext::disposition_ext(&mut headers, ".webp");
		let disposition = headers
			.get("Content-Disposition")
			.unwrap()
			.to_str()
			.unwrap();
		assert!(disposition.contains("Mixed%20Case.webp"));
	}
	#[test]
	fn private_ipv4_blocked_without_allow() {
		let c = base_config();
		let policy = NetworkPolicy::from_config(&c).unwrap();
		assert!(policy
			.check_ipv4(&std::net::Ipv4Addr::new(10, 0, 0, 1))
			.is_err());
		assert!(policy
			.check_ipv4(&std::net::Ipv4Addr::new(8, 8, 8, 8))
			.is_ok());
	}
	#[test]
	fn allowed_overrides_private() {
		let mut c = base_config();
		c.allowed_networks = Some(vec!["10.1.2.3/32".to_owned()]);
		let policy = NetworkPolicy::from_config(&c).unwrap();
		assert!(policy
			.check_ipv4(&std::net::Ipv4Addr::new(10, 1, 2, 3))
			.is_ok());
		assert!(policy
			.check_ipv4(&std::net::Ipv4Addr::new(10, 1, 2, 4))
			.is_err());
	}
	#[test]
	fn dns_flight_guard_removes_cancelled_owner() {
		let cache = test_dns_cache(Duration::from_millis(20));
		let tx = cache.force_register("cancelled.test");
		{
			let _guard = DnsFlightGuard {
				cache: &cache,
				host: "cancelled.test".to_owned(),
				tx,
				active: true,
			};
		}
		assert!(cache.try_subscribe("cancelled.test").is_none());
	}
	#[test]
	fn dns_flight_guard_does_not_remove_replacement() {
		let cache = test_dns_cache(Duration::from_millis(20));
		let old_tx = cache.force_register("replaced.test");
		let guard = DnsFlightGuard {
			cache: &cache,
			host: "replaced.test".to_owned(),
			tx: old_tx,
			active: true,
		};
		let new_tx = cache.force_register("replaced.test");
		drop(guard);
		let rx = cache
			.try_subscribe("replaced.test")
			.expect("replacement flight should remain");
		assert!(rx.same_channel(&new_tx.subscribe()));
	}
	#[test]
	fn dns_resolve_recovers_from_stale_flight() {
		let cache = test_dns_cache(Duration::from_millis(20));
		let _stale_tx = cache.force_register("localhost");
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let start = Instant::now();
		let result = runtime.block_on(cache.resolve("localhost", 80));
		assert!(result.is_ok());
		assert!(start.elapsed() < Duration::from_secs(1));
	}
}

#[cfg(test)]
mod cache_tests {
	use super::cache::*;
	use std::sync::Arc;
	use std::time::Duration;

	fn test_cache() -> Arc<ResponseCache> {
		Arc::new(ResponseCache::new(CacheConfig {
			enabled: true,
			max_bytes: 1024 * 1024,
			entry_max_bytes: 512 * 1024,
			ttl: Duration::from_secs(60),
			stale_max: Duration::from_secs(86400),
		}))
	}
	fn test_key() -> CacheKey {
		CacheKey {
			url: "https://example.com/test.png".to_owned(),
			is_static: false,
			emoji: false,
			avatar: false,
			preview: false,
			badge: false,
			accept_avif: false,
		}
	}
	#[test]
	fn fingerprint_is_stable_and_does_not_expose_input() {
		let fingerprint = fingerprint_bytes(b"abc");
		assert_eq!(fingerprint, "ba7816bf8f01cfea414140de5dae2223");
		assert_eq!(fingerprint.len(), 32);
		assert!(!fingerprint.contains("abc"));
	}
	#[test]
	fn cache_hit_after_put() {
		let cache = test_cache();
		let key = test_key();
		let entry = CacheEntry::new(200, Some("image/png".to_owned()), None, None, vec![1, 2, 3]);
		cache.put(key.clone(), entry);
		let hit = cache.get(&key);
		assert!(hit.is_some());
		assert_eq!(hit.unwrap().body, vec![1, 2, 3]);
	}
	#[test]
	fn cache_miss_when_disabled() {
		let cache = Arc::new(ResponseCache::new(CacheConfig {
			enabled: false,
			max_bytes: 1024 * 1024,
			entry_max_bytes: 512 * 1024,
			ttl: Duration::from_secs(60),
			stale_max: Duration::from_secs(86400),
		}));
		let key = test_key();
		let entry = CacheEntry::new(200, Some("image/png".to_owned()), None, None, vec![1, 2, 3]);
		cache.put(key.clone(), entry);
		assert!(cache.get(&key).is_none());
	}
	#[test]
	fn cache_skip_non_200() {
		let cache = test_cache();
		let key = test_key();
		let entry = CacheEntry::new(502, None, None, None, vec![1, 2, 3]);
		cache.put(key.clone(), entry);
		assert!(cache.get(&key).is_none());
	}
	#[test]
	fn cache_evicts_on_capacity() {
		let cache = Arc::new(ResponseCache::new(CacheConfig {
			enabled: true,
			max_bytes: 1024,
			entry_max_bytes: 600,
			ttl: Duration::from_secs(60),
			stale_max: Duration::from_secs(86400),
		}));
		let key1 = CacheKey {
			url: "a".to_owned(),
			is_static: false,
			emoji: false,
			avatar: false,
			preview: false,
			badge: false,
			accept_avif: false,
		};
		let key2 = CacheKey {
			url: "b".to_owned(),
			is_static: false,
			emoji: false,
			avatar: false,
			preview: false,
			badge: false,
			accept_avif: false,
		};
		// 各エントリは body + 256 のオーバーヘッド。body=300 → size=556。2つで1112 > 1024
		cache.put(
			key1.clone(),
			CacheEntry::new(200, None, None, None, vec![0; 300]),
		);
		cache.put(
			key2.clone(),
			CacheEntry::new(200, None, None, None, vec![0; 300]),
		);
		// key1 は追い出されているはず
		assert!(cache.get(&key1).is_none());
		assert!(cache.was_capacity_evicted(&key1));
		assert!(!cache.was_capacity_evicted(&key2));
		assert!(cache.get(&key2).is_some());
		assert_eq!(cache.cumulative_evictions(), (1, 0));
		assert_eq!(cache.swap_reset_evictions(), (1, 0));
		assert_eq!(cache.swap_reset_evictions(), (0, 0));
		assert_eq!(cache.cumulative_evictions(), (1, 0));
		cache.put(
			key1.clone(),
			CacheEntry::new(200, None, None, None, vec![0; 300]),
		);
		assert!(!cache.was_capacity_evicted(&key1));
	}
	#[test]
	fn singleflight_count_tracks_active_guards() {
		let cache = test_cache();
		assert_eq!(cache.inflight_count(), 0);
		let guard = cache
			.start_flight(test_key())
			.expect("first request should own the flight");
		assert_eq!(cache.inflight_count(), 1);
		drop(guard);
		assert_eq!(cache.inflight_count(), 0);
	}
	#[test]
	fn cache_skip_oversized_entry() {
		let cache = Arc::new(ResponseCache::new(CacheConfig {
			enabled: true,
			max_bytes: 1024 * 1024,
			entry_max_bytes: 100,
			ttl: Duration::from_secs(60),
			stale_max: Duration::from_secs(86400),
		}));
		let key = test_key();
		// body=200 + overhead=256 → size=456 > entry_max_bytes=100
		cache.put(
			key.clone(),
			CacheEntry::new(200, None, None, None, vec![0; 200]),
		);
		assert!(cache.get(&key).is_none());
	}
}

/// 外部由来バイトを含むエラーの`X-Proxy-Error`値を生成
///
/// ヘッダ不正文字を含む場合があり、unwrapしてはならない(finding #3)
fn error_header_value(
	msg: impl AsRef<str>,
	fallback: &'static str,
) -> reqwest::header::HeaderValue {
	reqwest::header::HeaderValue::from_bytes(msg.as_ref().as_bytes())
		.unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(fallback))
}
