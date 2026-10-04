use foyer::{BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, RecoverMode};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: usize = 40 * 1024 * 1024;
const BUFFER_POOL_SIZE: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceEntry {
	pub fetched_at_ms: u64,
	pub status: u16,
	pub content_type: Option<String>,
	pub content_disposition: Option<String>,
	pub body: Vec<u8>,
}

impl SourceEntry {
	pub fn new(
		status: u16,
		content_type: Option<String>,
		content_disposition: Option<String>,
		body: Vec<u8>,
	) -> Self {
		Self {
			fetched_at_ms: now_ms(),
			status,
			content_type,
			content_disposition,
			body,
		}
	}
}

#[derive(Clone, Debug)]
pub enum SourceLookup {
	Fresh(SourceEntry),
	Stale(SourceEntry),
}

#[derive(Clone)]
pub struct SourceCache {
	cache: HybridCache<String, SourceEntry>,
	fresh_for: Duration,
	stale_for: Duration,
	max_entry_bytes: usize,
}

impl SourceCache {
	pub async fn open(
		path: impl AsRef<Path>,
		capacity_bytes: usize,
		max_entry_bytes: usize,
		fresh_for: Duration,
		stale_for: Duration,
	) -> foyer::Result<Self> {
		let device = FsDeviceBuilder::new(path)
			.with_capacity(capacity_bytes)
			.build()?;
		let engine = BlockEngineConfig::new(device)
			.with_block_size(BLOCK_SIZE)
			.with_buffer_pool_size(BUFFER_POOL_SIZE)
			.with_submit_queue_size_threshold(BUFFER_POOL_SIZE * 2)
			.with_tombstone_log(true);
		let cache = HybridCache::builder()
			.with_name("source")
			.memory(1)
			.storage()
			.with_engine_config(engine)
			.with_recover_mode(RecoverMode::Quiet)
			.build()
			.await?;
		Ok(Self {
			cache,
			fresh_for,
			stale_for,
			max_entry_bytes,
		})
	}

	pub async fn get(&self, url: &str) -> foyer::Result<Option<SourceLookup>> {
		let Some(entry) = self.cache.get(url).await? else {
			return Ok(None);
		};
		let entry = entry.value().clone();
		match classify_age(
			entry.fetched_at_ms,
			now_ms(),
			self.fresh_for,
			self.stale_for,
		) {
			Age::Fresh => Ok(Some(SourceLookup::Fresh(entry))),
			Age::Stale => Ok(Some(SourceLookup::Stale(entry))),
			Age::Expired => {
				self.cache.remove(url);
				Ok(None)
			}
		}
	}

	pub fn put(&self, url: String, entry: SourceEntry) -> bool {
		if entry.status != 200 || !self.can_store(entry.body.len()) {
			return false;
		}
		self.cache
			.storage_writer(url)
			.force()
			.insert(entry)
			.is_some()
	}

	pub fn can_store(&self, body_len: usize) -> bool {
		body_len <= self.max_entry_bytes
	}

	pub async fn close(&self) -> foyer::Result<()> {
		self.cache.close().await
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Age {
	Fresh,
	Stale,
	Expired,
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

fn classify_age(fetched_at_ms: u64, now_ms: u64, fresh_for: Duration, stale_for: Duration) -> Age {
	let age_ms = now_ms.saturating_sub(fetched_at_ms);
	let fresh_ms = fresh_for.as_millis() as u64;
	let stale_deadline_ms = fresh_ms.saturating_add(stale_for.as_millis() as u64);
	if age_ms <= fresh_ms {
		Age::Fresh
	} else if age_ms <= stale_deadline_ms {
		Age::Stale
	} else {
		Age::Expired
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::{AtomicU64, Ordering};

	static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

	#[test]
	fn classifies_fresh_stale_and_expired_entries() {
		let fresh_for = Duration::from_secs(12 * 60 * 60);
		let stale_for = Duration::from_secs(24 * 60 * 60);
		let hour_ms = 60 * 60 * 1000;

		assert_eq!(
			classify_age(0, 12 * hour_ms, fresh_for, stale_for),
			Age::Fresh
		);
		assert_eq!(
			classify_age(0, 13 * hour_ms, fresh_for, stale_for),
			Age::Stale
		);
		assert_eq!(
			classify_age(0, 37 * hour_ms, fresh_for, stale_for),
			Age::Expired
		);
	}

	#[tokio::test]
	async fn stores_and_reads_disk_only_entry() {
		let path = std::env::temp_dir().join(format!(
			"media-proxy-source-cache-{}-{}",
			std::process::id(),
			NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
		));
		let cache = SourceCache::open(
			&path,
			BLOCK_SIZE * 2,
			1024,
			Duration::from_secs(60),
			Duration::from_secs(60),
		)
		.await
		.unwrap();
		let entry = SourceEntry::new(200, Some("image/png".to_owned()), None, vec![1, 2, 3]);

		assert!(cache.put("https://example.com/image.png".to_owned(), entry));
		let lookup = cache.get("https://example.com/image.png").await.unwrap();
		match lookup {
			Some(SourceLookup::Fresh(entry)) => assert_eq!(entry.body, vec![1, 2, 3]),
			other => panic!("unexpected source cache lookup: {other:?}"),
		}

		cache.close().await.unwrap();
		drop(cache);
		std::fs::remove_dir_all(path).unwrap();
	}
}
