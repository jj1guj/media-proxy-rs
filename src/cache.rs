use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use tokio::sync::broadcast;

/// キャッシュキー: 正規化URL + 画像系パラメータ + avif 有無。
/// fallback はキーに含めない(結果に影響しないため)。
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct CacheKey {
	pub url: String,
	pub is_static: bool,
	pub emoji: bool,
	pub avatar: bool,
	pub preview: bool,
	pub badge: bool,
	pub accept_avif: bool,
}

/// キャッシュに保存するレスポンスの要約。
#[derive(Clone)]
pub struct CacheEntry {
	pub status: u16,
	pub content_type: Option<String>,
	pub content_disposition: Option<String>,
	pub cache_control: Option<String>,
	pub body: Vec<u8>,
	created_at: Instant,
	size: usize,
}
impl CacheEntry {
	pub fn new(
		status: u16,
		content_type: Option<String>,
		content_disposition: Option<String>,
		cache_control: Option<String>,
		body: Vec<u8>,
	) -> Self {
		let size = body.len() + 256; // ヘッダ文字列等の概算オーバーヘッド
		Self {
			status,
			content_type,
			content_disposition,
			cache_control,
			body,
			created_at: Instant::now(),
			size,
		}
	}
}

pub struct CacheConfig {
	pub enabled: bool,
	pub max_bytes: usize,
	pub entry_max_bytes: usize,
	pub ttl: Duration,
}

/// キャッシュヒット/ミス/合流の区分(ログ用)。
#[derive(Clone, Copy, Debug)]
pub enum CacheResult {
	Hit,
	Miss,
	Joined,
	/// キャッシュ無効、または Range リクエスト等でキャッシュ対象外。
	Bypass,
}
impl std::fmt::Display for CacheResult {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			CacheResult::Hit => write!(f, "hit"),
			CacheResult::Miss => write!(f, "miss"),
			CacheResult::Joined => write!(f, "joined"),
			CacheResult::Bypass => write!(f, "bypass"),
		}
	}
}

/// エンコード済みレスポンスの LRU キャッシュ + singleflight。
pub struct ResponseCache {
	config: CacheConfig,
	/// LRU キャッシュ本体。IndexMap で末尾が最新、先頭が最古。
	entries: Mutex<LruInner>,
	/// singleflight: 処理中のキーに対する broadcast sender。
	inflight: Mutex<HashMap<CacheKey, broadcast::Sender<Option<CacheEntry>>>>,
}

struct LruInner {
	map: IndexMap<CacheKey, CacheEntry>,
	total_bytes: usize,
}

impl ResponseCache {
	pub fn new(config: CacheConfig) -> Self {
		Self {
			config,
			entries: Mutex::new(LruInner {
				map: IndexMap::new(),
				total_bytes: 0,
			}),
			inflight: Mutex::new(HashMap::new()),
		}
	}

	/// キャッシュからエントリを取得する(ヒット時は LRU 位置を更新)。
	pub fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
		if !self.config.enabled {
			return None;
		}
		let mut inner = self.entries.lock().ok()?;
		// TTL チェック付きで取得
		if let Some(entry) = inner.map.get(key) {
			if entry.created_at.elapsed() < self.config.ttl {
				let entry = entry.clone();
				// LRU: 末尾(最新)に移動
				let from = inner.map.get_index_of(key).unwrap();
				let to = inner.map.len() - 1;
				inner.map.move_index(from, to);
				return Some(entry);
			} else {
				// TTL 切れ → 削除
				let removed = inner.map.shift_remove(key);
				if let Some(r) = removed {
					inner.total_bytes = inner.total_bytes.saturating_sub(r.size);
				}
			}
		}
		None
	}

	/// エントリをキャッシュに格納する。
	pub fn put(&self, key: CacheKey, entry: CacheEntry) {
		if !self.config.enabled {
			return;
		}
		if entry.status != 200 {
			return;
		}
		if entry.size > self.config.entry_max_bytes {
			return;
		}
		let Ok(mut inner) = self.entries.lock() else {
			return;
		};
		// 既存エントリがあれば削除してサイズ回収
		if let Some(old) = inner.map.shift_remove(&key) {
			inner.total_bytes = inner.total_bytes.saturating_sub(old.size);
		}
		// 容量に収まるまで先頭(最古)から追い出し
		while inner.total_bytes + entry.size > self.config.max_bytes && !inner.map.is_empty() {
			if let Some((_, evicted)) = inner.map.shift_remove_index(0) {
				inner.total_bytes = inner.total_bytes.saturating_sub(evicted.size);
			}
		}
		inner.total_bytes += entry.size;
		inner.map.insert(key, entry);
	}

	/// singleflight: 同一キーの処理に合流する receiver を取得する。
	/// None が返れば自分が最初のリクエスト(処理を開始すべき)。
	pub fn try_join(&self, key: &CacheKey) -> Option<broadcast::Receiver<Option<CacheEntry>>> {
		if !self.config.enabled {
			return None;
		}
		let inflight = self.inflight.lock().ok()?;
		inflight.get(key).map(|tx| tx.subscribe())
	}

	/// singleflight: 処理開始を登録し、完了時に通知するための guard を返す。
	/// self が Arc で保持されている前提。guard は Arc を clone して所有する。
	pub fn start_flight(self: &Arc<Self>, key: CacheKey) -> Option<FlightGuard> {
		if !self.config.enabled {
			return None;
		}
		let mut inflight = self.inflight.lock().ok()?;
		if inflight.contains_key(&key) {
			return None; // 既に誰かが処理中(try_join 側で合流すべき)
		}
		let (tx, _) = broadcast::channel(1);
		inflight.insert(key.clone(), tx.clone());
		Some(FlightGuard {
			key,
			tx,
			cache: Arc::clone(self),
			completed: false,
		})
	}

	/// singleflight: 処理完了を通知し、成功なら結果をキャッシュに格納する。
	pub fn complete_flight(self: &Arc<Self>, guard: &mut FlightGuard, entry: Option<CacheEntry>) {
		if let Some(ref e) = entry {
			self.put(guard.key.clone(), e.clone());
		}
		// broadcast で待機者に通知(受信者がいなくても SendError は無視)
		let _ = guard.tx.send(entry);
		guard.completed = true;
		// inflight から削除
		if let Ok(mut inflight) = self.inflight.lock() {
			inflight.remove(&guard.key);
		}
	}
}

/// singleflight の処理中ガード。Drop で inflight を掃除する(complete 忘れ/パニック対策)。
pub struct FlightGuard {
	key: CacheKey,
	tx: broadcast::Sender<Option<CacheEntry>>,
	cache: Arc<ResponseCache>,
	completed: bool,
}
impl Drop for FlightGuard {
	fn drop(&mut self) {
		if !self.completed {
			// 完了せずに drop された → 待機者にエラー(None)を通知
			let _ = self.tx.send(None);
			if let Ok(mut inflight) = self.cache.inflight.lock() {
				inflight.remove(&self.key);
			}
		}
	}
}
