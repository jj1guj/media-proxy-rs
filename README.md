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
2. `apt-get install -y build-essential meson ninja-build pkg-config nasm git python3 libglib2.0-dev`
3. `git clone https://github.com/yojo-art/media-proxy-rs && cd media-proxy-rs`
4. `bash crossfiles/build-libvips.sh`
5. `cargo build --release`

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
- HEIC
- MNG (MNG-LC規格相当)
- PDF
- VIPSネイティブ形式

## 設定項目一覧

| 項目                           | 型        | 既定値              | 説明                                                                         |
| ------------------------------ | --------- | ------------------- | ---------------------------------------------------------------------------- |
| `bind_addr`                    | string    | `"0.0.0.0:12766"`   | バインドアドレス                                                             |
| `timeout`                      | u64       | `10000`             | 外部リクエストのタイムアウト(ms)                                             |
| `user_agent`                   | string    |                     | User-Agent ヘッダ                                                            |
| `max_size`                     | u64       | `33554432` (32MB)   | ダウンロードの最大バイト数。**既定値が256MBから32MBに変更されました**        |
| `proxy`                        | string?   | `null`              | HTTPプロキシURL                                                              |
| `filter_type`                  | string    | `"Triangle"`        | リサイズフィルタ                                                             |
| `max_pixels`                   | u32       | `2048`              | 最大ピクセル寸法                                                             |
| `webp_quality`                 | f32       | `75.0`              | WebPエンコード品質(0-100)                                                    |
| `encode_avif`                  | bool      | `false`             | AVIFエンコードを有効にする                                                   |
| `jpeg_quality`                 | i32       | `85`                | JPEG出力品質(0-100)。従来は`webp_quality`を流用していた                      |
| `webp_method`                  | i32       | `4`                 | WebPエンコードのmethod(0-6)。小さいほど高速だが圧縮率が下がる                |
| `slow_log_ms`                  | u64       | `50`                | この時間(ms)未満かつ正常完了のリクエストはログをDEBUGに降格                  |
| `enable_cache`                 | bool      | `true`              | レスポンスキャッシュの有効/無効                                              |
| `cache_max_bytes`              | u64       | `134217728` (128MB) | キャッシュ合計バイト数上限                                                   |
| `cache_entry_max_bytes`        | u64       | `5242880` (5MB)     | 1エントリの最大バイト数                                                      |
| `cache_ttl_secs`               | u64       | `3600`              | キャッシュTTL(秒)                                                            |
| `passthrough_max_bytes`        | u64       | `1048576` (1MB)     | パススルー対象の最大バイトサイズ                                             |
| `dns_negative_ttl_secs`        | u64       | `10`                | DNS解決失敗のネガティブキャッシュTTL(秒)                                     |
| `dns_timeout_ms`               | u64       | `4000`              | DNS解決のタイムアウト(ms)。タイムアウト時は1回リトライ                       |
| `dns_ttl_secs`                 | u64       | `300`               | DNSキャッシュのTTL(秒)                                                       |
| `dns_cache_max_entries`        | usize     | `1024`              | DNSキャッシュの最大エントリ数                                                |
| `max_concurrent_downloads`     | usize     | `24`                | ダウンロードの最大同時接続数                                                 |
| `inflight_buffer_budget_bytes` | u64       | `268435456` (256MB) | 同時ダウンロードの合計バイト予算                                             |
| `connect_timeout_ms`           | u64       | `3000`              | TCP接続タイムアウト(ms)。`timeout`より小さく設定すること                     |
| `fetch_retry_delay_ms`         | u64       | `500`               | 接続失敗時のリトライ前待機(ms)                                               |
| `otlp_metrics_endpoint`        | string?   | `null`              | OTLP/HTTP metrics送信先。`/v1/metrics`を含む完全なURL。`null`で無効          |
| `otlp_export_interval_ms`      | u64       | `5000`              | OTLP metrics送信間隔(ms)                                                     |
| `otlp_service_name`            | string    | `"media-proxy-rs"`  | OpenTelemetryの`service.name`                                                |
| `allowed_networks`             | string[]? | `null`              | 許可するCIDR。ヘルスチェック等でloopbackを使う場合は`["127.0.0.1/32"]`を追加 |
| `blocked_networks`             | string[]? | `null`              | 遮断するCIDR                                                                 |
| `blocked_hosts`                | string[]? | `null`              | 遮断するホスト名                                                             |

すべての追加項目は `#[serde(default)]` 付きのため、既存の config.json をそのまま使えます。

### OTLP基本メトリクス

`otlp_metrics_endpoint` を設定すると、以下のメトリクスをOTLP/HTTPで送信します。時間の単位は秒です。ステータス属性は `status_class` (`1xx`〜`5xx`、`other`) のみで、URL、ドメイン、IP、キャッシュキー、エラー詳細は属性に含めません。

| メトリクス                             | 種別          | 内容                                           |
| -------------------------------------- | ------------- | ---------------------------------------------- |
| `media_proxy_requests_total`           | Counter       | 完了リクエスト累積数（最終ステータスクラス別） |
| `media_proxy_errors_total`             | Counter       | エラー累積数（最終ステータスクラス別）         |
| `media_proxy_domain_requests_total`    | Counter       | caller／targetドメイン・成否別リクエスト累積数 |
| `media_proxy_requests_active`          | UpDownCounter | 処理中リクエスト数                             |
| `media_proxy_upstream_responses_total` | Counter       | 上流レスポンス累積数（ステータスクラス別）     |
| `media_proxy_request_duration`         | Histogram     | リクエスト合計レイテンシ                       |
| `media_proxy_url_check_duration`       | Histogram     | URL／ポリシーチェック時間                      |
| `media_proxy_download_wait_duration`   | Histogram     | ダウンロード枠待機時間                         |
| `media_proxy_upstream_ttfb_duration`   | Histogram     | 上流TTFB                                       |
| `media_proxy_upstream_body_duration`   | Histogram     | 上流body受信時間                               |
| `media_proxy_cpu_wait_duration`        | Histogram     | CPU枠待機時間                                  |
| `media_proxy_decode_duration`          | Histogram     | decode時間                                     |
| `media_proxy_encode_duration`          | Histogram     | encode時間                                     |

キャッシュ結果の属性 `result` は `hit`、`miss`、`joined`、`bypass`、`stale` の5種類です。キャッシュヒット率は `media_proxy_cache_requests_total` から `(hit + joined) / (hit + joined + miss)` で算出します。GaugeはOTLP送信間隔ごとに更新されます。

| メトリクス                                   | 種別      | 内容                                   |
| -------------------------------------------- | --------- | -------------------------------------- |
| `media_proxy_cache_requests_total`           | Counter   | キャッシュ結果別リクエスト累積数       |
| `media_proxy_cache_entries`                  | Gauge     | キャッシュエントリ数                   |
| `media_proxy_cache_bytes`                    | Gauge     | キャッシュ使用バイト数                 |
| `media_proxy_cache_capacity_bytes`           | Gauge     | キャッシュ容量上限                     |
| `media_proxy_cache_capacity_evictions_total` | Counter   | 容量超過によるeviction累積数           |
| `media_proxy_cache_expired_evictions_total`  | Counter   | 有効期間超過によるeviction累積数       |
| `media_proxy_singleflight_active`            | Gauge     | singleflight処理中件数                 |
| `media_proxy_static_requests_total`          | Counter   | `/static.webp`のキャッシュ結果別累積数 |
| `media_proxy_downloads_active`               | Gauge     | ダウンロード同時処理数                 |
| `media_proxy_downloads_limit`                | Gauge     | ダウンロード同時処理上限               |
| `media_proxy_cpu_active`                     | Gauge     | CPU同時処理数                          |
| `media_proxy_cpu_limit`                      | Gauge     | CPU同時処理上限                        |
| `media_proxy_buffer_used_bytes`              | Gauge     | 処理中バッファ使用量                   |
| `media_proxy_buffer_limit_bytes`             | Gauge     | 処理中バッファ上限                     |
| `media_proxy_buffer_wait_duration`           | Histogram | バッファ予算セマフォ待機時間           |

出力形式の属性 `format` は `jpeg`、`png`、`webp`、`avif`、`other` の5種類です。圧縮率は `media_proxy_output_bytes_total / media_proxy_input_bytes_total`、パススルー率は `media_proxy_passthrough_total / media_proxy_requests_total` で算出します。処理エラーの `category` は `decode`、`encode`、`policy`、`size`、`internal`、fetchエラーは `dns`、`connect`、`timeout`、`reset`、`body`、`other` の固定分類です。

| メトリクス                                 | 種別    | 内容                                 |
| ------------------------------------------ | ------- | ------------------------------------ |
| `media_proxy_outputs_total`                | Counter | 出力形式別累積数                     |
| `media_proxy_input_bytes_total`            | Counter | 処理入力バイト累積数                 |
| `media_proxy_output_bytes_total`           | Counter | 処理出力バイト累積数                 |
| `media_proxy_passthrough_total`            | Counter | パススルー累積数                     |
| `media_proxy_processing_errors_total`      | Counter | 画像処理・ポリシーエラー分類別累積数 |
| `media_proxy_fetch_errors_total`           | Counter | fetchエラー分類別累積数              |
| `media_proxy_fetch_retry_attempts_total`   | Counter | fetchリトライ累積数                  |
| `media_proxy_fetch_retry_successes_total`  | Counter | fetchリトライ成功累積数              |
| `media_proxy_dns_cache_requests_total`     | Counter | DNSキャッシュ結果別累積数            |
| `media_proxy_dns_cache_entries`            | Gauge   | DNSキャッシュエントリ数              |
| `media_proxy_dns_cache_capacity_entries`   | Gauge   | DNSキャッシュエントリ上限            |
| `media_proxy_dns_retry_attempts_total`     | Counter | DNSリトライ累積数                    |
| `media_proxy_dns_retry_successes_total`    | Counter | DNSリトライ成功累積数                |
| `media_proxy_stale_served_total`           | Counter | staleキャッシュ提供累積数            |
| `media_proxy_animations_total`             | Counter | アニメーション処理累積数             |
| `media_proxy_animation_frames_total`       | Counter | アニメーション処理フレーム累積数     |
| `media_proxy_animation_input_bytes_total`  | Counter | アニメーション入力バイト累積数       |
| `media_proxy_animation_output_bytes_total` | Counter | アニメーション出力バイト累積数       |
| `media_proxy_uptime`                       | Gauge   | プロセス起動からの経過秒数           |

DNSキャッシュ結果の属性 `result` は `hit`、`stale`、`miss` の3種類です。

### ドメイン構造化ログ

リクエストサマリの `request` ログには、ログ分析に利用できる以下のフィールドを出力します。

| フィールド            | 内容                                                           |
| --------------------- | -------------------------------------------------------------- |
| `caller_domain`       | `Origin`のドメイン。未指定時は`Referer`、両方なければ`unknown` |
| `target_domain`       | `url`パラメータの取得対象ドメイン。不正URLは`invalid`          |
| `final_target_domain` | リダイレクト後に最後に試行した取得対象ドメイン                 |
| `error`               | HTTP 4xx／5xxまたはプロキシ処理エラーの場合は`true`            |

ドメインは小文字へ正規化し、ポート、パス、クエリを含めません。IPv4／IPv6アドレスは生値を記録せず、`ip`として集約します。`Origin`と`Referer`は欠落・偽装可能な申告値であり、認証やアクセス制御には使用しません。

### Alloy

`compose.example.yml` は、media-proxyとAlloyを同じComposeプロジェクトで起動します。Prometheusは既存インスタンスを使用し、GrafanaはこのComposeでは起動しません。

既存Prometheusはremote-write receiverを有効にして起動してください。

```text
--enable-feature=remote-write-receiver
```

既定ではAlloyから `http://host.docker.internal:9090/api/v1/write` へメトリクスを送ります。`config/config.json` のOTLP設定は、同じComposeネットワーク内のAlloyを指定します。

```json
{
  "otlp_metrics_endpoint": "http://alloy:4318/v1/metrics",
  "otlp_export_interval_ms": 5000
}
```

既存設定の他の項目は変更せず、上記2項目だけを反映してください。環境に合わせてPrometheus接続先を指定して起動します。

```fish
set -lx PROMETHEUS_REMOTE_WRITE_URL http://host.docker.internal:9090/api/v1/write
docker compose -f compose.example.yml up -d
```

`server` とAlloyは同じComposeネットワークに参加するため、OTLPポート4318をホストへ公開する必要はありません。Grafanaダッシュボードは既存Prometheusだけをデータソースとして使用します。Alloyの管理画面だけが `127.0.0.1:12345` に公開され、OTLP受信ポートはComposeネットワーク内に限定されます。

### Grafana provisioning

外部Grafanaに以下の環境変数を設定します。

| 環境変数         | 内容                                 |
| ---------------- | ------------------------------------ |
| `PROMETHEUS_URL` | Grafanaから到達できるPrometheusのURL |

次のディレクトリをGrafanaコンテナへread-onlyでマウントします。

| リポジトリ内のパス                               | Grafanaコンテナ内のパス                   |
| ------------------------------------------------ | ----------------------------------------- |
| `observability/grafana/provisioning/datasources` | `/etc/grafana/provisioning/datasources`   |
| `observability/grafana/provisioning/dashboards`  | `/etc/grafana/provisioning/dashboards`    |
| `observability/grafana/dashboards`               | `/var/lib/grafana/dashboards/media-proxy` |

provisioning後は `Media Proxy` フォルダーに `Media Proxy Overview` ダッシュボードが作成されます。既定範囲は直近15分、更新間隔は5秒です。RPS、エラー率、レイテンシ、キャッシュ、リソース使用量、上流障害、出力形式、Prometheusで集計したドメインTop 10を表示します。`Telemetry freshness` が30秒を超えるか `No data - check media-proxy / Alloy` になった場合は、media-proxyまたはAlloyからの送信停止を確認してください。

## media-proxy.jiskey.dev 向け運用設定

`config.media-proxy.jiskey.dev.json` にIntel N95 (4コア, RAM 8GB) で稼働するmedia-proxy.jiskey.dev向けの運用設定を用意しています。主な変更点:

- **`cache_max_bytes`: 1GB** — キャッシュ滞留時間を確保する
- **`cache_ttl_secs`: 43200** — キャッシュTTLを12時間に延長する
- **`dns_cache_max_entries`: 4096** — 多数の連合先ホストを保持し、DNSキャッシュの回転を抑える
- **`max_concurrent_downloads`: 128** — バースト時のダウンロード同時実行枠を拡大する
- **`inflight_buffer_budget_bytes`: 512MB** — 同時ダウンロードの合計バイト予算を拡大する

使い方:

```bash
cp config.media-proxy.jiskey.dev.json config.json
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

### Step 13: fetchエラーの完全可視化

- `classify_reqwest_error` で reqwest::Error を connect/timeout/dns/reset/body/other に分類
- サマリログに `fetch_err=` フィールド追加(正常時は `-`)
- `req.send()` 失敗時に `X-Proxy-Error` ヘッダ付与・`error=true` に修正(従来はerror=falseで原因不明だった)
- `load_all` ボディ受信エラーも同様に分類
- periodic_stats に `ferr_connect`/`ferr_timeout`/`ferr_dns`/`ferr_reset`/`ferr_body`/`ferr_other` カウンタ追加
- サマリログに `dns_v4`/`dns_v6` フィールド追加(DNS解決結果のv4/v6アドレス数)

### Step 14: connect_timeoutと接続リトライ

- `connect_timeout_ms`(既定3000)でTCP接続タイムアウトを全体タイムアウトより短く設定
- 接続段階の失敗(is_connect/is_timeout)時に `fetch_retry_delay_ms`(既定500ms)待って1回リトライ
- Rangeリクエスト・ボディ受信中エラー・4xx/5xxはリトライしない
- 2回目のsendには残り時間ベースのタイムアウトを適用(全体 `timeout` を超えない)
- サマリログに `retried=true/false`、periodic_stats に `retry_attempts`/`retry_saved` を追加

### Step 15: HTTP/2有効化とHTTPバージョン計測

- reqwestの `http2` featureを有効化(rustls + ALPNによる自動h2ネゴシエーション)
- サマリログに `http=2/1.1/1.0` フィールド追加
- periodic_stats に `http1_responses`/`http2_responses` カウンタ追加
- HTTP/2により同一ホストへのTCP接続が1本に多重化され、CGNセッション消費を抑制

### Step 16: 仕上げ

- README更新: DS-Lite環境ガイダンス・切り分け表・効果確認観点を追記

## DS-Lite / CGN 環境での既知の問題

DS-Lite方式(IPv4 over IPv6)でCGN(Carrier-Grade NAT)を経由する環境では、CGNのNATセッション枯渇により**新規TCP接続(SYN)だけが間欠的に失敗**する現象が発生することがあります。既存接続は影響を受けません。

本プロキシでは以下の設定で緩和できます:

| 設定                        | 効果                                              |
| --------------------------- | ------------------------------------------------- |
| `connect_timeout_ms: 3000`  | 接続タイムアウトを短くし、リトライの余地を確保    |
| `fetch_retry_delay_ms: 500` | 瞬断窓を跨ぐための待機後に1回リトライ             |
| HTTP/2 (自動)               | 同一ホストへの接続を多重化し、新規TCP接続数を削減 |

### エラー切り分け表

| periodic_stats の指標                 | 疑うべき原因                                             |
| ------------------------------------- | -------------------------------------------------------- |
| `ferr_timeout` が多い                 | CGN瞬断。`retry_saved` で救済されていれば緩和は機能中    |
| `ferr_connect` + `NetworkUnreachable` | IPv6到達不能(コンテナにv6疎通がない場合)                 |
| `ferr_reset` が多い                   | 接続プール内の死んだ接続の再利用                         |
| `retry_saved / retry_attempts` が低い | 瞬断が長い(3秒超)。`connect_timeout_ms` の引き上げを検討 |
| `http2_responses` が0                 | HTTP/2ネゴシエーション失敗。TLS設定を確認                |

### 本番ログでの効果確認の観点

- `ferr_timeout` のうち `retry_saved` で救済された比率(目標: 瞬断起因の失敗の大半が救済)
- エラー率が大幅に低下すること(目標: 3%以下)
- 失敗時の所要時間が10秒張り付きから最大約7秒(3秒+0.5秒+残り)に短縮
- HTTP/2比率(`http2_responses / (http1_responses + http2_responses)`)と新規接続数の減少傾向
