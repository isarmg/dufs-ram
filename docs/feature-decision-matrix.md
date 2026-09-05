# 完整功能与取舍清单：开发者决策矩阵

本文是[完整功能与取舍清单](feature-inventory-and-tradeoffs.md)的规范化索引。前者解释协议细节、状态机和已知边界；本文保证每个可独立决策的功能都有唯一 ID、实现锚点、分类、复杂度、删除后果以及验证与边界。开发者不得只删除 UI、单一路由或一项测试后宣称能力已移除。

分类只允许 `核心/保障/可选/建议保留/开发运维`；复杂度只允许 `低/中/高`。复杂度是完整删除或改变能力的影响面，不是实现 happy path 的工时。

## 1. 产品、平台与仓库边界

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-001 | 单进程管理一个共享根；同根第二实例被 advisory `flock` 拒绝 | `src/main.rs`、`src/server/rooted_fs.rs` | 核心 | 高 | 产品失去清晰数据域；误启多实例会越过进程内协调 | 同根双启失败、不同根可独立启动；锁不能阻止 shell/宿主写入 |
| DFM-002 | 服务端唯一 target 为 `x86_64-unknown-linux-gnu` | `build.rs`、`sarmg-server-target` | 保障 | 高 | 未证明的 CPU/OS/ABI 进入交付矩阵 | 精确 target 正例；aarch64、musl、Windows、macOS 负例；无 best-effort |
| DFM-003 | Linux `openat2` 必需，不降级到路径拼接 | `src/server/rooted_fs.rs` | 保障 | 高 | 共享根逃逸和 TOCTOU 安全证明失效 | 缺系统调用启动失败；symlink、magic link 和路径竞态测试 |
| DFM-004 | Dufs 是项目组唯一非 React/Vite 例外，保留原生 ES modules | `clients/web/`、`src/server/assets.rs` | 建议保留 | 高 | 改框架需重做嵌入、CSP、缓存、构建和供应链 | 生产无 Node 服务/无 bundler；例外不改变 Foundation 认证合同 |
| DFM-005 | 客户端统一在 `clients/web/`，源码配置在 `config/`，部署资产在 `deploy/` | 三个同名目录 | 开发运维 | 中 | 代码与资产散落，登记和审查易漏 | 新生产模块进入 asset registry；不复制当前配置/部署模板 |
| DFM-006 | 生产配置故意使用 `/etc/dufs/dufs.yaml`，TLS 使用 `/etc/dufs/tls/` | `deploy/dufs.service`、`deploy/nginx-dufs.conf` | 开发运维 | 中 | 随意换路径会破坏 systemd/nginx 权限约定 | 这是 FHS/TLS 权限例外，不是旧路径兼容 |
| DFM-007 | current-only：只接受当前配置、API、页面和 SQLite identity | `src/args.rs`、`src/server/router/`、`src/server/state_store/database.rs` | 保障 | 高 | alias/fallback 令分支、攻击面和测试矩阵增加 | 非当前字段、路径、schema 在修改状态前拒绝；未来稳定版本的精确迁移边只进入 `sarmg-upgrade` 的独立 adapter/fixture/CLI |
| DFM-008 | 外部 HTTPS 网关终止 TLS，Dufs 只做明文 HTTP/1 回源 | `src/main.rs`、`deploy/nginx-dufs.conf` | 保障 | 高 | 直接公网暴露会泄露凭据；内置 TLS 则新增证书生命周期 | 默认回环；HTTP/1.0/1.1；无 h2c；浏览器入口必须 HTTPS |
| DFM-009 | 现代桌面浏览器是唯一 UI 支持范围 | `clients/web/index.css`、`playwright.config.js` | 核心 | 中 | 扩到移动端会新增布局、输入、性能与测试合同 | Chromium/Firefox、320 CSS px 缩放回流；不承诺手机产品体验 |

## 2. 启动、配置与资源

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-010 | 严格 YAML，未知字段拒绝；CLI 显式值整体覆盖对应 YAML | `src/args.rs`、`config/dufs.yaml.example` | 保障 | 中 | 拼写错误可静默变成错误配置 | unknown/duplicate/empty/precedence；无环境变量配置 |
| DFM-011 | 配置文件 no-follow、安全 owner/mode、单硬链接和身份复核 | `src/args.rs` | 保障 | 高 | 服务可能读取攻击者替换或公开可写配置 | symlink/ACL/mode/owner/hardlink/rename race；最大 1 MiB |
| DFM-012 | `serve-path` canonicalize 并作为唯一共享根 | `src/args.rs`、`src/server/rooted_fs.rs` | 核心 | 高 | 所有浏览、写入和路径隔离失去边界 | 不存在/非目录拒绝；相对路径按 cwd；根 fd identity |
| DFM-013 | `state-dir` 必填、私有 `0700`，与根/配置/日志分离 | `src/args.rs`、`src/server/state_store.rs` | 保障 | 高 | 重启证据丢失或敏感状态泄露 | owner/mode/symlink/祖先关系/object alias/sidecar 冲突 |
| DFM-014 | 一个或多个明确 IP listener，共享全局连接许可 | `src/main.rs`、`src/args.rs` | 可选 | 中 | 收敛为单地址会失去 IPv6/多网卡；删许可会失去资源上限 | bind 非空无重复；IPv6 v6-only；全部绑定后才 accept |
| DFM-015 | 生产 HTTPS，显式开发仅 loopback | Foundation Origin Mode、`src/args.rs` | 保障 | 高 | 错误信任代理头可破坏认证边界 | 忽略 XFF/XFP；开发 wildcard 拒绝 |
| DFM-016 | 连接、请求头、普通请求与流式 I/O deadline | `src/main.rs`、`src/server/router.rs`、`src/server/download.rs` | 保障 | 高 | 慢连接和停滞 I/O 可无限占资源 | 默认 256 连接；10 秒/64 KiB header；300 秒请求；30 秒 stream idle |
| DFM-017 | blocking-I/O 有界，许可活到真实 syscall 返回 | `src/server/blocking_io.rs` | 保障 | 高 | 故障 FUSE/NFS 可挤占 Tokio 或无界占满 pool | 64 默认；取消等待者不遗留任务；已开始 syscall 不提前归还 |
| DFM-018 | 访问日志格式、元素、单行转义和队列都有预算 | `src/http_logger.rs`、`src/logger.rs` | 可选 | 中 | 删除定制会失去字段；删除预算会允许注入/巨型分配 | 4096-byte 格式、128 元素、16 KiB 行、4096 队列、Secret 脱敏 |
| DFM-019 | 文件日志安全 no-follow 打开；默认统一 stderr | `src/logger.rs`、`src/args.rs` | 可选 | 中 | 无文件 sink 时依赖 journald；弱化检查可能泄密 | owner、`0600`、单链接、append；不自动 reopen/逐条 fsync |
| DFM-020 | `hash-password` 生成唯一当前 Foundation Argon2id PHC | `src/main.rs`、`src/auth.rs` | 建议保留 | 低 | 需外部工具精确复现参数 | v19/m19456/t2/p1/salt16/output32；密码 12～1024 bytes |
| DFM-021 | 配置至少一个、最多 1024 个管理员 username/PHC | `src/args.rs`、`src/auth.rs` | 核心 | 中 | 零管理员无法登录；无上限扩大启动资源 | canonical username、重复拒绝、当前 PHC；无 role/path rule |

## 3. Foundation 管理员认证

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-022 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-023 | 管理员 username 使用 Foundation 唯一 current canonical 规则 | `src/auth.rs`、`clients/web/login.js` | 保障 | 中 | 大小写、Unicode、`@` 或边界差异会破坏跨项目身份与 owner 摘要 | 配置为 3～64 lowercase ASCII bytes、首尾 alnum、字符 `[a-z0-9._-]`；登录 candidate 为 1～64 bytes 且每字节 `0x20`～`0x7e`，trim/lowercase 后再校验；相邻分隔符允许 |
| DFM-024 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-025 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-026 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-027 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-028 | 严格同源覆盖登录及所有已认证 unsafe method | `sarmg-admin-auth`、`src/server/router/dispatch.rs` | 保障 | 高 | Cookie 可被跨站诱导；重复头造成解析分歧 | Origin/effective Host/Sec-Fetch-Site 均必需、唯一、一致；生产 HTTPS |
| DFM-029 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-030 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-031 | 平台安全 Cookie | Foundation Core/Hyper | 保障 | 中 | Cookie 属性弱化扩大盗用风险 | 生产 __Host-sarmg-dufs-ram-session；开发 sarmg-dufs-ram-session；Set/Clear 一致 |
| DFM-032 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-033 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-034 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-035 | Foundation 管理员控制面 | Core/Static/Hyper/Auth/contracts；产品仅配置与 HTML Adapter | 保障 | 高 | 产品副本会造成平台策略漂移 | 见功能清单 A-01～A-20；共享 wire 与真实产品 HTTP 回归 |
| DFM-036 | session 查询/注销固定 Foundation 路径 | `src/server/router/dispatch.rs`、`clients/web/modules/operations/file_operations.js` | 核心 | 中 | 无注销则只能等过期；私有路径令集成漂移 | GET `/api/v2/auth/session`；POST `/api/v2/auth/logout` 要 CSRF/同源 |

## 4. 浏览、下载与普通写操作

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-037 | 目录 HTML 骨架与分页 list API 分离 | `src/server/listing.rs`、`clients/web/modules/listing/controller.js` | 核心 | 高 | 无法有界浏览大目录 | 首屏/空/不存在/500项页/no-store/严格 JSON |
| DFM-038 | 列表/搜索快照有 TTL、全局/管理员容量和 cursor 绑定 | `src/server/listing/snapshot.rs` | 保障 | 高 | 每页重复扫描或 cursor 跨身份泄漏 | TTL120 秒；32/64MiB 全局、8/32MiB 每管理员；重启失效 |
| DFM-039 | 有界递归搜索、显式 DFS 与目录变化复核 | `src/server/listing/walk.rs` | 建议保留 | 高 | 只能逐层浏览；弱预算会耗尽内存 | 条目/深度/32MiB、取消、前后 identity；不是原子快照 |
| DFM-040 | 下载只以 attachment 返回单个普通文件 | `src/server/download.rs` | 核心 | 中 | 文件无法取回；inline 扩大主动内容风险 | GET/HEAD/MIME/Disposition；无预览、无目录 ZIP |
| DFM-041 | 单段 Range 与条件请求 | `src/server/download.rs`、`tests/range.rs` | 建议保留 | 高 | 大文件续传与缓存前置条件下降 | 单 Range、416、多段拒绝、If-*、ETag；流限制打开时长度 |
| DFM-042 | 根 fd 相对路径类型与规范 URI 校验 | `src/server/path_policy.rs`、`src/server/rooted_fs.rs` | 保障 | 高 | 目录穿越、编码别名和 symlink 逃逸 | UTF-8/组件/PATH_MAX、重复斜杠、编码分隔符、根内链接 |
| DFM-043 | 路径协调器按父子关系序列化冲突 mutation | `src/server/path_coordinator.rs` | 保障 | 高 | rename/delete/upload 可交错 | 父/子、同路径、公平、deadline；不控制外部 writer |
| DFM-044 | mkdir 通过 browser API 与 Operation ID 创建 | `src/server/browser_api.rs` | 核心 | 中 | 网页无法建立目录 | JSON、basename/path、冲突、权限、operation replay |
| DFM-045 | Move 只改变父目录且目标必须是已存在目录 | `src/server/browser_api.rs`、`clients/web/modules/operations/file_operations.js` | 核心 | 高 | 无法整理目录；隐式 mkdir 改变语义 | 同/跨父目录、跨 filesystem、目标类型、same inode |
| DFM-046 | Rename 只改变 basename | `src/server/browser_api.rs`、`clients/web/modules/listing/controller.js` | 核心 | 高 | 无法改名；与 Move 合并会增加误移动 | basename、覆盖、source/target revision、焦点恢复 |
| DFM-047 | Delete 持久化 outbox、移入隐藏 trash，再递归 purge | `src/server/delete.rs`、`src/server/purge.rs` | 核心 | 高 | 直接递归删除丢失崩溃恢复证据 | file/dir/link、fsync、Prepared/Ready/Claimed、restart/quarantine |
| DFM-048 | mutation 用规范 UUID Operation ID 与请求指纹幂等 | `src/server/operation_registry.rs`、`src/server/state_store/operation.rs` | 保障 | 高 | 超时重试可能重复执行或无法判断 | same ID/same bytes replay；different fingerprint 409；TTL/容量 |
| DFM-049 | job API 查询 `running/succeeded/failed/unknown` | `src/server/router/dispatch.rs`、`clients/web/modules/http/client.js` | 保障 | 中 | 504/断线后只能猜或盲重发 | jobs/<uuid> owner 绑定、过期 404、只查原 ID |
| DFM-050 | detached commit 与明确 mutation boundary | `src/server/router.rs`、`src/server/operation_registry.rs` | 保障 | 高 | Future 取消被误当成磁盘回滚 | boundary 前撤预留；之后超时/错误为 unknown；后台收尾 |
| DFM-051 | Dufs browser API 错误统一 RFC 9457 Problem Details | `src/server/problem.rs`、`clients/web/modules/http/client.js` | 保障 | 中 | 前端按 message 猜分支或与 Foundation 错误混用 | canonical media type/status/code；平铺 operation/upload 扩展 |

## 5. 上传、覆盖与磁盘保障

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-052 | 批量 preflight 返回存在性、可替换性和 target revision | `src/server/browser_api.rs`、`clients/web/modules/upload/preflight.js` | 建议保留 | 高 | 冲突只能传输后发现；页面可能盲目覆盖 | 顺序绑定、路径预算、missing/existing/special；预检不是锁 |
| DFM-053 | PUT 新建、PATCH 续传、HEAD 查询绑定同一 Upload ID | `src/server/upload.rs`、`src/server/upload/protocol.rs` | 核心 | 高 | 删除 PATCH 后断线全量重传；删除 ID 后无法确认 | UUID/length/offset/owner/restart/method/status/header 矩阵 |
| DFM-054 | Running/AwaitingConfirmation/Committed/Rejected/Unknown 持久状态 | `src/server/upload/record.rs`、`src/server/state_store/upload.rs` | 保障 | 高 | 重启或断网后无法分辨检查点与终态 | 合法转换、terminal replay、7天 TTL、管理员/全局容量 |
| DFM-055 | stage 在目标父目录私有 `0700` 目录并同文件系统发布 | `src/server/upload/prepare.rs`、`src/server/internal_names.rs` | 保障 | 高 | 跨设备 rename 不原子；公开 stage 泄露半文件 | no-follow/owner/mode、隐藏、orphan maintenance；名称仅 current |
| DFM-056 | create-only 用 `RENAME_NOREPLACE` 并发布后核对 identity | `src/server/upload/commit.rs` | 保障 | 高 | 晚到目标可能被静默覆盖 | missing/late occupant/post-publish swap/parent sync；不明时报 unknown |
| DFM-057 | 覆盖必须携带绑定路径、管理员和目标 identity 的 revision | `src/server/upload/target.rs`、`src/server/browser_api.rs` | 保障 | 高 | 陈旧页面可覆盖并发更新 | 64 lowercase hex、overwrite=true 才允许、rename 紧前复核；非外部 CAS |
| DFM-058 | 覆盖保留 numeric uid/gid、非特权 mode/xattr，拒绝特权 metadata | `src/server/upload/target.rs`、`src/server/upload/commit.rs` | 保障 | 高 | 静默改变 ACL/xattr，或复制 capability/SELinux 形成提权 | 单链接普通文件；setuid/setgid/security.*/trusted.* 拒绝；预算 |
| DFM-059 | 正文按 offset 写、flush、长度确认与 `sync_all` | `src/server/upload/transfer.rs` | 保障 | 高 | 成功后可能未落盘，续传可产生洞或重复 | short write/EOF/offset/deadline/fault/zero-byte |
| DFM-060 | idle/total deadline；首次 mutation 与 total deadline 原子竞争 | `src/server/upload.rs`、`src/server/upload/transfer.rs` | 保障 | 高 | 超时后后台仍可能开始写，或慢客户端永久占槽 | 默认 60秒/24h；not-started vs unknown；PATCH 重计时 |
| DFM-061 | 服务端上传槽与前端有界队列 | `src/server/upload.rs`、`clients/web/modules/upload/queue.js` | 保障 | 中 | fd/内存/磁盘并发失控；过小则吞吐下降 | server 默认4；UI queue/cancel/order；429 不读正文 |
| DFM-062 | 按实际 stage `st_dev` 的空间预留账本 | `src/server/disk_space.rs` | 保障 | 高 | 并发上传可共同写穿最低水位 | f_frsize 取整、metadata 余量、8MiB 重检、overflow、8次 revision 重试 |
| DFM-063 | fresh PUT 建 stage 前检查路径 upload/purge 持久义务 | `src/server/upload/prepare.rs`、`src/server/state_store.rs` | 保障 | 高 | 新写入会切断待恢复/清理路径 | 409/503/408 not-started；keyset 分页；不建本次 stage/记录 |
| DFM-064 | AwaitingConfirmation 保留满 stage，空 PATCH+新 revision 或 discard | `src/server/upload/commit.rs`、`clients/web/modules/upload/manager.js` | 建议保留 | 高 | 晚到冲突必须重传；自动覆盖破坏条件写 | target changed/missing with old metadata/discard idempotence/restart |
| DFM-065 | 客户端异常响应只用原 ID HEAD，不盲重放 PUT/PATCH | `clients/web/modules/upload/transport.js`、`upload/protocol.js` | 保障 | 高 | 网络错误可能重复提交或覆盖 | ID/state/length/offset 严格矩阵；unknown 暂停并人工处理 |

## 6. SQLite、后台维护与生命周期

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-066 | StateStore 专用 OS 线程串行拥有 SQLite connection | `src/server/state_store/actor.rs` | 保障 | 高 | Tokio 被 SQLite 阻塞；多连接次序更难证明 | 有界 command；独立 control/wake；单命令失败不杀 actor |
| DFM-067 | rollback `DELETE`、`synchronous=EXTRA`、FK/defensive/trusted_schema off | `src/server/state_store/database.rs` | 保障 | 高 | 崩溃持久性与不可信 schema 防护下降 | pragma 精确值、write transaction、quick check、sidecar |
| DFM-068 | 五列 `product_metadata` 与 Foundation schema fingerprint | `src/server/state_store/database.rs` | 保障 | 高 | 错产品/版本/对象漂移被当当前库写入 | application/version/revision/fingerprint/root binding 精确；额外对象拒绝 |
| DFM-069 | SQLite 打开前验证主库和固定 sidecar identity | `src/server/state_store/database.rs` | 保障 | 高 | SQLite 跟随 symlink/特殊文件/替换竞态 | lstat/open nofollow/fstat/前后复核；主库缺失时孤立 sidecar 拒绝 |
| DFM-070 | 现存库从 no-follow fd 建 raw baseline，再验原路径视图 | `src/server/state_store/database.rs` | 保障 | 高 | 热 sidecar/路径替换可逃过只读预检 | baseline 与 merged view 都 current；失败前不 chmod/恢复写 |
| DFM-071 | 启动恢复按最后可靠状态保守转换 | `src/server/state_store/database.rs`、`src/server/maintenance.rs` | 保障 | 高 | 把 running 一律失败会抹掉可能已提交事实 | operation CommitStarted→unknown、upload、purge Claimed→Ready |
| DFM-072 | purge 每 slice 限条目/时间并持久 cursor/backoff | `src/server/purge.rs`、`src/server/rooted_fs/purge.rs` | 保障 | 高 | 大删除长期独占，或重启反复从头扫描 | 256项/25ms、100ms～30s、fd-relative nofollow、restart |
| DFM-073 | trash identity 异常进入永久 quarantine，不猜测删除 | `src/server/purge.rs`、`src/server/internal_names.rs` | 保障 | 高 | 错对象可能被递归删除；不隔离则 worker 循环 | quarantine hold 不被 maintenance 扫描；停服人工调查 |
| DFM-074 | authenticated readiness 真实写根并做 SQLite 回滚写事务 | `src/server.rs`、`tests/health.rs` | 建议保留 | 中 | 探针只能证明端口可连 | 需管理员 session；不做 rename/介质读回，不等于 CRUD |
| DFM-075 | 两阶段优雅停机与约40秒硬截止 | `src/main.rs`、`src/server.rs` | 保障 | 高 | 立即退增加 unknown；无硬截止会被故障 I/O 拖死 | 30秒+10秒；第二信号/SIGKILL；正常日志 flush 最多5秒 |

## 7. 原生前端、质量与交付

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-076 | 名称/MIME/内容共同生成 SHA-256 URL并编译嵌入 | `src/server/assets.rs`、`clients/web/` | 建议保留 | 高 | 页面/代码版本漂移，长期缓存返回旧模块 | registry/目录双向一致；GET/HEAD；immutable 仅精确资源 |
| DFM-077 | 登录使用同源外部 ESM，平台字体与许可证嵌入 | `administrator_web.rs`、`login.js`、平台 Vite | 保障 | 中 | inline/eval 或外站字体扩大注入与网络依赖 | 精确资产摘要、CSP、preload、真实字体字节 |
| DFM-078 | HTML 仅含两个业务字段，独立恢复共享 Session | `listing.rs`、`shared/index_data.js`、Foundation Admin Client | 保障 | 中 | 嵌入凭据或本地合同可造成泄露和漂移 | 严格 own data、Foundation session guard、frozen copies |
| DFM-079 | DOM 通过 textContent/安全属性构造业务内容 | `clients/web/modules/shared/dom.js` | 保障 | 中 | 文件名/错误文本可成为可执行 HTML | 注入 payload、禁动态 HTML API、CSP；静态 SVG 例外 |
| DFM-080 | DOM window、列表状态和 mutation invalidation 有界 | `clients/web/modules/listing/controller.js`、`shared/mutation_effect.js` | 建议保留 | 中 | 大列表无限 DOM；写后继续显示陈旧结果 | 200 window、cursor、四种 effect、focus/scroll |
| DFM-081 | 原生 dialog、键盘、focus return、live region、forced colors | `clients/web/modules/operations/dialogs.js`、`clients/web/index.css` | 建议保留 | 中 | 键盘/低视力用户无法可靠操作 | Chromium/Firefox、axe标签、Escape、320px；非完整 WCAG 声明 |
| DFM-082 | TypeScript strict `checkJs`+JSDoc，外部输入为 unknown | `clients/web/tsconfig.json`、`scripts/check-js.mjs` | 开发运维 | 中 | 前端协议漂移更晚发现 | 无 any；runtime guard 仍必需；不等于迁移 `.ts` |
| DFM-083 | Acorn AST 安全门与内置正负对抗样例 | `scripts/check-js.mjs` | 开发运维 | 高 | 动态 HTML/prompt/反射别名绕过文本搜索 | computed/destructure/alias/reflect；非通用污点证明 |
| DFM-084 | 统一 Rust/JS/浏览器/部署/文档/审计门 | `scripts/check.sh`、`scripts/check-deployment.sh` | 开发运维 | 高 | 协议、路径、样例和依赖漂移进入发布 | fmt/clippy/test/coverage/audit/checkJs/Playwright/nginx/systemd |
| DFM-085 | exact-source、vendor 构建、SBOM/notice/checksum/signature | `scripts/package-release.sh` | 开发运维 | 高 | 无法证明源码/依赖/制品一致或发现篡改 | clean tag=version=HEAD、SHA、no-clobber、强算法、独立固定公钥 |
| DFM-086 | 中文 README/指南/流程/功能/运维文档是交付合同 | `README.md`、`docs/`、`scripts/check-docs.mjs` | 开发运维 | 中 | 开发者凭旧经验重建已删除兼容分支 | 只保留五类；links/anchors；协议/路径变更同步 |

## 8. 明确不实现的能力

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| DFM-087 | 不提供匿名文件访问 | `src/server/router/dispatch.rs` | 保障 | 高 | 若删除限制会直接暴露共享根 | 仅 health、login page/API、摘要资源公开；普通内容 401/303 |
| DFM-088 | 不提供角色分级、路径权限或租户隔离 | `src/auth.rs`、`src/server/administrator_web.rs` | 核心 | 高 | 新角色需完整授权矩阵/UI/审计/Foundation 合同 | 全管理员全根同权；隐藏按钮不是授权 |
| DFM-089 | 不提供 WebDAV、CORS 或通用第三方 API | `src/server/router/` | 可选 | 高 | 新增会扩大 method/lock/cache/cross-site/兼容矩阵 | unknown internal 404、known wrong method 405；无 CORS 承诺 |
| DFM-090 | 不提供 preview/edit/static site/SPA fallback | `src/server/download.rs`、router | 可选 | 高 | 新增会执行/渲染不可信内容并改变 CSP/MIME | 下载 attachment；未知路径不回退 index；内容不 inline |
| DFM-091 | 不提供目录 ZIP、多段 Range 或空目录上传 | `src/server/download.rs`、`upload/selection.js` | 可选 | 高 | 新增需归档预算、流错误和目录 metadata 合同 | 仅单文件/单 Range；webkitdirectory 只产生文件 |
| DFM-092 | 不提供运行时 assets 覆盖或前端独立部署 | `src/server/assets.rs` | 建议保留 | 高 | 后端与页面协议版本可错配 | 生产不读工作区；修改后重编译/重启 |
| DFM-093 | 不提供内置备份、恢复、迁移或自动回滚 | `src/`、`docs/operations.md` | 开发运维 | 高 | 转换塞回服务会增加旧格式分支与恢复风险 | 停服一致备份/演练由运维；未来历史转换只在 `sarmg-upgrade` 的精确 edge 中实现 |
| DFM-094 | 不提供多节点共享同一根或分布式协调 | `src/server/path_coordinator.rs`、root flock | 可选 | 高 | 新增需 distributed lock/fencing/shared state 证明 | 本机同根第二实例拒绝；外部 writer 由部署排除 |

## 9. 使用规则

删除或改变任一行前，设计评审至少回答：哪些代码锚点被删或替换；配置/API/持久状态是否还有入口；删除后果是否被产品接受；正向、负向、竞态、崩溃和运维验证由哪一层承担；是否意外引入旧版本 fallback。横跨多个 ID 的改动应作为一个原子版本合同交付，不保留半套兼容层。
