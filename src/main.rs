use core::str;
use std::{io::Write, net::SocketAddr, pin::Pin, str::FromStr, sync::Arc};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

type AppState = (reqwest::Client, Arc<ConfigFile>, Arc<Vec<u8>>, Arc<resvg::usvg::fontdb::Database>, Arc<Semaphore>, Arc<NetworkPolicy>, Arc<DnsCache>, Arc<ResponseCache>);

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
}
fn default_slow_log_ms()->u64{50}
fn default_true()->bool{true}
fn default_cache_max_bytes()->u64{128*1024*1024}
fn default_cache_entry_max_bytes()->u64{5*1024*1024}
fn default_cache_ttl_secs()->u64{3600}
fn default_passthrough_max_bytes()->u64{1024*1024}
fn default_dns_negative_ttl_secs()->u64{10}
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
	let dns_cache=Arc::new(DnsCache::new(DNS_CACHE_TTL,Duration::from_secs(config.dns_negative_ttl_secs),DNS_CACHE_MAX_ENTRIES));
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let client=reqwest::ClientBuilder::new();
	let client=match &config.proxy{
		Some(url)=>client.proxy(reqwest::Proxy::http(url).unwrap()),
		None=>client,
	};
	// reqwestのDNS解決をDnsCacheに一本化する。
	// check_urlと実フェッチが同じキャッシュを共有し、1リクエストあたりのDNS解決を実質1回にする。
	let client=client.dns_resolver(Arc::new(SharedDnsResolver(dns_cache.clone())));
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

	// 同時に処理する画像数を制限する
	// num_cpus + 1: 全枠が重いエンコードで埋まっていても軽量リクエスト(ヘルスチェック等)が
	// 1枠に滑り込めるようにする。Step 4 でキャッシュが入ればヘルスチェックはセマフォ自体を通らなくなる。
	let max_concurrent_encode = num_cpus::get().max(2) + 1;
	let encode_semaphore = Arc::new(Semaphore::new(max_concurrent_encode));
	let response_cache = Arc::new(ResponseCache::new(cache::CacheConfig {
		enabled: config.enable_cache,
		max_bytes: config.cache_max_bytes as usize,
		entry_max_bytes: config.cache_entry_max_bytes as usize,
		ttl: Duration::from_secs(config.cache_ttl_secs),
	}));
	let arg_tup = (client, config, dummy_png, fontdb, encode_semaphore, network_policy, dns_cache, response_cache);
	rt.block_on(async{
		let http_addr:SocketAddr = arg_tup.1.bind_addr.parse().unwrap();
		let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
		let app = Router::new();
		let arg_tup0=arg_tup.clone();
		let app=app.route("/",axum::routing::get(move|headers,parms|get_file(None,headers,arg_tup0.clone(),parms)));
		let app=app.route("/{*path}",axum::routing::get(move|path,headers,parms|get_file(Some(path),headers,arg_tup.clone(),parms)));
		axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown_signal()).await.unwrap();
	});
}
/// DNS解決のタイムアウト。リゾルバのリトライ由来の張り付き(本番で約2秒/4秒)を防ぐ。
const DNS_TIMEOUT: Duration = Duration::from_millis(1500);
/// DNSキャッシュのTTL。
const DNS_CACHE_TTL: Duration = Duration::from_secs(60);
/// DNSキャッシュの最大エントリ数(無制限成長の防止)。
const DNS_CACHE_MAX_ENTRIES: usize = 1024;

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
}
/// host -> 解決済みIPの簡易キャッシュ(TTL付き・上限付き)。
pub struct DnsCache{
	inner: RwLock<HashMap<String,DnsCacheEntry>>,
	ttl: Duration,
	negative_ttl: Duration,
	max_entries: usize,
}
impl DnsCache{
	pub fn new(ttl:Duration,negative_ttl:Duration,max_entries:usize)->Self{
		Self{inner:RwLock::new(HashMap::new()),ttl,negative_ttl,max_entries}
	}
	/// hostを非同期解決する。TTL内はキャッシュを返す。
	/// ポートは解決結果に影響しないためキーはhostのみ。
	/// 戻り値の bool はキャッシュヒットかどうか(true=ヒット)。
	pub async fn resolve(&self,host:&str,port:u16)->Result<(Vec<IpAddr>,bool),String>{
		{
			let map=self.inner.read().await;
			if let Some(entry)=map.get(host){
				let ttl=if entry.ips.is_ok(){self.ttl}else{self.negative_ttl};
				if entry.resolved_at.elapsed()<ttl{
					match &entry.ips{
						Ok(ips)=>return Ok((ips.clone(),true)),
						Err(e)=>return Err(e.clone()),
					}
				}
			}
		}
		let host_port=format!("{}:{}",host,port);
		let result=match tokio::time::timeout(DNS_TIMEOUT,tokio::net::lookup_host(host_port)).await{
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
		};
		{
			let mut map=self.inner.write().await;
			if map.len()>=self.max_entries && !map.contains_key(host){
				map.retain(|_,e|{
					let ttl=if e.ips.is_ok(){self.ttl}else{self.negative_ttl};
					e.resolved_at.elapsed()<ttl
				});
				if map.len()>=self.max_entries{
					if let Some(k)=map.keys().next().cloned(){
						map.remove(&k);
					}
				}
			}
			map.insert(host.to_owned(),DnsCacheEntry{resolved_at:Instant::now(),ips:result.clone()});
		}
		match result{
			Ok(addrs)=>Ok((addrs,false)),
			Err(e)=>Err(e),
		}
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
			// ポート0で解決(reqwest がポートを上書きする)
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

async fn check_url(policy:&NetworkPolicy,dns_cache:&DnsCache,url:impl AsRef<str>)->Result<bool,String>{
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
	Ok(dns_cache_hit)
}
/// 1リクエストの各フェーズ所要時間と付随情報。Arc<Mutex<>> で get_file と
/// RequestContext(spawn_blocking 内も含む)で共有する。
#[derive(Default)]
struct PhaseTimings{
	dns_cache_hit:bool,
	cache_result:Option<CacheResult>,
	passthrough:bool,
	check:Duration,
	/// download の計測開始時刻(送信直前にセット)。
	download_start:Option<Instant>,
	download:Duration,
	decode:Duration,
	encode:Duration,
	anim:bool,
	anim_frames:u32,
	anim_in_bytes:usize,
	anim_out_bytes:usize,
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
/// 1リクエスト1行のサマリを出力する。正常かつ高速(slow_log_ms未満)なら DEBUG に落とす。
fn emit_summary(cfg:&ConfigFile,s:&ReqSummary,t:&PhaseTimings,status:u16,has_error:bool){
	let check_ms=t.check.as_millis() as u64;
	let download_ms=t.download.as_millis() as u64;
	let decode_ms=t.decode.as_millis() as u64;
	let encode_ms=t.encode.as_millis() as u64;
	let total_ms=check_ms+download_ms+decode_ms+encode_ms;
	let mut params=String::new();
	if s.is_static{params.push_str("static,");}
	if s.emoji{params.push_str("emoji,");}
	if s.avatar{params.push_str("avatar,");}
	if s.preview{params.push_str("preview,");}
	if s.badge{params.push_str("badge,");}
	if s.fallback{params.push_str("fallback,");}
	let cache_str=t.cache_result.map(|c|c.to_string()).unwrap_or_else(||"-".to_owned());
	let fast=status<400 && !has_error && total_ms<cfg.slow_log_ms;
	if fast{
		tracing::debug!(
			url=%s.url,params=%params,dns_hit=t.dns_cache_hit,cache=%cache_str,
			passthrough=t.passthrough,
			check_ms,download_ms,decode_ms,encode_ms,
			status=status as u64,error=has_error,anim=t.anim,
			anim_frames=t.anim_frames as u64,
			anim_in=t.anim_in_bytes as u64,anim_out=t.anim_out_bytes as u64,
			"request"
		);
	}else{
		tracing::info!(
			url=%s.url,params=%params,dns_hit=t.dns_cache_hit,cache=%cache_str,
			passthrough=t.passthrough,
			check_ms,download_ms,decode_ms,encode_ms,
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
	(client,config,dummy_img,fontdb,encode_semaphore,network_policy,dns_cache,response_cache):AppState,
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
				emit_summary(&config,&summary,&t,cached.status,false);
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
						emit_summary(&config,&summary,&t,cached.status,false);
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
		Ok(hit)=>{
			if let Ok(mut t)=timings.lock(){
				t.check=check_start.elapsed();
				t.dns_cache_hit=hit;
			}
		},
		Err(s)=>{
			if let Ok(mut t)=timings.lock(){
				t.check=check_start.elapsed();
			}
			let has_error=if let Ok(v)=s.parse(){
				headers.append("X-Proxy-Error",v);
				true
			}else{
				false
			};
			let is_fallback=q.fallback.is_some();
			if let Ok(t)=timings.lock(){
				emit_summary(&config,&summary,&t,if is_fallback{200}else{400},has_error);
			}
			if is_fallback{
				headers.append("Content-Type","image/png".parse().unwrap());
				return Err((axum::http::StatusCode::OK,headers,(*dummy_img).clone()).into_response());
			}
			return Err((axum::http::StatusCode::BAD_REQUEST,headers).into_response())
		}
	};

	if let Ok(mut t)=timings.lock(){
		t.download_start=Some(Instant::now());
	}
	let req=client.get(&q.url);
	let req=req.timeout(std::time::Duration::from_millis(config.timeout));
	let req=req.header("User-Agent",config.user_agent.clone());
	let req=if let Some(range)=client_headers.get("Range"){
		req.header("Range",range.as_bytes())
	}else{
		req
	};
	let resp=match req.send().await{
		Ok(resp) => resp,
		Err(e) => {
			let is_fallback=q.fallback.is_some();
			if let Ok(t)=timings.lock(){
				emit_summary(&config,&summary,&t,if is_fallback{200}else{400},false);
			}
			if is_fallback{
				headers.append("Content-Type","image/png".parse().unwrap());
				return Err((axum::http::StatusCode::OK,headers,(*dummy_img).clone()).into_response());
			}
			return Err((axum::http::StatusCode::BAD_REQUEST,headers,format!("{:?}",e)).into_response())
		}
	};
	fn add_remote_header(key:&'static str,headers:&mut HeaderMap,remote_headers:&reqwest::header::HeaderMap){
		for v in remote_headers.get_all(key){
			headers.append(key,String::from_utf8_lossy(v.as_bytes()).parse().unwrap());
		}
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
	headers.append("Cache-Control","max-age=300".parse().unwrap());
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
		emit_summary(&config,&summary,&t,status,has_error);
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
	timings: Arc<Mutex<PhaseTimings>>,
	response_cache: Arc<ResponseCache>,
	cache_key: CacheKey,
}
impl RequestContext{
	/// フェーズ計測ガードを生成する(Arcを複製して保持するため self を借用し続けない)。
	pub(crate) fn phase_guard(&self,phase:Phase)->PhaseGuard{
		PhaseGuard{timings:self.timings.clone(),start:Instant::now(),phase}
	}
	/// download の計測を確定する(download_start からの経過を download に設定)。
	pub(crate) fn mark_download_done(&self){
		if let Ok(mut t)=self.timings.lock(){
			if let Some(s)=t.download_start{
				t.download=s.elapsed();
			}
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
			// セマフォをダウンロードの前に取得する。
			// 「バッファ済みデータを抱えたままセマフォ待ち」を防ぐ。
			let semaphore = self.encode_semaphore.clone();
			let mut header=self.headers.clone();
			let _permit = semaphore.acquire().await.map_err(|_| {
				header.append("X-Proxy-Error", "SemaphoreError".parse().unwrap());
				(axum::http::StatusCode::SERVICE_UNAVAILABLE, header.clone()).into_response()
			})?;
			self.load_all(resp).await?;
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
			// セマフォをダウンロードの前に取得する。
			// 「バッファ済みデータを抱えたままセマフォ待ち」を防ぐ。
			let semaphore = self.encode_semaphore.clone();
			let _permit = semaphore.acquire().await.map_err(|_| {
                header.append("X-Proxy-Error", "SemaphoreError".parse().unwrap());
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, header.clone()).into_response()
            })?;
			self.load_all(resp).await?;
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
			self.mark_download_done();
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
					self.headers.append("X-Proxy-Error",format!("LoadAll:{:?}",e).parse().unwrap());
					return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone(),format!("{:?}",e)).into_response())
				}
			}
		}
		self.src_bytes=response_bytes;
		self.mark_download_done();
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
