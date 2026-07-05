# media-proxy-rs
## misskey/cherrypick用メディアプロキシのrust実装
機能的には互換性を維持しつつ、様々な画像形式のデコードに対応  
ほとんどの画像読み書きに[image crate v0.25](https://crates.io/crates/image/0.25.5)を使用しています

## 実行(Docker)
```
docker run -itd -p 12766:12766 ghcr.io/yojo-art/media-proxy-rs:main
```

## 実行(Linux)
例(x86_64/amd64)
```
curl -L https://github.com/yojo-art/media-proxy-rs/releases/download/nightly/media-proxy-rs_linux-amd64.gz | gzip -d > ./media-proxy-rs
chmod u+x ./media-proxy-rs
./media-proxy-rs
```
利用するプラットフォームに応じて適切なバイナリを選択してください。ファイル名のリストを示します
```
media-proxy-rs_linux-386.gz (i686+sse2)
media-proxy-rs_linux-amd64.gz (x86-64-v3)
media-proxy-rs_linux-arm-v6.gz
media-proxy-rs_linux-arm-v7.gz
media-proxy-rs_linux-arm64.gz
media-proxy-rs_linux-riscv64.gz
```

## 設定ファイル
環境変数`MEDIA_PROXY_CONFIG_PATH`を設定する事でファイルの場所を指定できます  
デフォルト値は`$(pwd)/config.json`です  
十分に強力なマシンでは`encode_avif`を`true`に変更することでAVIFエンコードを利用する事ができます

### max_size の既定値変更
`max_size` の既定値が 256MB (268435456) から **32MB (33554432)** に変更されました。  
既存の `config.json` を使用している環境では設定値がそのまま使われるため影響はありません。  
運用中の環境でも、実際に必要な上限に合わせて `max_size` の見直しを推奨します。

## target support
- [x] x86_64-unknown-linux-musl
- [x] aarch64-unknown-linux-musl
- [x] armv7-unknown-linux-musleabihf
- [x] arm-unknown-linux-musleabihf
- [x] i686-unknown-linux-musl
- [x] riscv64gc-unknown-linux-musl

## ビルド(x64 Docker)
Dockerを使用する場合はbuildxとqemuによるクロスコンパイルが利用できます  
ビルド対象プラットフォームはtarget supportの項目を参照してください
1. `git clone https://github.com/yojo-art/media-proxy-rs && cd media-proxy-rs`
2. `docker build -t media-proxy-rs .`

## ビルド(Docker aarch64等その他)
./crosstiles/arm64.shのMUSL_NAMEと./crossfiles/deps.shのmuslをダウンロードする処理を調整する必要があります

## プラットフォーム最適化
amd64ではデフォルトでx86-64-v3向けにビルドしますが、x86-64-v3未満の環境やx86-64-v4向け最適化利用したい場合./crosstiles/amd64.shのRUSTFLAGSを編集してください
他プラットフォームであればarm64.shやriscv64.shの編集でRUSTFLAGSを変更してください
最も簡単なのはtarget-cpu=nativeを指定し、実行環境と同じCPUでビルドする方法です

## ビルド(x64 Debian系)
この方法では`x86_64-unknown-linux-gnu`向けにビルドします  
すべてを静的に組み込むmusl系とは異なる共有ライブラリを必要とする場合があります
1. https://www.rust-lang.org/ja/tools/install に従ってrustをインストール
1. `apt-get install -y meson ninja-build pkg-config nasm git`
2. `git clone https://github.com/yojo-art/media-proxy-rs && cd media-proxy-rs`
3. `cargo build --release`

## 対応する画像形式
- AVIF(dav1d)
- BMP
- DDS
- Farbfeld
- GIF
- HDR
- ICO(png+rgba not support)
- JPEG
- EXR
- PNG
- PNM
- QOI
- TGA
- TIFF
- WebP
- JPEG XL(jxl-oxide)
- JPEG 2000(openjp2)
- JPEG XR(jxrlib)

## 設定項目一覧

| 項目 | 型 | 既定値 | 説明 |
|------|------|--------|------|
| `bind_addr` | string | `"0.0.0.0:12766"` | バインドアドレス |
| `timeout` | u64 | `10000` | 外部リクエストのタイムアウト(ms) |
| `user_agent` | string | | User-Agent ヘッダ |
| `max_size` | u64 | `33554432` (32MB) | ダウンロードの最大バイト数。**既定値が256MBから32MBに変更されました** |
| `proxy` | string? | `null` | HTTPプロキシURL |
| `filter_type` | string | `"Triangle"` | リサイズフィルタ |
| `max_pixels` | u32 | `2048` | 最大ピクセル寸法 |
| `webp_quality` | f32 | `75.0` | WebPエンコード品質(0-100) |
| `encode_avif` | bool | `false` | AVIFエンコードを有効にする |
| `jpeg_quality` | i32 | `85` | JPEG出力品質(0-100)。従来は`webp_quality`を流用していた |
| `webp_method` | i32 | `4` | WebPエンコードのmethod(0-6)。小さいほど高速だが圧縮率が下がる |
| `slow_log_ms` | u64 | `50` | この時間(ms)未満かつ正常完了のリクエストはログをDEBUGに降格 |
| `enable_cache` | bool | `true` | レスポンスキャッシュの有効/無効 |
| `cache_max_bytes` | u64 | `134217728` (128MB) | キャッシュ合計バイト数上限 |
| `cache_entry_max_bytes` | u64 | `5242880` (5MB) | 1エントリの最大バイト数 |
| `cache_ttl_secs` | u64 | `3600` | キャッシュTTL(秒) |
| `passthrough_max_bytes` | u64 | `1048576` (1MB) | パススルー対象の最大バイトサイズ |
| `dns_negative_ttl_secs` | u64 | `10` | DNS解決失敗のネガティブキャッシュTTL(秒) |
| `dns_timeout_ms` | u64 | `4000` | DNS解決のタイムアウト(ms)。タイムアウト時は1回リトライ |
| `dns_ttl_secs` | u64 | `300` | DNSキャッシュのTTL(秒) |
| `max_concurrent_downloads` | usize | `24` | ダウンロードの最大同時接続数 |
| `inflight_buffer_budget_bytes` | u64 | `268435456` (256MB) | 同時ダウンロードの合計バイト予算 |
| `allowed_networks` | string[]? | `null` | 許可するCIDR。ヘルスチェック等でloopbackを使う場合は`["127.0.0.1/32"]`を追加 |
| `blocked_networks` | string[]? | `null` | 遮断するCIDR |
| `blocked_hosts` | string[]? | `null` | 遮断するホスト名 |

すべての追加項目は `#[serde(default)]` 付きのため、既存の config.json をそのまま使えます。

## Raspberry Pi 5 向けチューニング

`example.config.rpi5.json` にRaspberry Pi 5 (aarch64, 4コア, RAM 4GB) 向けの推奨設定を用意しています。主な変更点:

- **`webp_method`: 2** — method=4(既定)比で2〜3倍高速。サイズは5〜15%増だが、Pi上ではエンコード時間の短縮効果が大きい
- **`cache_entry_max_bytes`: 3MB** — 大きなアニメGIF等のキャッシュを抑制
- **`webp_quality`: 70** / **`jpeg_quality`: 80** — やや品質を下げてエンコード時間を短縮
- **`slow_log_ms`: 100** — Pi上では処理が遅いため、ログ降格閾値を緩める
- **`max_concurrent_downloads`: 16** — メモリ4GBに合わせて制限
- **`inflight_buffer_budget_bytes`: 128MB** — メモリ4GBに合わせて半減

使い方:
```bash
cp example.config.rpi5.json config.json
```

## CHANGELOG

### Step 1: check_urlのDNS解決を非同期化
- `to_socket_addrs()`(同期DNS)を`tokio::net::lookup_host`+タイムアウト(1.5秒)に置換
- ネットワークポリシー(allowed/blocked_networks, blocked_hosts)の起動時パースをArc化
- DNSキャッシュ(TTL 60秒、上限1024件)を導入

### Step 2: フェーズ別計測とtracingへの移行
- `println!`ベースのログを`tracing`+`tracing-subscriber`(env-filter)に移行
- 1リクエスト1行のサマリログ(check/download/decode/encode各所要時間、cache状態、passthrough)
- encode_animにフレーム数・入出力バイト数のログを追加
- Timer構造体を削除

### Step 3: セマフォ取得順序の変更とメモリ保護
- セマフォ取得をload_all(ダウンロード)の前に移動(メモリを抱えたままセマフォ待ちを解消)
- 許可数をnum_cpus+1に(ヘルスチェック等の軽量リクエスト用に1枠確保)
- with_capacityの初期確保をmin(len_hint, 8MB)に制限
- max_size既定値を256MB→32MBに変更

### Step 4: レスポンスキャッシュと重複リクエストの合流
- エンコード済みレスポンスのLRUキャッシュ(バイト数上限管理、TTL付き)
- singleflight(同一キーの同時リクエストを1つの処理に合流)
- キャッシュヒット時はセマフォ・ダウンロード・エンコードをすべてスキップ

### Step 5: 不要な再エンコードの回避(パススルー)
- webp/png/jpeg/gifかつbadge/static無かつ寸法が目標以下かつサイズが閾値以下なら元バイト列をそのまま返却
- デコード・リサイズ・再エンコードを丸ごとスキップ(画質劣化なし)

### Step 6: DNS解決の一本化とネガティブキャッシュ
- reqwest::dns::Resolveを実装し、check_urlと実フェッチのDNS解決を同一キャッシュで一本化
- 解決失敗のネガティブキャッシュ(既定10秒)で、落ちているドメインへの連続タイムアウトを防止

### Step 7: エンコード設定のチューニングと整合
- encode_animのWebPConfigにquality/methodを反映(従来は既定値で設定を無視)
- jpeg_quality(既定85)を新設し、webp_qualityの流用を廃止
- webp_method(既定4)を新設し、静止画・アニメ両方に反映

### 本番ログでの効果確認の観点
- **キャッシュヒット率**: `cache=hit` の割合。重複率72%の環境で50%超が目標
- **パススルー率**: `passthrough=true` の割合。絵文字リクエスト(55%)の大半が該当する見込み
- **encode_animの所要時間**: `anim=true` のリクエストの `encode_ms` 分布
- **1秒超スタック件数**: `check_ms` + `wait_ms` + `ttfb_ms` + `body_ms` が1000を超えるリクエスト数(DNS一本化+ネガティブキャッシュで激減する見込み)
- **定期統計ログ**: 60秒ごとの `periodic_stats` で `dl_active`, `cpu_active`, `buf_used_mb` の推移を確認

### Step 8: DNSキャッシュの安定性強化
- configurable TTL(dns_ttl_secs既定300秒)・タイムアウト(dns_timeout_ms既定4000ms、1回リトライ)
- タイムアウト由来のネガティブキャッシュTTLを短く(2秒)、確定的失敗は既定10秒
- stale-while-error(元TTLの10倍まで古い成功エントリを再利用)
- singleflightをbroadcast方式に変更(Mutex/awaitの排他回避)

### Step 9: フェーズ計測の細分化(download → wait/ttfb/body)
- download_ms を wait_ms(セマフォ待ち)・ttfb_ms(req.send→最初のレスポンス)・body_ms(ボディ全受信)に3分割
- ネットワーク遅延とリソース待ちの切り分けが容易に

### Step 10: セマフォの責務分離(ダウンロード直列化の解消)
- ダウンロードpermit(max_concurrent_downloads既定24): req.send前〜load_all完了まで保持
- バイト予算(inflight_buffer_budget_bytes既定256MB): load_all前に予約、エンコード完了後に解放
- CPUセマフォ(num_cpus): spawn_blocking直前〜完了まで保持
- 従来の単一セマフォ(DL+CPU共用)による head-of-line blocking を解消

### Step 11: 定期統計ログ(60秒間隔)
- 60秒ごとに `periodic_stats` をINFOログ出力
- requests/errors/cache_hits/cache_misses(期間カウンタ、リセット型)
- dl_active/cpu_active/buf_used_mb(瞬時値)
- cache_entries/cache_bytes/dns_entries(瞬時値)

### Step 12: clippy修正・README更新
- type_complexity警告をDnsResult型エイリアスで解消
- len_without_is_empty警告をpub(crate)で回避
- 設定項目一覧・CHANGELOGを最新化
