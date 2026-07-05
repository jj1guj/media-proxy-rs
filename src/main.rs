use core::str;
use std::{io::Write, net::SocketAddr, pin::Pin, str::FromStr, sync::Arc};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::error::Error as _;

use axum::{http::HeaderMap, response::IntoResponse, Router};
use iprange::IpRange;
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore};
use tokio_stream::StreamExt;

mod img;
mod svg;
mod browsersafe;
mod image_test;
mod cache;

use cache::{CacheKey, CacheResult, ResponseCache};

/// 定期統計ログ用のグローバルカウンタ。
/// emit_summary でインクリメントし、60秒ごとにリセット＋ログ出力する。
struct GlobalStats {
	requests: AtomicU64,
	errors: AtomicU64,
	cache_hits: AtomicU64,
	cache_misses: AtomicU64,
	ferr_connect: AtomicU64,
	ferr_timeout: AtomicU64,
	ferr_dns: AtomicU64,
	ferr_reset: AtomicU64,
	ferr_body: AtomicU64,
	ferr_other: AtomicU64,
	retry_attempts: AtomicU64,
	retry_saved: AtomicU64,
	http1_responses: AtomicU64,
	http2_responses: AtomicU64,
}
impl GlobalStats {
	fn new() -> Self {
		Self {
			requests: AtomicU64::new(0),
			errors: AtomicU64::new(0),
			cache_hits: AtomicU64::new(0),
			cache_misses: AtomicU64::new(0),
			ferr_connect: AtomicU64::new(0),
			ferr_timeout: AtomicU64::new(0),
			ferr_dns: AtomicU64::new(0),
			ferr_reset: AtomicU64::new(0),
			ferr_body: AtomicU64::new(0),
			ferr_other: AtomicU64::new(0),
			retry_attempts: AtomicU64::new(0),
			retry_saved: AtomicU64::new(0),
			http1_responses: AtomicU64::new(0),
			http2_responses: AtomicU64::new(0),
		}
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
	fn swap_reset_ferr(&self) -> (u64, u64, u64, u64, u64, u64) {
		(
			self.ferr_connect.swap(0, Ordering::Relaxed),
			self.ferr_timeout.swap(0, Ordering::Relaxed),
			self.ferr_dns.swap(0, Ordering::Relaxed),
			self.ferr_reset.swap(0, Ordering::Relaxed),
			self.ferr_body.swap(0, Ordering::Relaxed),
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
			"connect" => { self.ferr_connect.fetch_add(1, Ordering::Relaxed); },
			"timeout" => { self.ferr_timeout.fetch_add(1, Ordering::Relaxed); },
			"dns" => { self.ferr_dns.fetch_add(1, Ordering::Relaxed); },
			"reset" => { self.ferr_reset.fetch_add(1, Ordering::Relaxed); },
			"body" => { self.ferr_body.fetch_add(1, Ordering::Relaxed); },
			_ => { self.ferr_other.fetch_add(1, Ordering::Relaxed); },
		}
	}
}

type AppState = (reqwest::Client, Arc<ConfigFile>, Arc<Vec<u8>>, Arc<resvg::usvg::fontdb::Database>, Arc<Semaphore>, Arc<NetworkPolicy>, Arc<DnsCache>, Arc<ResponseCache>, Arc<Semaphore>, Arc<Semaphore>, Arc<GlobalStats>);

#[derive(Debug,Serialize,Deserialize)]
pub struct ConfigFile{
	bind_addr: String,
	timeout:u64,
	user_agent:String,
	max_size:u64,
	proxy:Option<String>,
	filter_type:FilterType,
	max_pixels:u32,
	append_headers:Vec<String>,
	load_system_fonts:bool,
	webp_quality:f32,
	encode_avif:bool,
	allowed_networks:Option<Vec<String>>,
	blocked_networks:Option<Vec<String>>,
	blocked_hosts:Option<Vec<String>>,
	/// 正常完了かつ全フェーズ合計がこのms未満のリクエストは、
	/// アクセスログを INFO ではなく DEBUG に落とす(ヘルスチェック等でログが埋まるのを防ぐ)。
	#[serde(default = "default_slow_log_ms")]
	slow_log_ms:u64,
	#[serde(default = "default_true")]
	enable_cache:bool,
	/// キャッシュ合計バイト数上限(既定128MB)。
	#[serde(default = "default_cache_max_bytes")]
	cache_max_bytes:u64,
	/// 1エントリのバイト数上限(既定5MB)。超過するレスポンスはキャッシュしない。
	#[serde(default = "default_cache_entry_max_bytes")]
	cache_entry_max_bytes:u64,
	/// キャッシュTTL(秒、既定3600)。
	#[serde(default = "default_cache_ttl_secs")]
	cache_ttl_secs:u64,
	/// パススルー対象の最大バイトサイズ(既定1MB)。
	/// webp/png/jpeg/gif かつ badge/static 未指定かつ寸法が目標以下かつこのサイズ以下なら
	/// デコード・再エンコードせず元バイト列を返す。
	#[serde(default = "default_passthrough_max_bytes")]
	passthrough_max_bytes:u64,
	/// DNS解決失敗のネガティブキャッシュTTL(秒、既定10)。
	/// 落ちているドメインへの連続リクエストが毎回1.5秒のDNSタイムアウトを踏むのを防ぐ。
	#[serde(default = "default_dns_negative_ttl_secs")]
	dns_negative_ttl_secs:u64,
	/// DNS解決のタイムアウト(ms、既定4000)。タイムアウト時は1回リトライする(合計最大約8秒)。
	#[serde(default = "default_dns_timeout_ms")]
	dns_timeout_ms:u64,
	/// DNSキャッシュのTTL(秒、既定300)。
	#[serde(default = "default_dns_ttl_secs")]
	dns_ttl_secs:u64,
	/// JPEG出力用の品質(0-100、既定85)。webp_qualityの流用をやめる。
	#[serde(default = "default_jpeg_quality")]
	jpeg_quality:i32,
	/// WebPエンコードのmethod(0-6、既定4)。
	/// 値が小さいほどエンコードが速いが圧縮率が下がる。Pi等の低性能環境ではmethod=2を推奨。
	#[serde(default = "default_webp_method")]
	webp_method:i32,
	/// ダウンロードの最大同時接続数(既定24)。バースト時にオリジンへの同時接続が無制限にならないようにする。
	#[serde(default = "default_max_concurrent_downloads")]
	max_concurrent_downloads:usize,
	/// 同時ダウンロードの合計バイト予算(既定256MB)。
	/// load_all前に予約し、エンコード完了後に解放する。
	#[serde(default = "default_inflight_buffer_budget")]
	inflight_buffer_budget_bytes:u64,
	/// TCP接続タイムアウト(ms、既定3000)。全体タイムアウト(timeout)より小さく設定すること。
	#[serde(default = "default_connect_timeout_ms")]
	connect_timeout_ms:u64,
	/// 接続段階失敗時のリトライ前待機(ms、既定500)。SYN再送で瞬断窓を跨ぐ効果を狙う。
	#[serde(default = "default_fetch_retry_delay_ms")]
	fetch_retry_delay_ms:u64,
}
fn default_slow_log_ms()->u64{50}
fn default_true()->bool{true}
fn default_cache_max_bytes()->u64{128*1024*1024}
fn default_cache_entry_max_bytes()->u64{5*1024*1024}
fn default_cache_ttl_secs()->u64{3600}
fn default_passthrough_max_bytes()->u64{1024*1024}
fn default_dns_negative_ttl_secs()->u64{10}
fn default_dns_timeout_ms()->u64{4000}
fn default_dns_ttl_secs()->u64{300}
fn default_jpeg_quality()->i32{85}
fn default_webp_method()->i32{4}
fn default_max_concurrent_downloads()->usize{24}
fn default_inflight_buffer_budget()->u64{256*1024*1024}
fn default_connect_timeout_ms()->u64{3000}
fn default_fetch_retry_delay_ms()->u64{500}
#[derive(Debug, Deserialize)]
pub struct RequestParams{
	url: String,
	//#[serde(rename = "static")]
	r#static:Option<String>,
	emoji:Option<String>,
	avatar:Option<String>,
	preview:Option<String>,
	badge:Option<String>,
	fallback:Option<String>,
}
#[derive(Clone, Copy,Debug,Serialize,Deserialize)]
enum FilterType{
	Nearest,
	Triangle,
	CatmullRom,
	Gaussian,
	Lanczos3,
}
impl From<FilterType> for image::imageops::FilterType{
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
impl From<FilterType> for fast_image_resize::FilterType{
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
	use tokio::signal;
	use futures::{future::FutureExt,pin_mut};
	let ctrl_c = async {
		signal::ctrl_c()
			.await
			.expect("failed to install Ctrl+C handler");
	}.fuse();

	#[cfg(unix)]
	let terminate = async {
		signal::unix::signal(signal::unix::SignalKind::terminate())
			.expect("failed to install signal handler")
			.recv()
			.await;
	}.fuse();
	#[cfg(not(unix))]
	let terminate = std::future::pending::<()>().fuse();
	pin_mut!(ctrl_c, terminate);
	futures::select!{
		_ = ctrl_c => {},
		_ = terminate => {},
	}
}
fn main() {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
		)
		.init();
	let config_path=match std::env::var("MEDIA_PROXY_CONFIG_PATH"){
		Ok(path)=>{
			if path.is_empty(){
				"config.json".to_owned()
			}else{
				path
			}
		},
		Err(_)=>"config.json".to_owned()
	};
	if !std::path::Path::new(&config_path).exists(){
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
			slow_log_ms:default_slow_log_ms(),
			enable_cache:default_true(),
			cache_max_bytes:default_cache_max_bytes(),
			cache_entry_max_bytes:default_cache_entry_max_bytes(),
			cache_ttl_secs:default_cache_ttl_secs(),
			passthrough_max_bytes:default_passthrough_max_bytes(),
			dns_negative_ttl_secs:default_dns_negative_ttl_secs(),
			dns_timeout_ms:default_dns_timeout_ms(),
			dns_ttl_secs:default_dns_ttl_secs(),
			jpeg_quality:default_jpeg_quality(),
			webp_method:default_webp_method(),
			max_concurrent_downloads:default_max_concurrent_downloads(),
			inflight_buffer_budget_bytes:default_inflight_buffer_budget(),
			connect_timeout_ms:default_connect_timeout_ms(),
			fetch_retry_delay_ms:default_fetch_retry_delay_ms(),
		};
		let default_config=serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path).expect("create default config.json").write_all(default_config.as_bytes()).unwrap();
	}
	let mut config:ConfigFile=serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();
	if let Ok(networks)=std::env::var("MEDIA_PROXY_ALLOWED_NETWORKS"){
		let mut allowed_networks=config.allowed_networks.take().unwrap_or_default();
		for networks in networks.split(","){
			allowed_networks.push(networks.to_owned());
		}
		config.allowed_networks.replace(allowed_networks);
	}
	if let Ok(networks)=std::env::var("MEDIA_PROXY_BLOCKED_NETWORKS"){
		let mut blocked_networks=config.blocked_networks.take().unwrap_or_default();
		for networks in networks.split(","){
			blocked_networks.push(networks.to_owned());
		}
		config.blocked_networks.replace(blocked_networks);
	}
	if let Ok(networks)=std::env::var("MEDIA_PROXY_BLOCKED_HOSTS"){
		let mut blocked_hosts=config.blocked_hosts.take().unwrap_or_default();
		for networks in networks.split(","){
			blocked_hosts.push(networks.to_owned());
		}
		config.blocked_hosts.replace(blocked_hosts);
	}
	let dummy_png=Arc::new(include_bytes!("../asset/dummy.png").to_vec());
	let config=Arc::new(config);
	// allowed_networks / blocked_networks / blocked_hosts のパースは起動時に1回だけ行う。
	// 不正な設定値はここで明確なエラーメッセージ付きに失敗させる。
	let network_policy=match NetworkPolicy::from_config(&config){
		Ok(p)=>Arc::new(p),
		Err(e)=>{
			tracing::error!("設定エラー(network): {}",e);
			std::process::exit(1);
		}
	};
	let dns_cache=Arc::new(DnsCache::new(
		Duration::from_secs(config.dns_ttl_secs),
		Duration::from_secs(config.dns_negative_ttl_secs),
		Duration::from_millis(config.dns_timeout_ms),
		DNS_CACHE_MAX_ENTRIES,
	));
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let client=reqwest::ClientBuilder::new();
	let client=match &config.proxy{
		Some(url)=>client.proxy(reqwest::Proxy::http(url).unwrap()),
		None=>client,
	};
	// reqwestのDNS解決をDnsCacheに一本化する。
	// check_urlと実フェッチが同じキャッシュを共有し、1リクエストあたりのDNS解決を実質1回にする。
	let client=client.dns_resolver(Arc::new(SharedDnsResolver(dns_cache.clone())));
	let client=client.connect_timeout(Duration::from_millis(config.connect_timeout_ms));
	if config.connect_timeout_ms >= config.timeout {
		tracing::warn!(
			connect_timeout_ms=config.connect_timeout_ms,
			timeout=config.timeout,
			"connect_timeout_ms >= timeout: リトライの余地がありません"
		);
	}
	let client=client.build().unwrap();
	let mut fontdb=resvg::usvg::fontdb::Database::new();
	if config.load_system_fonts{
		fontdb.load_system_fonts();
	}
	if std::path::Path::new("asset/font/").exists(){
		fontdb.load_fonts_dir("asset/font/");
	}
	fontdb.load_font_source(resvg::usvg::fontdb::Source::Binary(Arc::new(include_bytes!("../asset/font/Aileron-Light.otf"))));
	let fontdb=Arc::new(fontdb);
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
	}));
	let global_stats = Arc::new(GlobalStats::new());
	let arg_tup = (client, config, dummy_png, fontdb, encode_semaphore, network_policy, dns_cache, response_cache, download_semaphore, buffer_budget, global_stats);
	rt.block_on(async{
		// --- 60秒ごとの定期統計ログ ---
		{
			let stats = arg_tup.10.clone();
			let encode_sem = arg_tup.4.clone();
			let dl_sem = arg_tup.8.clone();
			let buf_sem = arg_tup.9.clone();
			let resp_cache = arg_tup.7.clone();
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
					let (fc, ft, fd, fr, fb, fo) = stats.swap_reset_ferr();
					let (retry_att, retry_sav) = stats.swap_reset_retry();
					let (http1, http2) = stats.swap_reset_http();
					let dl_active = max_dl - dl_sem.available_permits();
					let cpu_active = max_encode - encode_sem.available_permits();
					let buf_used = max_buf - buf_sem.available_permits();
					let (cache_entries, cache_bytes) = resp_cache.stats();
					let dns_entries = dns.len();
					tracing::info!(
						requests = reqs,
						errors = errs,
						cache_hits = hits,
						cache_misses = misses,
						dl_active = dl_active as u64,
						dl_max = max_dl as u64,
						cpu_active = cpu_active as u64,
						cpu_max = max_encode as u64,
						buf_used_mb = (buf_used / (1024 * 1024)) as u64,
						buf_max_mb = (max_buf / (1024 * 1024)) as u64,
						cache_entries = cache_entries as u64,
						cache_bytes = cache_bytes as u64,
						dns_entries = dns_entries as u64,
						ferr_connect = fc,
						ferr_timeout = ft,
						ferr_dns = fd,
						ferr_reset = fr,
						ferr_body = fb,
						ferr_other = fo,
						retry_attempts = retry_att,
						retry_saved = retry_sav,
						http1_responses = http1,
						http2_responses = http2,
						"periodic_stats"
					);
				}
			});
		}
		let http_addr:SocketAddr = arg_tup.1.bind_addr.parse().unwrap();
		let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
		let app = Router::new();
		let arg_tup0=arg_tup.clone();
		let app=app.route("/",axum::routing::get(move|headers,parms|get_file(None,headers,arg_tup0.clone(),parms)));
		let app=app.route("/{*path}",axum::routing::get(move|path,headers,parms|get_file(Some(path),headers,arg_tup.clone(),parms)));
		axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown_signal()).await.unwrap();
	});
}
/// DNSキャッシュの最大エントリ数(無制限成長の防止)。
const DNS_CACHE_MAX_ENTRIES: usize = 1024;
/// タイムアウト由来のネガティブキャッシュの短いTTL。
const DNS_TIMEOUT_NEGATIVE_TTL: Duration = Duration::from_secs(2);
/// stale-while-error の上限倍率(元TTLのこの倍までstaleエントリを使う)。
const DNS_STALE_FACTOR: u32 = 10;

/// 起動時に1度だけパースするネットワークポリシー。
/// check_url には Arc の参照として渡す(リクエストごとの再パースをしない)。
pub struct NetworkPolicy{
	/// RFC1918 プライベートレンジ(allowedが無ければ遮断)。
	ipv4_private_range: IpRange<Ipv4Net>,
	allowed_networks: Option<IpRange<Ipv4Net>>,
	blocked_networks: Option<IpRange<Ipv4Net>>,
	/// 小文字化済みの遮断ホスト集合。
	blocked_hosts: HashSet<String>,
}
impl NetworkPolicy{
	fn parse_ranges(list:&[String],label:&str)->Result<IpRange<Ipv4Net>,String>{
		let mut range=IpRange::new();
		for s in list{
			let net:Ipv4Net=s.parse().map_err(|e|format!("{} の不正な値 {:?}: {}",label,s,e))?;
			range.add(net);
		}
		range.simplify();
		Ok(range)
	}
	pub fn from_config(config:&ConfigFile)->Result<Self,String>{
		let ipv4_private_range=Self::parse_ranges(
			&["10.0.0.0/8".to_owned(),"172.16.0.0/12".to_owned(),"192.168.0.0/16".to_owned()],
			"builtin private range",
		)?;
		let allowed_networks=match &config.allowed_networks{
			Some(list)=>Some(Self::parse_ranges(list,"allowed_networks")?),
			None=>None,
		};
		let blocked_networks=match &config.blocked_networks{
			Some(list)=>Some(Self::parse_ranges(list,"blocked_networks")?),
			None=>None,
		};
		let blocked_hosts=config.blocked_hosts.as_ref()
			.map(|hosts|hosts.iter().map(|h|h.to_lowercase()).collect())
			.unwrap_or_default();
		Ok(Self{ipv4_private_range,allowed_networks,blocked_networks,blocked_hosts})
	}
	/// IPv4アドレスの遮断判定。allowed_networks は遮断より優先。
	fn check_ipv4(&self,ip:&std::net::Ipv4Addr)->Result<(),String>{
		if let Some(block)=&self.blocked_networks{
			if block.contains(ip){
				return Err("Blocked address".to_owned());
			}
		}
		if self.ipv4_private_range.contains(ip){
			let allow=self.allowed_networks.as_ref().is_some_and(|a|a.contains(ip));
			if !allow{
				return Err("Blocked address".to_owned());
			}
		}
		Ok(())
	}
}

struct DnsCacheEntry{
	resolved_at: Instant,
	ips: Result<Vec<IpAddr>,String>,
	/// タイムアウト由来の失敗かどうか(ネガティブキャッシュTTLの選別に使う)。
	is_timeout: bool,
}

/// DNS解決結果のキャッシュ状態(ログ用)。
#[derive(Clone,Copy)]
pub enum DnsHitStatus{
	/// キャッシュヒット(TTL内)。
	Hit,
	/// TTL切れだが再解決失敗のため期限切れの値を使用。
	Stale,
	/// キャッシュミス(新規解決)。
	Miss,
}
impl std::fmt::Display for DnsHitStatus{
	fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{
		match self{
			DnsHitStatus::Hit=>write!(f,"hit"),
			DnsHitStatus::Stale=>write!(f,"stale"),
			DnsHitStatus::Miss=>write!(f,"miss"),
		}
	}
}

/// DNS singleflight の broadcast で送受信する型。
type DnsResult = Result<Vec<IpAddr>,String>;

/// host -> 解決済みIPの簡易キャッシュ(TTL付き・上限付き・singleflight・stale-while-error)。
pub struct DnsCache{
	inner: RwLock<HashMap<String,DnsCacheEntry>>,
	ttl: Duration,
	negative_ttl: Duration,
	dns_timeout: Duration,
	max_entries: usize,
	/// singleflight: 進行中のDNS解決。同一ホストへの並列lookup_hostを1本に束ねる。
	inflight: Mutex<HashMap<String,tokio::sync::broadcast::Sender<DnsResult>>>,
}
impl DnsCache{
	pub fn new(ttl:Duration,negative_ttl:Duration,dns_timeout:Duration,max_entries:usize)->Self{
		Self{
			inner:RwLock::new(HashMap::new()),
			ttl,negative_ttl,dns_timeout,max_entries,
			inflight:Mutex::new(HashMap::new()),
		}
	}
	/// エントリのTTLを返す。タイムアウト由来の失敗は短いTTL(2秒)、確定的失敗はnegative_ttl。
	fn entry_ttl(&self,entry:&DnsCacheEntry)->Duration{
		match &entry.ips{
			Ok(_)=>self.ttl,
			Err(_)=>{
				if entry.is_timeout{
					DNS_TIMEOUT_NEGATIVE_TTL
				}else{
					self.negative_ttl
				}
			}
		}
	}
	/// stale-while-error の上限判定。成功エントリが元TTLのDNS_STALE_FACTOR倍以内なら stale として使える。
	fn is_stale_usable(&self,entry:&DnsCacheEntry)->bool{
		entry.ips.is_ok() && entry.resolved_at.elapsed() < self.ttl * DNS_STALE_FACTOR
	}

	/// DNSキャッシュのエントリ数を返す(統計ログ用)。
	pub(crate) fn len(&self) -> usize {
		self.inner.try_read().map(|m| m.len()).unwrap_or(0)
	}

	/// hostを非同期解決する。TTL内はキャッシュを返す。singleflight付き。
	/// タイムアウト時は1回リトライする。
	pub async fn resolve(&self,host:&str,port:u16)->Result<(Vec<IpAddr>,DnsHitStatus),String>{
		// --- キャッシュヒット判定 ---
		{
			let map=self.inner.read().await;
			if let Some(entry)=map.get(host){
				let ttl=self.entry_ttl(entry);
				if entry.resolved_at.elapsed()<ttl{
					match &entry.ips{
						Ok(ips)=>return Ok((ips.clone(),DnsHitStatus::Hit)),
						Err(e)=>return Err(e.clone()),
					}
				}
			}
		}
		// --- singleflight: 既に進行中なら合流、そうでなければ自分が処理開始 ---
		let rx_opt=self.try_subscribe(host);
		if let Some(mut rx)=rx_opt{
			if let Ok(result)=rx.recv().await{
				match result{
					Ok(ips)=>return Ok((ips,DnsHitStatus::Miss)),
					Err(e)=>return self.try_stale_or_err(host,e).await,
				}
			}
			// broadcast側がdropされた→自分で解決にフォールスルー
		}
		// --- singleflight: 処理開始を登録(race check込み) ---
		let tx=match self.register_or_subscribe(host){
			Err(mut rx)=>{
				// 別タスクが先に登録した→合流
				if let Ok(result)=rx.recv().await{
					match result{
						Ok(ips)=>return Ok((ips,DnsHitStatus::Miss)),
						Err(e)=>return self.try_stale_or_err(host,e).await,
					}
				}
				// broadcast側がdropされた→自分で解決にフォールスルー(txを登録)
				self.force_register(host)
			},
			Ok(tx)=>tx,
		};
		// --- 実際のDNS解決(リトライ1回付き) ---
		let result=self.do_lookup(host,port).await;
		let result=match &result{
			Err(e) if e.contains("timeout")=>{
				// タイムアウト→1回リトライ
				self.do_lookup(host,port).await
			},
			_=>result,
		};
		let is_timeout=matches!(&result,Err(e) if e.contains("timeout"));
		// --- キャッシュ格納 ---
		{
			let mut map=self.inner.write().await;
			if map.len()>=self.max_entries && !map.contains_key(host){
				map.retain(|_,e|{
					let ttl=self.entry_ttl(e);
					// stale-while-error用に成功エントリは長めに保持
					if e.ips.is_ok(){
						e.resolved_at.elapsed() < self.ttl * DNS_STALE_FACTOR
					}else{
						e.resolved_at.elapsed() < ttl
					}
				});
				if map.len()>=self.max_entries{
					if let Some(k)=map.keys().next().cloned(){
						map.remove(&k);
					}
				}
			}
			map.insert(host.to_owned(),DnsCacheEntry{
				resolved_at:Instant::now(),
				ips:result.clone(),
				is_timeout,
			});
		}
		// --- singleflight: 結果を通知して inflight から削除 ---
		let _ = tx.send(result.clone());
		{
			let mut inflight=self.inflight.lock().unwrap_or_else(|e|e.into_inner());
			inflight.remove(host);
		}
		// --- 結果を返す(失敗時はstaleを試す) ---
		match result{
			Ok(addrs)=>Ok((addrs,DnsHitStatus::Miss)),
			Err(e)=>self.try_stale_or_err(host,e).await,
		}
	}

	/// 1回のlookup_host実行(タイムアウト付き)。
	async fn do_lookup(&self,host:&str,port:u16)->Result<Vec<IpAddr>,String>{
		let host_port=format!("{}:{}",host,port);
		match tokio::time::timeout(self.dns_timeout,tokio::net::lookup_host(host_port)).await{
			Ok(Ok(iter))=>{
				let addrs:Vec<_>=iter.map(|sa|sa.ip()).collect();
				if addrs.is_empty(){
					Err("dns lookup: no address".to_owned())
				}else{
					Ok(addrs)
				}
			},
			Ok(Err(e))=>Err(format!("dns lookup error: {}",e)),
			Err(_)=>Err("dns lookup timeout".to_owned()),
		}
	}

	/// singleflight: 進行中の解決があればsubscribeする(同期・MutexGuardがawaitをまたがない)。
	fn try_subscribe(&self,host:&str)->Option<tokio::sync::broadcast::Receiver<DnsResult>>{
		let inflight=self.inflight.lock().unwrap_or_else(|e|e.into_inner());
		inflight.get(host).map(|tx|tx.subscribe())
	}
	/// singleflight: 自分が処理開始を登録する。既に別タスクが登録済みならそのrxを返す。
	fn register_or_subscribe(&self,host:&str)->Result<tokio::sync::broadcast::Sender<DnsResult>,tokio::sync::broadcast::Receiver<DnsResult>>{
		let mut inflight=self.inflight.lock().unwrap_or_else(|e|e.into_inner());
		if let Some(tx)=inflight.get(host){
			Err(tx.subscribe())
		}else{
			let (tx,_)=tokio::sync::broadcast::channel(1);
			inflight.insert(host.to_owned(),tx.clone());
			Ok(tx)
		}
	}
	/// singleflight: 強制的にtxを登録する(raceで合流が全て失敗した場合のフォールバック)。
	fn force_register(&self,host:&str)->tokio::sync::broadcast::Sender<DnsResult>{
		let mut inflight=self.inflight.lock().unwrap_or_else(|e|e.into_inner());
		let (tx,_)=tokio::sync::broadcast::channel(1);
		inflight.insert(host.to_owned(),tx.clone());
		tx
	}

	/// 解決失敗時にstaleエントリがあればそれを返す。なければエラー。
	async fn try_stale_or_err(&self,host:&str,err:String)->Result<(Vec<IpAddr>,DnsHitStatus),String>{
		let map=self.inner.read().await;
		if let Some(entry)=map.get(host){
			if self.is_stale_usable(entry){
				if let Ok(ips)=&entry.ips{
					return Ok((ips.clone(),DnsHitStatus::Stale));
				}
			}
		}
		Err(err)
	}
}
/// reqwest::dns::Resolve の実装ラッパー。Arc<DnsCache> を保持し、
/// check_url と実フェッチの DNS 解決を一本化する。
struct SharedDnsResolver(Arc<DnsCache>);
impl reqwest::dns::Resolve for SharedDnsResolver{
	fn resolve(&self,name:reqwest::dns::Name)->reqwest::dns::Resolving{
		let cache=self.0.clone();
		let host=name.as_str().to_owned();
		Box::pin(async move{
			let result=cache.resolve(&host,0).await;
			match result{
				Ok((ips,_hit))=>{
					let addrs:Vec<std::net::SocketAddr>=ips.into_iter().map(|ip|std::net::SocketAddr::new(ip,0)).collect();
					let addrs:reqwest::dns::Addrs=Box::new(addrs.into_iter());
					Ok(addrs)
				},
				Err(e)=>{
					Err(std::io::Error::other(e).into())
				},
			}
		})
	}
}

async fn check_url(policy:&NetworkPolicy,dns_cache:&DnsCache,url:impl AsRef<str>)->Result<(DnsHitStatus,u16,u16),String>{
	let u=reqwest::Url::from_str(url.as_ref()).map_err(|e|format!("{:?}",e))?;
	match u.scheme().to_lowercase().as_str(){
		"http"|"https"=>{},
		scheme=>return Err(format!("scheme: {}",scheme))
	}
	let host=u.host_str().ok_or_else(||"no host".to_owned())?;
	if policy.blocked_hosts.contains(&host.to_lowercase()){
		return Err("Blocked address".to_owned());
	}
	let port=u.port_or_known_default().ok_or_else(||"no port".to_owned())?;
	// 同期DNS(to_socket_addrs)を廃止し、非同期解決+独自タイムアウトに置き換え。
	let (ips,dns_cache_hit)=dns_cache.resolve(host,port).await?;
	let mut v4_count:u16=0;
	let mut v6_count:u16=0;
	for ip in &ips{
		match ip{
			IpAddr::V4(_)=>{ v4_count+=1; },
			IpAddr::V6(_)=>{ v6_count+=1; },
		}
	}
	for ip in ips{
		match ip{
			IpAddr::V4(v4)=>{
				policy.check_ipv4(&v4)?;
			},
			IpAddr::V6(v6)=>{
				if v6.is_multicast()||v6.is_unicast_link_local(){
					return Err("Blocked address".to_owned());
				}
			},
		}
	}
	Ok((dns_cache_hit,v4_count,v6_count))
}
/// 1リクエストの各フェーズ所要時間と付随情報。Arc<Mutex<>> で get_file と
/// RequestContext(spawn_blocking 内も含む)で共有する。
#[derive(Default)]
struct PhaseTimings{
	dns_hit:Option<DnsHitStatus>,
	cache_result:Option<CacheResult>,
	passthrough:bool,
	check:Duration,
	/// permit待ちの合計。
	wait:Duration,
	/// TTFB(送信開始〜レスポンスヘッダ受信)。
	ttfb:Duration,
	/// ボディ受信時間。
	body:Duration,
	decode:Duration,
	encode:Duration,
	anim:bool,
	anim_frames:u32,
	anim_in_bytes:usize,
	anim_out_bytes:usize,
	/// fetchエラーの分類+詳細(正常時はNone)。
	fetch_err:Option<String>,
	/// DNS解決結果のIPv4アドレス数。
	dns_v4:u16,
	/// DNS解決結果のIPv6アドレス数。
	dns_v6:u16,
	/// 接続リトライが実行された。
	retried:bool,
	/// レスポンスのHTTPバージョン(例: "1.1", "2")。
	http_version:Option<&'static str>,
}
/// 計測対象フェーズ(decode/encode は複数の return を持つ関数が多いため Drop で計測する)。
pub(crate) enum Phase{
	Decode,
	Encode,
}
/// スコープ離脱時に経過時間を該当フェーズへ加算するガード。
/// self を借用せず Arc を複製して保持するため、&mut self のメソッド内でも使える。
pub(crate) struct PhaseGuard{
	timings:Arc<Mutex<PhaseTimings>>,
	start:Instant,
	phase:Phase,
}
impl Drop for PhaseGuard{
	fn drop(&mut self){
		if let Ok(mut t)=self.timings.lock(){
			let e=self.start.elapsed();
			match self.phase{
				Phase::Decode=>t.decode+=e,
				Phase::Encode=>t.encode+=e,
			}
		}
	}
}
/// アクセスログ用に、消費(move)される前の RequestParams から必要な値だけ控えておく。
struct ReqSummary{
	url:String,
	is_static:bool,
	emoji:bool,
	avatar:bool,
	preview:bool,
	badge:bool,
	fallback:bool,
}
impl ReqSummary{
	fn new(q:&RequestParams)->Self{
		Self{
			url:q.url.clone(),
			is_static:q.r#static.is_some(),
			emoji:q.emoji.is_some(),
			avatar:q.avatar.is_some(),
			preview:q.preview.is_some(),
			badge:q.badge.is_some(),
			fallback:q.fallback.is_some(),
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
fn emit_summary(cfg:&ConfigFile,s:&ReqSummary,t:&PhaseTimings,status:u16,has_error:bool,stats:&GlobalStats){
	stats.requests.fetch_add(1, Ordering::Relaxed);
	if status >= 400 || has_error {
		stats.errors.fetch_add(1, Ordering::Relaxed);
	}
	match t.cache_result {
		Some(CacheResult::Hit | CacheResult::Joined) => { stats.cache_hits.fetch_add(1, Ordering::Relaxed); },
		Some(CacheResult::Miss) => { stats.cache_misses.fetch_add(1, Ordering::Relaxed); },
		_ => {},
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
	let check_ms=t.check.as_millis() as u64;
	let wait_ms=t.wait.as_millis() as u64;
	let ttfb_ms=t.ttfb.as_millis() as u64;
	let body_ms=t.body.as_millis() as u64;
	let decode_ms=t.decode.as_millis() as u64;
	let encode_ms=t.encode.as_millis() as u64;
	let total_ms=check_ms+wait_ms+ttfb_ms+body_ms+decode_ms+encode_ms;
	let mut params=String::new();
	if s.is_static{params.push_str("static,");}
	if s.emoji{params.push_str("emoji,");}
	if s.avatar{params.push_str("avatar,");}
	if s.preview{params.push_str("preview,");}
	if s.badge{params.push_str("badge,");}
	if s.fallback{params.push_str("fallback,");}
	let dns_str=t.dns_hit.map(|d|d.to_string()).unwrap_or_else(||"-".to_owned());
	let cache_str=t.cache_result.map(|c|c.to_string()).unwrap_or_else(||"-".to_owned());
	let fetch_err_str=t.fetch_err.as_deref().unwrap_or("-");
	let http_str=t.http_version.unwrap_or("-");
	let fast=status<400 && !has_error && total_ms<cfg.slow_log_ms;
	if fast{
		tracing::debug!(
			url=%s.url,params=%params,dns_hit=%dns_str,cache=%cache_str,
			passthrough=t.passthrough,fetch_err=%fetch_err_str,retried=t.retried,
			http=%http_str,dns_v4=t.dns_v4,dns_v6=t.dns_v6,
			check_ms,wait_ms,ttfb_ms,body_ms,decode_ms,encode_ms,
			status=status as u64,error=has_error,anim=t.anim,
			anim_frames=t.anim_frames as u64,
			anim_in=t.anim_in_bytes as u64,anim_out=t.anim_out_bytes as u64,
			"request"
		);
	}else{
		tracing::info!(
			url=%s.url,params=%params,dns_hit=%dns_str,cache=%cache_str,
			passthrough=t.passthrough,fetch_err=%fetch_err_str,retried=t.retried,
			http=%http_str,dns_v4=t.dns_v4,dns_v6=t.dns_v6,
			check_ms,wait_ms,ttfb_ms,body_ms,decode_ms,encode_ms,
			status=status as u64,error=has_error,anim=t.anim,
			anim_frames=t.anim_frames as u64,
			anim_in=t.anim_in_bytes as u64,anim_out=t.anim_out_bytes as u64,
			"request"
		);
	}
}
async fn get_file(
	_path:Option<axum::extract::Path<String>>,
	client_headers:axum::http::HeaderMap,
	(client,config,dummy_img,fontdb,encode_semaphore,network_policy,dns_cache,response_cache,download_semaphore,buffer_budget,global_stats):AppState,
	axum::extract::Query(q):axum::extract::Query<RequestParams>,
)->Result<(axum::http::StatusCode,HeaderMap,axum::body::Body),axum::response::Response>{
	let timings=Arc::new(Mutex::new(PhaseTimings::default()));
	let summary=ReqSummary::new(&q);

	// Range リクエストはキャッシュ対象外
	let has_range=client_headers.contains_key("Range");

	// avif 判定(キャッシュキー生成にも使う)
	let mut is_accept_avif=false;
	if config.encode_avif{
		if let Some(accept)=client_headers.get("Accept"){
			if let Ok(accept)=std::str::from_utf8(accept.as_bytes()){
				for e in accept.split(","){
					if e.trim()=="image/avif"{
						is_accept_avif=true;
					}
				}
			}
		}
	}

	let cache_key=CacheKey{
		url:q.url.clone(),
		is_static:q.r#static.is_some(),
		emoji:q.emoji.is_some(),
		avatar:q.avatar.is_some(),
		preview:q.preview.is_some(),
		badge:q.badge.is_some(),
		accept_avif:is_accept_avif,
	};

	// --- キャッシュヒット ---
	if !has_range {
		if let Some(cached)=response_cache.get(&cache_key){
			if let Ok(mut t)=timings.lock(){
				t.cache_result=Some(CacheResult::Hit);
			}
			let mut headers=HeaderMap::new();
			if let Some(ct)=&cached.content_type{
				if let Ok(v)=ct.parse(){ headers.append("Content-Type",v); }
			}
			if let Some(cd)=&cached.content_disposition{
				if let Ok(v)=cd.parse(){ headers.append("Content-Disposition",v); }
			}
			if let Some(cc)=&cached.cache_control{
				if let Ok(v)=cc.parse(){ headers.append("Cache-Control",v); }
			}
			if config.encode_avif{
				headers.append("Vary","Accept,Range".parse().unwrap());
			}
			for line in config.append_headers.iter(){
				if let Some(idx)=line.find(":"){
					if idx+1>=line.len(){ continue; }
					if let Ok(k)=axum::http::HeaderName::from_str(&line[0..idx]){
						if let Ok(v)=line[idx+1..].parse(){
							headers.append(k,v);
						}
					}
				}
			}
			if let Ok(t)=timings.lock(){
				emit_summary(&config,&summary,&t,cached.status,false,&global_stats);
			}
			let status=axum::http::StatusCode::from_u16(cached.status)
				.unwrap_or(axum::http::StatusCode::OK);
			return Err((status,headers,cached.body).into_response());
		}
	}

	// --- singleflight: 合流 ---
	if !has_range {
		if let Some(mut rx)=response_cache.try_join(&cache_key){
			if let Ok(mut t)=timings.lock(){
				t.cache_result=Some(CacheResult::Joined);
			}
			match rx.recv().await{
				Ok(Some(cached))=>{
					let mut headers=HeaderMap::new();
					if let Some(ct)=&cached.content_type{
						if let Ok(v)=ct.parse(){ headers.append("Content-Type",v); }
					}
					if let Some(cd)=&cached.content_disposition{
						if let Ok(v)=cd.parse(){ headers.append("Content-Disposition",v); }
					}
					if let Some(cc)=&cached.cache_control{
						if let Ok(v)=cc.parse(){ headers.append("Cache-Control",v); }
					}
					if config.encode_avif{
						headers.append("Vary","Accept,Range".parse().unwrap());
					}
					for line in config.append_headers.iter(){
						if let Some(idx)=line.find(":"){
							if idx+1>=line.len(){ continue; }
							if let Ok(k)=axum::http::HeaderName::from_str(&line[0..idx]){
								if let Ok(v)=line[idx+1..].parse(){
									headers.append(k,v);
								}
							}
						}
					}
					if let Ok(t)=timings.lock(){
						emit_summary(&config,&summary,&t,cached.status,false,&global_stats);
					}
					let status=axum::http::StatusCode::from_u16(cached.status)
						.unwrap_or(axum::http::StatusCode::OK);
					return Err((status,headers,cached.body).into_response());
				},
				_=>{
					// 元処理が失敗 → 自分でフォールスルーして処理する
				}
			}
		}
	}

	// --- singleflight: 処理開始を登録 ---
	let mut flight_guard=if !has_range{
		response_cache.start_flight(cache_key.clone())
	}else{
		None
	};

	if let Ok(mut t)=timings.lock(){
		if t.cache_result.is_none(){
			t.cache_result=Some(if has_range{ CacheResult::Bypass }else{ CacheResult::Miss });
		}
	}

	let mut headers=HeaderMap::new();
	if let Ok(url)=q.url.parse(){
		headers.append("X-Remote-Url",url);
	}
	if config.encode_avif{
		headers.append("Vary","Accept,Range".parse().unwrap());
	}
	let check_start=Instant::now();
	match check_url(&network_policy,&dns_cache,&q.url).await{
		Ok((hit,v4_count,v6_count))=>{
			if let Ok(mut t)=timings.lock(){
				t.check=check_start.elapsed();
				t.dns_hit=Some(hit);
				t.dns_v4=v4_count;
				t.dns_v6=v6_count;
			}
		},
		Err(s)=>{
			if let Ok(mut t)=timings.lock(){
				t.check=check_start.elapsed();
				// DNS解決失敗の場合のみ fetch_err に記録(ポリシー拒否は除外)
				if !s.contains("Blocked") && !s.contains("Private") && !s.contains("Loopback") {
					t.fetch_err=Some(format!("dns:{}",s.chars().take(60).collect::<String>()));
				}
			}
			let has_error=if let Ok(v)=s.parse(){
				headers.append("X-Proxy-Error",v);
				true
			}else{
				false
			};
			let is_fallback=q.fallback.is_some();
			if let Ok(t)=timings.lock(){
				emit_summary(&config,&summary,&t,if is_fallback{200}else{400},has_error,&global_stats);
			}
			if is_fallback{
				headers.append("Cache-Control","no-store".parse().unwrap());
				headers.append("Content-Type","image/png".parse().unwrap());
				return Err((axum::http::StatusCode::OK,headers,(*dummy_img).clone()).into_response());
			}
			headers.append("Cache-Control","no-store".parse().unwrap());
			return Err((axum::http::StatusCode::BAD_REQUEST,headers).into_response())
		}
	};

	// --- ダウンロードpermit取得(取得順序: DL permit → バイト予算 → CPU permit) ---
	let wait_start=Instant::now();
	let dl_permit = download_semaphore.acquire_owned().await.map_err(|_| {
		let mut h=HeaderMap::new();
		h.append("X-Proxy-Error","DownloadSemaphoreError".parse().unwrap());
		(axum::http::StatusCode::SERVICE_UNAVAILABLE,h).into_response()
	})?;
	if let Ok(mut t)=timings.lock(){
		t.wait+=wait_start.elapsed();
	}
	let send_start=Instant::now();
	let build_req=||{
		let req=client.get(&q.url);
		let remaining=config.timeout.saturating_sub(send_start.elapsed().as_millis() as u64);
		let req=req.timeout(Duration::from_millis(remaining.max(1)));
		let req=req.header("User-Agent",config.user_agent.clone());
		if let Some(range)=client_headers.get("Range"){
			req.header("Range",range.as_bytes())
		}else{
			req
		}
	};
	let resp=match build_req().send().await{
		Ok(resp) => {
			if let Ok(mut t)=timings.lock(){
				t.ttfb=send_start.elapsed();
			}
			resp
		},
		Err(e) => {
			let first_err = classify_reqwest_error(&e);
			let is_connect_phase = e.is_connect() || e.is_timeout();
			// 接続段階の失敗かつRangeリクエスト以外かつ残り時間がある場合のみ1回リトライ
			let remaining_ms = config.timeout.saturating_sub(send_start.elapsed().as_millis() as u64);
			if is_connect_phase && !has_range && remaining_ms > config.fetch_retry_delay_ms {
				global_stats.retry_attempts.fetch_add(1, Ordering::Relaxed);
				tokio::time::sleep(Duration::from_millis(config.fetch_retry_delay_ms)).await;
				match build_req().send().await {
					Ok(resp) => {
						if let Ok(mut t)=timings.lock(){
							t.ttfb=send_start.elapsed();
							t.retried=true;
						}
						resp
					},
					Err(e2) => {
						let fetch_err = classify_reqwest_error(&e2);
						let is_fallback=q.fallback.is_some();
						if let Ok(mut t)=timings.lock(){
							t.ttfb=send_start.elapsed();
							t.fetch_err=Some(fetch_err.clone());
							t.retried=true;
						}
						headers.append("X-Proxy-Error",format!("Send:{}",fetch_err).parse().unwrap_or_else(|_|"Send:unknown".parse().unwrap()));
						if let Ok(t)=timings.lock(){
							emit_summary(&config,&summary,&t,if is_fallback{200}else{400},true,&global_stats);
						}
						if is_fallback{
							headers.append("Cache-Control","no-store".parse().unwrap());
							headers.append("Content-Type","image/png".parse().unwrap());
							return Err((axum::http::StatusCode::OK,headers,(*dummy_img).clone()).into_response());
						}
						headers.append("Cache-Control","no-store".parse().unwrap());
						return Err((axum::http::StatusCode::BAD_REQUEST,headers).into_response())
					}
				}
			} else {
				// リトライ不可(接続段階以外 or 残り時間不足 or Rangeリクエスト)
				let is_fallback=q.fallback.is_some();
				if let Ok(mut t)=timings.lock(){
					t.ttfb=send_start.elapsed();
					t.fetch_err=Some(first_err.clone());
				}
				headers.append("X-Proxy-Error",format!("Send:{}",first_err).parse().unwrap_or_else(|_|"Send:unknown".parse().unwrap()));
				if let Ok(t)=timings.lock(){
					emit_summary(&config,&summary,&t,if is_fallback{200}else{400},true,&global_stats);
				}
				if is_fallback{
					headers.append("Cache-Control","no-store".parse().unwrap());
					headers.append("Content-Type","image/png".parse().unwrap());
					return Err((axum::http::StatusCode::OK,headers,(*dummy_img).clone()).into_response());
				}
				headers.append("Cache-Control","no-store".parse().unwrap());
				return Err((axum::http::StatusCode::BAD_REQUEST,headers).into_response())
			}
		}
	};
	fn add_remote_header(key:&'static str,headers:&mut HeaderMap,remote_headers:&reqwest::header::HeaderMap){
		for v in remote_headers.get_all(key){
			headers.append(key,String::from_utf8_lossy(v.as_bytes()).parse().unwrap());
		}
	}
	// HTTPバージョンを記録
	{
		let ver = match resp.version() {
			reqwest::Version::HTTP_2 => { global_stats.http2_responses.fetch_add(1, Ordering::Relaxed); "2" },
			reqwest::Version::HTTP_11 => { global_stats.http1_responses.fetch_add(1, Ordering::Relaxed); "1.1" },
			reqwest::Version::HTTP_10 => { global_stats.http1_responses.fetch_add(1, Ordering::Relaxed); "1.0" },
			_ => "?",
		};
		if let Ok(mut t)=timings.lock(){ t.http_version=Some(ver); }
	}
	let remote_headers=resp.headers();
	add_remote_header("Content-Disposition",&mut headers,remote_headers);
	add_remote_header("Content-Type",&mut headers,remote_headers);
	let is_img=if let Some(media)=headers.get("Content-Type"){
		let s=String::from_utf8_lossy(media.as_bytes());
		s.starts_with("image/")
	}else{
		false
	};
	if !is_img{
		add_remote_header("Content-Length",&mut headers,remote_headers);
		add_remote_header("Content-Range",&mut headers,remote_headers);
		add_remote_header("Accept-Ranges",&mut headers,remote_headers);
	}
	headers.append("Cache-Control","no-store".parse().unwrap());
	for line in config.append_headers.iter(){
		if let Some(idx)=line.find(":"){
			if idx+1>=line.len(){
				continue;
			}
			if let Ok(k)=axum::http::HeaderName::from_str(&line[0..idx]){
				if let Ok(v)=line[idx+1..].parse(){
					headers.append(k,v);
				}
			}
		}
	}
	let result=RequestContext{
		is_accept_avif,
		headers,
		parms:q,
		src_bytes:Vec::new(),
		config:config.clone(),
		codec:Err(None),
		dummy_img,
		fontdb,
		encode_semaphore,
		buffer_budget,
		dl_permit: Some(dl_permit),
		timings:timings.clone(),
		response_cache:response_cache.clone(),
		cache_key:cache_key.clone(),
	}.encode(resp,is_img).await;

	// --- singleflight 完了通知 ---
	if let Some(ref mut guard)=flight_guard{
		// encode 結果が 200 ならキャッシュから取得してflight完了
		let entry=response_cache.get(&cache_key);
		response_cache.complete_flight(guard,entry);
	}

	let (status,has_error)=match &result{
		Ok((sc,h,_))=>(sc.as_u16(),h.contains_key("X-Proxy-Error")),
		Err(r)=>(r.status().as_u16(),r.headers().contains_key("X-Proxy-Error")),
	};
	if let Ok(t)=timings.lock(){
		emit_summary(&config,&summary,&t,status,has_error,&global_stats);
	}
	result
}
struct RequestContext{
	is_accept_avif:bool,
	headers:HeaderMap,
	parms:RequestParams,
	src_bytes:Vec<u8>,
	config:Arc<ConfigFile>,
	codec:Result<image::ImageFormat,Option<image::ImageError>>,
	dummy_img:Arc<Vec<u8>>,
	fontdb:Arc<resvg::usvg::fontdb::Database>,
	encode_semaphore: Arc<Semaphore>,
	buffer_budget: Arc<Semaphore>,
	dl_permit: Option<tokio::sync::OwnedSemaphorePermit>,
	timings: Arc<Mutex<PhaseTimings>>,
	response_cache: Arc<ResponseCache>,
	cache_key: CacheKey,
}
impl RequestContext{
	/// フェーズ計測ガードを生成する(Arcを複製して保持するため self を借用し続けない)。
	pub(crate) fn phase_guard(&self,phase:Phase)->PhaseGuard{
		PhaseGuard{timings:self.timings.clone(),start:Instant::now(),phase}
	}
	/// ボディ受信完了時の計測を記録する。
	pub(crate) fn mark_body_done(&self,body_duration:Duration){
		if let Ok(mut t)=self.timings.lock(){
			t.body=body_duration;
		}
	}
	/// encode_anim のフレーム数・入出力バイト数を記録する。
	pub(crate) fn record_anim(&self,frames:u32,in_bytes:usize,out_bytes:usize){
		if let Ok(mut t)=self.timings.lock(){
			t.anim=true;
			t.anim_frames=frames;
			t.anim_in_bytes=in_bytes;
			t.anim_out_bytes=out_bytes;
		}
	}
	/// 成功レスポンスをキャッシュに格納する。
	pub(crate) fn cache_response(&self,status:u16,headers:&HeaderMap,body:&[u8]){
		let ct=headers.get("Content-Type").and_then(|v|v.to_str().ok()).map(|s|s.to_owned());
		let cd=headers.get("Content-Disposition").and_then(|v|v.to_str().ok()).map(|s|s.to_owned());
		let cc=headers.get("Cache-Control").and_then(|v|v.to_str().ok()).map(|s|s.to_owned());
		let entry=cache::CacheEntry::new(status,ct,cd,cc,body.to_vec());
		self.response_cache.put(self.cache_key.clone(),entry);
	}
}
impl RequestContext{
	pub fn disposition_ext(headers:&mut HeaderMap,ext:&str){
		let k="Content-Disposition";
		if let Some(cd)=headers.get(k){
			let s=std::str::from_utf8(cd.as_bytes());
			if let Ok(s)=s{
				let cd=mailparse::parse_content_disposition(s);
				let cd_utf8=cd.params.get("filename*");
				let mut name=None;
				if let Some(cd_utf8)=cd_utf8{
					let cd_utf8=cd_utf8.to_uppercase();
					if cd_utf8.starts_with("UTF-8''")&&cd_utf8.len()>7{
						name=urlencoding::decode(&cd_utf8[7..]).map(|s|s.to_string()).ok();
					}
				}
				if name.is_none(){
					if let Some(filename)=cd.params.get("filename"){
						let m_filename=format!("_:{}",filename);
						let parsed=mailparse::parse_header(m_filename.as_bytes());
						if let Ok((parsed,_))=&parsed{
							name=Some(parsed.get_value());
						}else if !cd.params.contains_key("name"){
							name=Some(filename.clone());
						}
					}
				}
				let name=name.unwrap_or_else(||cd.params.get("name").cloned().unwrap_or_else(||"null".to_owned()));
				let mut name_arr:Vec<&str>=name.split('.').collect();
				name_arr.pop();
				let name=name_arr.join(".")+ext;
				let name=urlencoding::encode(&name);
				let content_disposition=format!("inline; filename=\"{}\";filename*=UTF-8''{};",name,name);
				headers.remove(k);
				headers.append(k,content_disposition.parse().unwrap());
			}
		}
	}
}
impl RequestContext{
	async fn encode(mut self,resp: reqwest::Response,mut is_img:bool)->Result<(axum::http::StatusCode,HeaderMap,axum::body::Body),axum::response::Response>{
		let mut is_svg=false;
		let mut content_type=None;
		if let Some(media)=self.headers.get("Content-Type"){
			let s=String::from_utf8_lossy(media.as_bytes());
			if s.as_ref()=="image/svg+xml"{
				is_svg=true;
			}else{
				content_type=Some(s);
			}
		}
		let status=resp.status();
		let resp=PreDataStream::new(resp).await;
		if let Some(Ok(head))=resp.head.as_ref(){
			//utf8にパースできて空白文字を削除した後の先頭部分が<svgの場合はsvg
			if std::str::from_utf8(head).map(|s|s.trim().starts_with("<svg")).unwrap_or(false){
				is_svg=true;
			}else{
				self.codec=image::guess_format(head).map_err(Some);
				if self.codec.is_err(){
					if let Some(content_type)=content_type.as_ref(){
						match content_type.as_ref(){
							"image/x-targa"|"image/x-tga"=>self.codec=Ok(image::ImageFormat::Tga),
							_=>{}
						}
					}
					if head.starts_with(&[0xFF,0x0A])||head.starts_with(&[0x00,0x00,0x00,0x0C,0x4A,0x58,0x4C,0x20,0x0D,0x0A,0x87,0x0A]){
						is_img=true;
						self.headers.remove("Content-Type");
						self.headers.append("Content-Type", "image/jxl".parse().unwrap());
					}
					if head.starts_with(&[0xFF,0x4F,0xFF,0x51])||head.starts_with(&[0x00,0x00,0x00,0x0C,0x6A,0x50,0x20,0x20,0x0D,0x0A,0x87,0x0A]){
						is_img=true;
						self.headers.remove("Content-Type");
						self.headers.append("Content-Type", "image/jp2".parse().unwrap());
					}
					if head.starts_with(&[0x49,0x49,0xBC]){
						is_img=true;
						self.headers.remove("Content-Type");
						self.headers.append("Content-Type", "image/jxr".parse().unwrap());
					}
				}
			}
		}
		if is_svg{
			// バイト予算を取得(仮予約8MB)。Content-Length不明のため固定値。
			let budget_bytes=8*1024*1024_u32;
			let budget_sem=self.buffer_budget.clone();
			let wait_start=Instant::now();
			let _budget_permit = budget_sem.acquire_many(budget_bytes).await.map_err(|_| {
				let mut h=self.headers.clone();
				h.append("X-Proxy-Error", "BufferBudgetError".parse().unwrap());
				(axum::http::StatusCode::SERVICE_UNAVAILABLE, h).into_response()
			})?;
			if let Ok(mut t)=self.timings.lock(){ t.wait+=wait_start.elapsed(); }
			self.load_all(resp).await?;
			drop(self.dl_permit.take()); // ダウンロード完了 → DL permit 解放
			// CPU permit を取得してエンコード
			let cpu_sem=self.encode_semaphore.clone();
			let wait_start=Instant::now();
			let _cpu_permit = cpu_sem.acquire().await.map_err(|_| {
				let mut h=self.headers.clone();
				h.append("X-Proxy-Error", "CpuSemaphoreError".parse().unwrap());
				(axum::http::StatusCode::SERVICE_UNAVAILABLE, h).into_response()
			})?;
			if let Ok(mut t)=self.timings.lock(){ t.wait+=wait_start.elapsed(); }
			if let Ok(img)=self.encode_svg(self.fontdb.clone()){
				self.headers.remove("Content-Length");
				self.headers.remove("Content-Range");
				self.headers.remove("Accept-Ranges");
				self.headers.remove("Cache-Control");
				self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
				return Err(self.response_img(img));
			}else{
				return Err((axum::http::StatusCode::OK,self.headers.clone(),self.src_bytes.clone()).into_response());
			}
		}else if is_img||self.codec.is_ok(){
			self.headers.remove("Content-Length");
			self.headers.remove("Content-Range");
			self.headers.remove("Accept-Ranges");
			let dummy_img=self.dummy_img.clone();
			let is_fallback=self.parms.fallback.is_some();
			let mut header=self.headers.clone();
			// バイト予算を取得(Content-Length or 仮予約8MB)
			let budget_hint=resp.content_length.unwrap_or(8*1024*1024);
			let budget_bytes=(budget_hint.min(self.config.max_size) as u32).max(1);
			let budget_sem=self.buffer_budget.clone();
			let wait_start=Instant::now();
			let _budget_permit = budget_sem.acquire_many(budget_bytes).await.map_err(|_| {
				header.append("X-Proxy-Error", "BufferBudgetError".parse().unwrap());
				(axum::http::StatusCode::SERVICE_UNAVAILABLE, header.clone()).into_response()
			})?;
			if let Ok(mut t)=self.timings.lock(){ t.wait+=wait_start.elapsed(); }
			self.load_all(resp).await?;
			drop(self.dl_permit.take()); // ダウンロード完了 → DL permit 解放
			// --- パススルー判定 ---
			// webp/png/jpeg/gif かつ badge/static 無 かつ寸法が目標以下かつサイズが閾値以下なら
			// デコード・再エンコードせず元バイト列をそのまま返す。
			if self.parms.badge.is_none() && self.parms.r#static.is_none() {
				if let Ok(codec)=&self.codec{
					let is_passthrough_format=matches!(codec,
						image::ImageFormat::WebP|image::ImageFormat::Png|
						image::ImageFormat::Jpeg|image::ImageFormat::Gif
					);
					if is_passthrough_format && self.src_bytes.len() <= self.config.passthrough_max_bytes as usize {
						// ヘッダ読みで寸法を取得(全デコードしない)
						let reader=image::ImageReader::new(std::io::Cursor::new(&self.src_bytes))
							.with_guessed_format();
						let dims=reader.ok().and_then(|r|r.into_dimensions().ok());
						if let Some((w,h))=dims{
							let (max_w,max_h)=self.image_size_hint();
							if w<=max_w && h<=max_h{
								// パススルー: 元バイト列をそのまま返す
								if let Ok(mut t)=self.timings.lock(){
									t.passthrough=true;
								}
								self.headers.remove("Cache-Control");
								self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
								// Content-Disposition の拡張子を元フォーマットに合わせる
								let ext=match codec{
									image::ImageFormat::WebP=>".webp",
									image::ImageFormat::Png=>".png",
									image::ImageFormat::Jpeg=>".jpeg",
									image::ImageFormat::Gif=>".gif",
									_=>".bin",
								};
								Self::disposition_ext(&mut self.headers,ext);
								let body=std::mem::take(&mut self.src_bytes);
								self.cache_response(200,&self.headers,&body);
								return Err((axum::http::StatusCode::OK,self.headers.clone(),body).into_response());
							}
						}
					}
				}
			}
			// CPU permit を取得してからエンコード
			let cpu_sem=self.encode_semaphore.clone();
			let wait_start=Instant::now();
			let _cpu_permit = cpu_sem.acquire().await.map_err(|_| {
				header.append("X-Proxy-Error", "CpuSemaphoreError".parse().unwrap());
				(axum::http::StatusCode::SERVICE_UNAVAILABLE, header.clone()).into_response()
			})?;
			if let Ok(mut t)=self.timings.lock(){ t.wait+=wait_start.elapsed(); }
			let mut handle=self;
			let resp=if let Ok(resp)=tokio::runtime::Handle::current().spawn_blocking(move ||{
				handle.encode_img()
			}).await{
				resp
			}else{
				header.append("X-Proxy-Error","ImageEncodeThread".parse().unwrap());
				return Err(if is_fallback{
					header.remove("Content-Type");
					header.append("Content-Type","image/png".parse().unwrap());
					(axum::http::StatusCode::OK,header,(*dummy_img).clone()).into_response()
				}else{
					(axum::http::StatusCode::INTERNAL_SERVER_ERROR,header).into_response()
				});
			};
			if is_fallback{
				return Err(if resp.status()==axum::http::StatusCode::OK{
					resp
				}else{
					header.remove("Content-Type");
					header.append("Content-Type","image/png".parse().unwrap());
					(axum::http::StatusCode::OK,header,(*dummy_img).clone()).into_response()
				});
			}
			return Err(resp);
		}
		if let Some(media)=self.headers.get("Content-Type"){
			let s=String::from_utf8_lossy(media.as_bytes());
			if crate::browsersafe::FILE_TYPE_BROWSERSAFE.contains(&s.as_ref()){

			}else{
				self.headers.remove("Content-Type");
				self.headers.append("Content-Type","octet-stream".parse().unwrap());
				Self::disposition_ext(&mut self.headers,".unknown");
			}
		}
		let body=axum::body::Body::from_stream(resp);
		if status.is_success(){
			// ストリーミングパス: ボディ計測は行わない(パススルー)
			self.headers.remove("Cache-Control");
			self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
			if status==reqwest::StatusCode::PARTIAL_CONTENT{
				Ok((axum::http::StatusCode::PARTIAL_CONTENT,self.headers.clone(),body))
			}else{
				Ok((axum::http::StatusCode::OK,self.headers.clone(),body))
			}
		}else{
			self.headers.append("X-Proxy-Error",format!("status:{}",status.as_u16()).parse().unwrap());
			Err(if self.parms.fallback.is_some(){
				self.headers.remove("Content-Type");
				self.headers.append("Content-Type","image/png".parse().unwrap());
				(axum::http::StatusCode::OK,self.headers.clone(),(*self.dummy_img).clone()).into_response()
			}else{
				let status=match status{
					reqwest::StatusCode::BAD_REQUEST=>axum::http::StatusCode::BAD_REQUEST,
					reqwest::StatusCode::FORBIDDEN=>axum::http::StatusCode::FORBIDDEN,
					reqwest::StatusCode::NOT_FOUND=>axum::http::StatusCode::NOT_FOUND,
					reqwest::StatusCode::REQUEST_TIMEOUT=>axum::http::StatusCode::GATEWAY_TIMEOUT,
					reqwest::StatusCode::GONE=>axum::http::StatusCode::GONE,
					reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS=>axum::http::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS,
					_=>axum::http::StatusCode::BAD_GATEWAY,
				};
				(status,self.headers.clone()).into_response()
			})
		}
	}
	async fn load_all(&mut self,mut resp: PreDataStream)->Result<(),axum::response::Response>{
		let len_hint=resp.content_length.unwrap_or(2048.min(self.config.max_size));
		if len_hint>self.config.max_size{
			self.headers.append("X-Proxy-Error",format!("lengthHint:{}>{}",len_hint,self.config.max_size).parse().unwrap());
			return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response())
		}
		// with_capacity の初期確保を min(len_hint, 8MB) に抑える。
		// 虚偽の巨大 Content-Length による即時巨大アロケーションを防ぐ。
		const INITIAL_CAP_LIMIT: usize = 8 * 1024 * 1024;
		let mut response_bytes=Vec::with_capacity((len_hint as usize).min(INITIAL_CAP_LIMIT));
		let body_start=Instant::now();
		while let Some(x) = resp.next().await{
			match x{
				Ok(b)=>{
					if response_bytes.len()+b.len()>self.config.max_size as usize{
						self.headers.append("X-Proxy-Error",format!("length:{}>{}",response_bytes.len()+b.len(),self.config.max_size).parse().unwrap());
						return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response())
					}
					response_bytes.extend_from_slice(&b);
				},
				Err(e)=>{
					let fetch_err = classify_reqwest_error(&e);
					if let Ok(mut t)=self.timings.lock(){
						t.fetch_err=Some(fetch_err.clone());
					}
					self.headers.append("X-Proxy-Error",format!("Body:{}",fetch_err).parse().unwrap_or_else(|_|"Body:unknown".parse().unwrap()));
					return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response())
				}
			}
		}
		self.src_bytes=response_bytes;
		self.mark_body_done(body_start.elapsed());
		Ok(())
	}
}
struct PreDataStream{
	content_length:Option<u64>,
	head:Option<Result<axum::body::Bytes, reqwest::Error>>,
	last:Pin<Box<dyn futures::stream::Stream<Item=Result<axum::body::Bytes, reqwest::Error>>+Send+Sync>>,
}
impl  PreDataStream{
	async fn new(value: reqwest::Response) -> Self {
		let content_length=value.content_length();
		let mut stream=value.bytes_stream();
		let head=stream.next().await;
		Self{
			content_length,
			head,
			last: Box::pin(stream)
		}
	}
}
impl futures::stream::Stream for PreDataStream{
	type Item=Result<axum::body::Bytes, reqwest::Error>;

	fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
		let mut r=self.as_mut();
		if let Some(d)=r.head.take(){
			return std::task::Poll::Ready(Some(d));
		}
		r.last.as_mut().poll_next(cx)
	}
}

#[cfg(test)]
mod network_policy_tests{
	use super::*;
	fn base_config()->ConfigFile{
		ConfigFile{
			bind_addr:"0.0.0.0:12766".to_owned(),
			timeout:10000,
			user_agent:"test".to_owned(),
			max_size:1024,
			proxy:None,
			filter_type:FilterType::Triangle,
			max_pixels:2048,
			append_headers:vec![],
			load_system_fonts:false,
			webp_quality:75.0,
			encode_avif:false,
			allowed_networks:None,
			blocked_networks:None,
			blocked_hosts:None,
			slow_log_ms:default_slow_log_ms(),
			enable_cache:false,
			cache_max_bytes:default_cache_max_bytes(),
			cache_entry_max_bytes:default_cache_entry_max_bytes(),
			cache_ttl_secs:default_cache_ttl_secs(),
			passthrough_max_bytes:default_passthrough_max_bytes(),
			dns_negative_ttl_secs:default_dns_negative_ttl_secs(),
			dns_timeout_ms:default_dns_timeout_ms(),
			dns_ttl_secs:default_dns_ttl_secs(),
			jpeg_quality:default_jpeg_quality(),
			webp_method:default_webp_method(),
			max_concurrent_downloads:default_max_concurrent_downloads(),
			inflight_buffer_budget_bytes:default_inflight_buffer_budget(),
			connect_timeout_ms:default_connect_timeout_ms(),
			fetch_retry_delay_ms:default_fetch_retry_delay_ms(),
		}
	}
	#[test]
	fn parse_valid_config(){
		let mut c=base_config();
		c.allowed_networks=Some(vec!["127.0.0.1/32".to_owned()]);
		c.blocked_networks=Some(vec!["10.0.0.0/8".to_owned()]);
		c.blocked_hosts=Some(vec!["Example.COM".to_owned()]);
		let policy=NetworkPolicy::from_config(&c).expect("valid config should parse");
		assert!(policy.blocked_hosts.contains("example.com"));
	}
	#[test]
	fn parse_invalid_network_fails(){
		let mut c=base_config();
		c.blocked_networks=Some(vec!["not-a-cidr".to_owned()]);
		assert!(NetworkPolicy::from_config(&c).is_err());
	}
	#[test]
	fn private_ipv4_blocked_without_allow(){
		let c=base_config();
		let policy=NetworkPolicy::from_config(&c).unwrap();
		assert!(policy.check_ipv4(&std::net::Ipv4Addr::new(10,0,0,1)).is_err());
		assert!(policy.check_ipv4(&std::net::Ipv4Addr::new(8,8,8,8)).is_ok());
	}
	#[test]
	fn allowed_overrides_private(){
		let mut c=base_config();
		c.allowed_networks=Some(vec!["10.1.2.3/32".to_owned()]);
		let policy=NetworkPolicy::from_config(&c).unwrap();
		assert!(policy.check_ipv4(&std::net::Ipv4Addr::new(10,1,2,3)).is_ok());
		assert!(policy.check_ipv4(&std::net::Ipv4Addr::new(10,1,2,4)).is_err());
	}
}

#[cfg(test)]
mod cache_tests{
	use super::cache::*;
	use std::sync::Arc;
	use std::time::Duration;

	fn test_cache()->Arc<ResponseCache>{
		Arc::new(ResponseCache::new(CacheConfig{
			enabled:true,
			max_bytes:1024*1024,
			entry_max_bytes:512*1024,
			ttl:Duration::from_secs(60),
		}))
	}
	fn test_key()->CacheKey{
		CacheKey{
			url:"https://example.com/test.png".to_owned(),
			is_static:false,emoji:false,avatar:false,preview:false,badge:false,accept_avif:false,
		}
	}
	#[test]
	fn cache_hit_after_put(){
		let cache=test_cache();
		let key=test_key();
		let entry=CacheEntry::new(200,Some("image/png".to_owned()),None,None,vec![1,2,3]);
		cache.put(key.clone(),entry);
		let hit=cache.get(&key);
		assert!(hit.is_some());
		assert_eq!(hit.unwrap().body,vec![1,2,3]);
	}
	#[test]
	fn cache_miss_when_disabled(){
		let cache=Arc::new(ResponseCache::new(CacheConfig{
			enabled:false,
			max_bytes:1024*1024,
			entry_max_bytes:512*1024,
			ttl:Duration::from_secs(60),
		}));
		let key=test_key();
		let entry=CacheEntry::new(200,Some("image/png".to_owned()),None,None,vec![1,2,3]);
		cache.put(key.clone(),entry);
		assert!(cache.get(&key).is_none());
	}
	#[test]
	fn cache_skip_non_200(){
		let cache=test_cache();
		let key=test_key();
		let entry=CacheEntry::new(502,None,None,None,vec![1,2,3]);
		cache.put(key.clone(),entry);
		assert!(cache.get(&key).is_none());
	}
	#[test]
	fn cache_evicts_on_capacity(){
		let cache=Arc::new(ResponseCache::new(CacheConfig{
			enabled:true,
			max_bytes:1024,
			entry_max_bytes:600,
			ttl:Duration::from_secs(60),
		}));
		let key1=CacheKey{url:"a".to_owned(),is_static:false,emoji:false,avatar:false,preview:false,badge:false,accept_avif:false};
		let key2=CacheKey{url:"b".to_owned(),is_static:false,emoji:false,avatar:false,preview:false,badge:false,accept_avif:false};
		// 各エントリは body + 256 のオーバーヘッド。body=300 → size=556。2つで1112 > 1024
		cache.put(key1.clone(),CacheEntry::new(200,None,None,None,vec![0;300]));
		cache.put(key2.clone(),CacheEntry::new(200,None,None,None,vec![0;300]));
		// key1 は追い出されているはず
		assert!(cache.get(&key1).is_none());
		assert!(cache.get(&key2).is_some());
	}
	#[test]
	fn cache_skip_oversized_entry(){
		let cache=Arc::new(ResponseCache::new(CacheConfig{
			enabled:true,
			max_bytes:1024*1024,
			entry_max_bytes:100,
			ttl:Duration::from_secs(60),
		}));
		let key=test_key();
		// body=200 + overhead=256 → size=456 > entry_max_bytes=100
		cache.put(key.clone(),CacheEntry::new(200,None,None,None,vec![0;200]));
		assert!(cache.get(&key).is_none());
	}
}
