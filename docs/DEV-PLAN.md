# QzoneArchive 升级开发计划（多数据源本地归档）

> 本文件是开发前分析：现有架构与数据流、差距分析、数据源清单、数据库升级设计、
> 分阶段实施计划、预计文件变更、风险与回滚方案。
> 目标：把「主要依靠 QQ 空间互动记录恢复内容」的工具，升级为「更完整、可靠的
> 本地归档软件」。**不推倒重写，不破坏现有功能与已有用户数据库。**

---

## 1. 现有架构与数据流分析

### 1.1 技术栈（确认现状）

| 层 | 现状 |
|---|---|
| 桌面框架 | Tauri 2（`src-tauri/tauri.conf.json`，identifier `top.ehre.qzonearchive`） |
| 后端 | Rust 2021，`rusqlite`(bundled SQLite) + `reqwest`(rustls) + `tokio` + `serde_json` |
| 前端 | Vue 3 + TypeScript + Vite + PrimeVue 4 + Pinia（`src/`） |
| 数据库 | 单文件 `qzone-archive.sqlite3`（WAL），位于 `app_data_dir` |

### 1.2 Rust 模块（现状）

- `src-tauri/src/main.rs` — 入口。
- `src-tauri/src/lib.rs` — 注册 `ArchiveState` / `QLoginState` / `RecycleAuthState` 三个托管状态，以及全部约 40 个 Tauri command。
- `src-tauri/src/qlogin.rs`（770 行）— QQ 登录：
  - 二维码登录（`ptqrshow` / `ptqrlogin` / `check_sig`，`p_skey` → `bkn()` 计算 `g_tk`）；
  - 网页登录（`i.qq.com` 独立窗口 + WebView Cookie API）；
  - 会话仅存内存：`LoginSession { cookies, uin, g_tk, user_agent, login_sig }`；
  - 内置移动端 UA 池、`ptqrtoken`/`bkn` 哈希、`warmup_qzone_session` 追踪 Cookie 收集；
  - 日志脱敏：Cookie 值打 `[已隐藏：登录凭证不会写入控制台]`。
- `src-tauri/src/qzone.rs`（1297 行）— QQ 空间 API 客户端：
  - `fetch_feeds`（`mobile.qzone.qq.com/get_feeds`）：`refresh_type=1`（首页）/ `2`（后续页），游标 `res_attach`，最多 6 次重试，指数退避 `1500ms * 2^(n-1)`，429/5xx/可重试业务码识别，永久错误（登录失效等）不重试；
  - 相册能力：`list_qzone_albums`（`fcg_list_album_v3`）、相册回收站（`cgi_alist_recycle_v2` / `cgi_plist_recycle_v2`，需 `pwd2sig` 独立密码）、`create_qzone_album`、`recover_recycle_album` / `recover_recycle_photos`；
  - `parse_qzone_json` — 兼容 JSON / JSONP（`frameElement.callback(...)` / `shine0(...)` / 任意外层对象兜底提取）。
- `src-tauri/src/archive.rs`（3252 行）— 归档引擎 + 数据库：
  - `start_feed_archive`：主循环，逐页 `fetch_feeds` → `save_page`；断点续传（`archive_checkpoints`）、10 分钟 300 页限流（`archive_rate_limits`）、异常页跳过与重试（`archive_skips`，offset 二分探测恢复）、重复游标死循环保护、取消（`AtomicBool`）；
  - 数据库：`open_database` 内联 `CREATE TABLE IF NOT EXISTS` + 若干硬编码列探测 `ALTER TABLE` + 两个数据迁移函数（`migrate_legacy_dynamics`、`migrate_dynamic_categories`）；
  - 媒体：**按需下载** —— `load_archived_image`（多候选地址 × cookie/referer 组合，写 `images/<uin>/<id>-<index>.<ext>`，`.part` 原子改名，magic bytes 类型验证、QQ 占位图识别）、`load_archived_video`（`cache/videos/<uin>-<id>.mp4`，类型验证，403 提示签名过期）；
  - 查询/导出：`list_archived_feeds` / `get_archived_feed`（评论/点赞从 `archive_feeds` event 行重建）、`list_archived_media`（媒体时光轴）、`get_archive_overview`、`get_interaction_ranking`、`list_interactors`、`export_archived_html`（**图片引用远程 URL**）、删除/清空/`delete_all_app_data`。

### 1.3 当前数据库结构（v1 基线，全部在 `archive.rs::open_database` 内联建表）

```
archive_feeds       互动事件行：owner_uin, feed_key(UNIQUE), cell_id, event_type(2评论/217赞/311回复…),
                    event_time, title/content/event_summary, actor_uin/actor_name,
                    original_author_uin/name, picture_count, pictures_json, video_json,
                    comments_json, raw_json(整条 feed 原始 JSON), archived_at
archive_dynamics    规范化动态：owner_uin, cell_id(UNIQUE), published_at, content,
                    author_uin/name, category('self'/'other'/'guestbook'), pictures_json,
                    video_json, raw_original_json(仅 original 子对象), archived_at
archive_checkpoints 断点续传：owner_uin(PK), attach_info(游标), pages/fetched/saved, updated_at
archive_rate_limits 限流：owner_uin(PK), window_started_at, requested_pages
archive_skips       异常页：id, owner_uin, cursor, resume_cursor, page_number, cursor_offset,
                    offset_advance, base_time, error, skipped_at, retry_count, last_retry_at,
                    resolved_at, recovered_records
```

### 1.4 数据流（现状）

```
扫码/网页登录 ──► qlogin（内存 Cookie + g_tk）
      │
      ▼
start_feed_archive ──循环──► qzone::fetch_feeds(get_feeds, 游标 res_attach)
      │                             │
      │                    重试/限流/异常跳过（archive_skips）
      ▼
save_page（事务）──► archive_feeds（每行=1 条互动事件，含 raw_json）
      └────────────► archive_dynamics（从 original 提取的规范化动态，cell_id 唯一）
      └────────────► archive_checkpoints（游标+统计）
      │
前端浏览 ──► load_archived_image / load_archived_video（按需下载，无 DB 记录）
      │
导出 ──► export_archived_html（HTML，图片=远程 URL）
```

### 1.5 现状优点（升级时必须保留）

- 登录凭证只存内存、日志已脱敏、无任何遥测/上传——安全基线良好；
- 断点续传、限流、异常跳过、取消、重复游标保护——长任务控制机制成熟；
- JSON/JSONP 兼容解析、多候选图片地址 + 4 种 cookie/referer 组合、占位图识别；
- 多账号数据隔离（`owner_uin` 贯穿所有表）。

---

## 2. 当前功能与升级目标之间的差距

| # | 目标 | 现状 | 差距 |
|---|---|---|---|
| 1 | 多数据源归档 | 仅 `get_feeds` 互动列表 | 无本人说说、相册照片、留言板、单条动态详情等数据源；无数据源抽象、无每源同步状态 |
| 2 | 统一数据模型 | `archive_dynamics` + 嵌入 JSON | 无独立评论/点赞/回复/留言/相册/用户表；无「按平台 ID 合并 + 指纹候选匹配」；无来源保留（多对多） |
| 3 | 本地媒体归档 | 仅按需缓存，无 DB 记录 | 无远程/本地/哈希/大小/类型/状态/下载时间记录；无断点续传、重试队列、限速、SHA-256 去重、三种下载模式 |
| 4 | 原始数据留存 | `raw_json` 只覆盖互动流，且嵌入业务表 | 无独立 Raw 层（接口级响应留存）、无「从 Raw 重建归档」流程 |
| 5 | 完整性审计 | 仅 `get_archive_overview` 计数 | 无年份分布、来源分布、媒体下载状态、失败/重复/缺口/缺正文/评论数不一致/疑似缺失时间区间等审计项；无「已发现完整率 vs 绝对完整率」区分 |
| 6 | 增量同步 | 断点续传 = 继续游标，无增量语义 | 无首次全量 vs 增量区分；无远端删除/权限变化/暂时不可访问/仅本地存在状态标记；多账号仅数据层隔离，会话层单账号 |
| 7 | 安全 | Cookie 内存存储、日志脱敏（大部分） | 无系统安全存储「保持登录」；`archive.rs` 任务错误日志把 `ownerUin` 打进 stderr；无「切换账号前停止任务」流程 |
| 8 | 开放格式导出 | 仅 HTML（远程图片） | 无离线 ZIP / JSON / Markdown(Obsidian) 导出；无相对路径媒体引用；无「从导出包重新导入」 |

---

## 3. 数据源清单：可确认 vs 需验证

> 原则：**不编造接口**。「可确认」均来自本仓库代码中已实际使用的端点；「需验证」来自公开社区资料、但本仓库尚无代码/真实响应样本，接入前必须在独立适配层中先用真实账号验证 URL、参数与返回结构。

### 3.1 可确认（代码中已使用，可直接迁入适配层）

| 数据源 | 端点 | 现状位置 | 备注 |
|---|---|---|---|
| 互动列表（主源） | `mobile.qzone.qq.com/get_feeds` | `qzone.rs::fetch_feeds` | `res_type=1`，游标 `res_attach`；已含重试/限流/跳过 |
| 相册列表 | `h5.qzone.qq.com/proxy/domain/photo.qzone.qq.com/fcgi-bin/fcg_list_album_v3` | `qzone.rs::list_qzone_albums` | JSONP `shine0`；`pageStart/pageNum` |
| 回收站相册 | `user.qzone.qq.com/proxy/domain/photo.qzone.qq.com/cgi-bin/common/cgi_alist_recycle_v2` | `qzone.rs` | 需 `pwd2sig` |
| 回收站照片 | `…/cgi_plist_recycle_v2` | `qzone.rs` | 需 `pwd2sig`，`albumId` |
| 相册操作 | `cgi_add_album_v2` / `cgi_recover_album_v2` / `cgi_recover_pic_v2` | `qzone.rs` | 写操作，不进入归档数据源 |
| 用户资料 | `h5.qzone.qq.com/proxy/domain/vip.qzone.qq.com/fcg-bin/fcg_get_vipinfo_mobile`（回退 `cgi_userinfo_get_all`） | 前端 `qlogin.ts` | 昵称/头像 |

### 3.2 需验证（本仓库无实现/无响应样本，Phase 1 不接入，仅设计适配层插槽）

| 数据源 | 预期形态（公开资料） | 验证项 |
|---|---|---|
| 本人说说列表 | `user.qzone.qq.com/proxy/domain/taotao.qq.com/cgi-bin/emotion_cgi_msglist_v6`（`uin/g_tk/pos/num/format=jsonp`） | URL、参数名（`pos`/`num`/`uin`/`g_tk`）、返回结构（`msglist[]`、`total`、`hasMore`） |
| 相册内照片列表 | `photo.qzone.qq.com` 系列 `cgi_list_photo`（`topicId`=相册 ID、`pageStart/pageNum`） | URL、参数、返回结构（`photoList[]`、`totalInPage`/`total`） |
| 留言板 | 留言板列表接口（`gb` 相关 cgi） | 端点形态与返回结构（`msgList`、游标） |
| 单条动态详情 | 动态详情/回复列表 cgi（按 `cell_id` 拉全量评论/点赞） | 端点、参数、返回结构 |
| 原图地址 | 照片大图/原图 URL 规则（`photourl` 之外的大图字段） | 字段名与地址形态 |

接入流程（后续阶段）：在独立适配层实现 → 用脱敏样本 + 真实响应验证 → 测试通过后启用对应数据源，并写入 `source_states`。

---

## 4. 数据库升级设计

### 4.1 版本化 migration 框架（替换现有硬编码列探测）

- 引入 `PRAGMA user_version` 作为 schema 版本号（0 = 全新库）。
- `src-tauri/src/db/mod.rs`：
  - `migrations: &[Migration]`，`Migration { version, name, apply(&mut Connection) }`；
  - `migrate(conn)`：读 `user_version`，对 `> current` 的 migration 逐个在事务内应用，成功后 `PRAGMA user_version = version`；失败回滚并报错；
  - **旧库兼容**：migration v1 = 现有全部 `CREATE TABLE IF NOT EXISTS` + 现有列探测 + `migrate_legacy_dynamics` + `migrate_dynamic_categories`（原样搬入，保证旧库打开后走完 v1 即与现状等价，且幂等）；
  - 现有 `archive.rs::open_database` 改为调用框架（不重复内联建表）。

### 4.2 新表（migration v2+，Phase 1）

```
source_states                每数据源独立同步状态
  owner_uin TEXT, source TEXT（'feeds'|'shuoshuo'|'album_list'|'album_photos:<aid>'|'guestbook'|…）
  cursor TEXT, status TEXT('idle'|'running'|'paused'|'completed'|'error'|'limited'),
  last_sync_at, next_sync_at, last_error, total_fetched, total_saved, updated_at
  PK(owner_uin, source)

raw_responses                接口原始响应（Raw 层）
  id, owner_uin, source, method, url, query_json（已脱敏：剔除 cookie 类参数），
  status_code, content_type, body TEXT（或 BLOB 当非 UTF-8）, body_sha256,
  fetched_at, UNIQUE(owner_uin, source, body_sha256)

users                        用户表
  uin TEXT, nickname, avatar_url, first_seen_at, last_seen_at, PK(uin)

archive_dynamics（ALTER 增列，不破坏现有行）
  + source TEXT NOT NULL DEFAULT 'feeds'
  + content_fingerprint TEXT（作者+时间+正文+媒体指纹，候选匹配用）
  + remote_status TEXT NOT NULL DEFAULT 'active'（active/remote_deleted/permission_changed/
    temporarily_unavailable/local_only）
  + first_seen_at, last_seen_at

dynamic_sources              动态 × 来源（多对多，保留合并前来源）
  dynamic_id, owner_uin, source, raw_response_id, matched_by('platform_id'|'fingerprint'|'new'),
  fetched_at, PK(dynamic_id, source)

merge_logs                   合并审计
  id, owner_uin, dynamic_id, source, matched_by, note, created_at

media_items                  本地媒体归档
  id, owner_uin, dynamic_id（可空：相册照片无动态）, media_kind
  ('image'|'image_original'|'video'|'video_cover'),
  remote_url, local_path, sha256, size_bytes, mime_type,
  download_status('pending'|'downloading'|'done'|'failed'|'skipped'),
  download_attempts, last_error, last_downloaded_at, created_at,
  UNIQUE(owner_uin, media_kind, remote_url)

comments / likes / replies / guestbook / albums / album_photos
  （统一模型分表，Phase 1 建表 + 从 feeds 数据写入评论/点赞行；后续阶段接入其他源）

archive_settings             键值设置（含媒体下载模式）
  key TEXT PK, value TEXT, updated_at
```

### 4.3 兼容性保证

- 所有新表独立创建；对既有表只做 `ADD COLUMN`（带默认值），不做 `DROP`/`RENAME`/改约束；
- 旧库打开：`user_version=0` → 依次跑 v1（现状等价）→ v2（新表）→ …；
- 新库打开：直接从 v1 建全部基础表 → v2；
- 每次升级前对数据库文件做一次 `PRAGMA quick_check`，失败拒绝打开并提示备份。

---

## 5. 分阶段实施计划

### Phase 1 — 基础设施（本次执行）：原始留存 + 统一模型 + migration + 媒体下载
1. migration 框架 + 旧库升级（v1/v2）；
2. `raw_responses` 留存：归档循环抓到的每页响应写入 Raw 层（脱敏、SHA-256 去重）；
3. 统一模型核心表（source_states / users / dynamic_sources / merge_logs / media_items / 评论点赞行表；`archive_dynamics` 加列）；
4. 媒体下载器 `media.rs`：断点续传（Range + `.part`）、重试退避、限速、magic bytes 类型验证、SHA-256 去重、状态记录、任务级 暂停/继续/取消/进度；
5. 三种下载模式（data-only / images / full）配置与命令；
6. 前端最小接入：设置页加「媒体下载模式」，任务页加「媒体下载」入口与进度；
7. 测试：migration 升级旧库、raw 留存去重、媒体下载器（本地 mock 服务器）、指纹合并工具函数；
8. 门禁：`cargo fmt --check`、`cargo check`、`cargo test`、`npm run build`。

### Phase 2 — 多数据源适配层与同步
- 独立适配层（`src-tauri/src/sources/`）：数据源 trait + 注册表 + 分页/限流/重试复用；
- 先把已确认源（相册列表 `fcg_list_album_v3`、回收站）迁入适配层；
- 在真实账号验证 3.2 中的待验证源后逐个接入（说说/照片/留言/详情）；
- `source_states` 驱动：每源独立游标、最后同步时间、错误；增量同步（新内容优先）；
- 切换账号前停止任务；远端删除 → `remote_status` 标记（不删本地）。

### Phase 3 — 完整性审计
- 审计命令 + 页面：动态总数/年份分布/来源分布/媒体下载状态/失败项/重复记录/分页缺口/缺正文或缺媒体/评论数不一致/疑似缺失时间区间；
- 明确区分「已发现内容的完整率」与「整个账号的绝对完整率」，绝不承诺无法验证的全量。

### Phase 4 — 开放格式导出
- 离线 ZIP（媒体相对路径打包）、JSON、Markdown（Obsidian 兼容，`![[...]]` 或相对路径图）；
- 从导出包重新导入（还原 Raw + 重建数据库）。

### Phase 5 — 安全加固收尾
- 系统安全存储「保持登录」（`keyring`，桌面平台）；
- 日志/导出诊断全面脱敏（含 UIN）；
- 全量回归 + 文档更新。

---

## 6. 预计修改与新增的文件

**修改（存量）**
- `src-tauri/src/archive.rs` — `open_database` 改用 migration 框架；`save_page` 顺带写 Raw；任务/媒体命令接入新层；错误日志去 UIN。
- `src-tauri/src/lib.rs` — 注册新状态（媒体下载管理器）与新 command。
- `src-tauri/Cargo.toml` — 增加 `sha2`、`hex` 等依赖。
- `src/utils/qzone.ts` — 新增媒体下载/审计相关 invoke 封装。
- `src/views/SettingsView.vue` / `src/views/TasksView.vue`（或 MediaView）— 最小 UI 接入。
- `AGENTS.md` / `README.md` — 架构说明更新。

**新增**
- `src-tauri/src/db/mod.rs`（+ migrations）— migration 框架。
- `src-tauri/src/raw.rs` — Raw 留存层。
- `src-tauri/src/media.rs` — 媒体下载管理器（下载器 + 任务控制 + 记录）。
- `src-tauri/src/model.rs`（或 `merge.rs`）— 统一模型写入与合并去重（平台 ID + 指纹候选）。
- `src-tauri/src/sources/mod.rs` — 数据源适配层（Phase 2）。
- `src-tauri/src/audit.rs` — 完整性审计（Phase 3）。
- `src-tauri/src/export.rs` — ZIP/JSON/Markdown 导出（Phase 4）。
- 各模块 `#[cfg(test)]` — 迁移/解析/去重/下载测试。
- `docs/` — 数据源验证记录、导出格式规范。

---

## 7. 主要风险及回滚方案

| 风险 | 影响 | 缓解 | 回滚 |
|---|---|---|---|
| migration 破坏旧库 | 已有用户数据不可读 | migration 全部只增不改、事务化、`quick_check` 前置；测试用「旧版建库 → 升级」夹具 | 数据库文件本身不修改（新表/新列），删新表或降级 `user_version` 即回滚；git 可回到升级前 commit |
| 媒体下载器影响现有浏览 | `load_archived_image/video` 行为变化 | 第一阶段下载器独立，`load_archived_*` 保留原逻辑，仅共享类型验证/哈希工具 | 不动 `load_archived_*`，随时可回退 |
| Raw 层体积膨胀 | 数据库过大 | body 去重（SHA-256）、可配置保留策略（后续阶段加清理/压缩） | 新表可删，不影响业务 |
| 未验证接口被误接入 | 请求失败/风控 | Phase 1 不调用未验证接口；适配层插槽 + 「需验证」清单把关 | 无网络行为变化即天然安全 |
| 并行写库竞态（归档 + 媒体下载 + Raw） | 锁竞争/写冲突 | rusqlite 单连接 + WAL + 短事务；下载器独立连接按批写入 | 下载器可随时暂停/取消 |
| 前端改动破坏现有 UX | 用户困惑 | UI 只做增量（新设置项/新卡片/新入口），沿用 `surface-card` 风格 | 独立 commit，可 cherry-pick 移除 |

---

## 8. Phase 1 验收门禁（每阶段执行）

```bash
cargo fmt --check          # 格式
cargo check                # 编译
cargo test                 # 单元测试（迁移/解析/去重/下载器）
npm run build              # vue-tsc + vite 前端构建
```

> 安全红线：不在代码/日志/测试数据/提交记录中写入真实 Cookie、QQ 号或个人信息；测试用占位 UIN；日志继续脱敏 Cookie 与凭证参数。

---

## 9. Phase 2 完成记录（多数据源适配层与同步）

**已交付**
- `src-tauri/src/sources/mod.rs` — 数据源适配层：`source_states` 读写（每源独立游标/状态/时间/错误/统计）、
  通用请求频率保护（每 10 分钟 300 次，`reserve_request_slot`，archive.rs 复用）、
  `list_source_states_command` / `reset_source_state_command`。
- `src-tauri/src/sources/albums.rs` — 相册列表源（`fcg_list_album_v3`，已确认接口）：
  容忍式字段候选解析（与前端 RecycleBinView 一致：`albumList`/`albumId`/`name`/`num` 等），
  写入 `albums` 表；**本次未出现的相册标记 `remote_status='remote_deleted'`，不删除本地数据**；
  再次出现自动恢复 `active`。含 `sync_album_list_command`。
- 互动列表（feeds）接入数据源层：归档循环每页写入 `source_states('feeds')`（游标+统计），
  开始/完成/失败/限流/取消均更新状态。
- **增量同步**：`start_feed_archive` 新增 `incremental` 参数；连续命中
  `INCREMENTAL_EXISTING_THRESHOLD(100)` 条已归档内容即停止扫描更早记录；
  首次全量（库空）不受影响；设置页可关闭（全量模式）。边界函数有单测。
- **切换账号守卫**：`QLoginState` 追踪 `active_uin`；登录成功且账号变化时自动停止
  归档与媒体下载任务（`request_cancel`）；归档循环内每轮校验 `qzone_auth().uin`，
  中途切号立即停止。任务错误日志移除 `ownerUin` 字段（隐私）。
- 数据库 v3 迁移：`albums` 表补 `remote_status` / `last_seen_at`（幂等补列）。
- model.rs 提供远端状态工具（`mark_dynamic_remote_status` / `count_dynamic_remote_status`，
  留待 Phase 3 审计页使用）。
- 前端：任务页新增「数据源同步状态」区（状态/统计/同步相册列表/重置）、设置页新增
  「增量同步」开关（默认开）。

**未接入（诚实边界）**：本人说说、相册内照片、留言板、单条动态详情、原图地址——
无本仓库实现与真实响应样本，等真实账号验证后按 `albums.rs` 模式接入
（source_states 驱动 + 容忍解析 + 消失标记）。

**验证**：`cargo fmt --check` ✅｜`cargo check`（零警告）✅｜`cargo test` 61 通过 ✅｜
`npm run build` ✅。新增测试：source_states 读写/重置、限流窗口、相册容忍解析、
远端消失标记（含恢复 active）、增量边界累计。
