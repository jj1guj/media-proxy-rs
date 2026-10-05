use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

use mixtrics::metrics::{
	BoxedCounter, BoxedCounterVec, BoxedGauge, BoxedGaugeVec, BoxedHistogram, BoxedHistogramVec,
	BoxedRegistry, CounterOps, CounterVecOps, GaugeOps, GaugeVecOps, HistogramOps, HistogramVecOps,
	RegistryOps,
};
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use opentelemetry::KeyValue;

#[derive(Debug)]
struct CounterHandle {
	instrument: Counter<u64>,
	labels: Vec<KeyValue>,
}

impl CounterOps for CounterHandle {
	fn increase(&self, value: u64) {
		self.instrument.add(value, &self.labels);
	}
}

#[derive(Debug)]
struct GaugeHandle {
	value: AtomicU64,
	instrument: Gauge<u64>,
	labels: Vec<KeyValue>,
}

impl GaugeOps for GaugeHandle {
	fn increase(&self, value: u64) {
		let value = self.value.fetch_add(value, Ordering::Relaxed) + value;
		self.instrument.record(value, &self.labels);
	}

	fn decrease(&self, value: u64) {
		let previous = self
			.value
			.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
				Some(current.saturating_sub(value))
			})
			.unwrap_or_default();
		self.instrument
			.record(previous.saturating_sub(value), &self.labels);
	}

	fn absolute(&self, value: u64) {
		self.value.store(value, Ordering::Relaxed);
		self.instrument.record(value, &self.labels);
	}
}

#[derive(Debug)]
struct HistogramHandle {
	instrument: Histogram<f64>,
	labels: Vec<KeyValue>,
}

impl HistogramOps for HistogramHandle {
	fn record(&self, value: f64) {
		self.instrument.record(value, &self.labels);
	}
}

#[derive(Debug)]
struct MetricVec {
	meter: Meter,
	name: Cow<'static, str>,
	description: Cow<'static, str>,
	label_names: &'static [&'static str],
	buckets: Option<Vec<f64>>,
}

impl MetricVec {
	fn labels(&self, values: &[Cow<'static, str>]) -> Vec<KeyValue> {
		assert_eq!(self.label_names.len(), values.len());
		self.label_names
			.iter()
			.zip(values)
			.map(|(name, value)| KeyValue::new(*name, value.clone()))
			.collect()
	}
}

impl CounterVecOps for MetricVec {
	fn counter(&self, labels: &[Cow<'static, str>]) -> BoxedCounter {
		Box::new(CounterHandle {
			instrument: self
				.meter
				.u64_counter(self.name.clone())
				.with_description(self.description.clone())
				.build(),
			labels: self.labels(labels),
		})
	}
}

impl GaugeVecOps for MetricVec {
	fn gauge(&self, labels: &[Cow<'static, str>]) -> BoxedGauge {
		Box::new(GaugeHandle {
			value: AtomicU64::new(0),
			instrument: self
				.meter
				.u64_gauge(self.name.clone())
				.with_description(self.description.clone())
				.build(),
			labels: self.labels(labels),
		})
	}
}

impl HistogramVecOps for MetricVec {
	fn histogram(&self, labels: &[Cow<'static, str>]) -> BoxedHistogram {
		let mut builder = self
			.meter
			.f64_histogram(self.name.clone())
			.with_description(self.description.clone());
		if let Some(buckets) = &self.buckets {
			builder = builder.with_boundaries(buckets.clone());
		}
		Box::new(HistogramHandle {
			instrument: builder.build(),
			labels: self.labels(labels),
		})
	}
}

#[derive(Debug)]
struct OpenTelemetryRegistry {
	meter: Meter,
}

impl RegistryOps for OpenTelemetryRegistry {
	fn register_counter_vec(
		&self,
		name: Cow<'static, str>,
		description: Cow<'static, str>,
		label_names: &'static [&'static str],
	) -> BoxedCounterVec {
		Box::new(MetricVec {
			meter: self.meter.clone(),
			name,
			description,
			label_names,
			buckets: None,
		})
	}

	fn register_gauge_vec(
		&self,
		name: Cow<'static, str>,
		description: Cow<'static, str>,
		label_names: &'static [&'static str],
	) -> BoxedGaugeVec {
		Box::new(MetricVec {
			meter: self.meter.clone(),
			name,
			description,
			label_names,
			buckets: None,
		})
	}

	fn register_histogram_vec(
		&self,
		name: Cow<'static, str>,
		description: Cow<'static, str>,
		label_names: &'static [&'static str],
	) -> BoxedHistogramVec {
		Box::new(MetricVec {
			meter: self.meter.clone(),
			name,
			description,
			label_names,
			buckets: None,
		})
	}

	fn register_histogram_vec_with_buckets(
		&self,
		name: Cow<'static, str>,
		description: Cow<'static, str>,
		label_names: &'static [&'static str],
		buckets: Vec<f64>,
	) -> BoxedHistogramVec {
		Box::new(MetricVec {
			meter: self.meter.clone(),
			name,
			description,
			label_names,
			buckets: Some(buckets),
		})
	}
}

pub fn registry(meter: Meter) -> BoxedRegistry {
	Box::new(OpenTelemetryRegistry { meter })
}
