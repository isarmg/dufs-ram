# 更新日志

本文件记录本项目的所有重要变更。

## 未发布

## [0.47.0] - 2026-07-24

### 变更

- 将本地包版本更新为 `0.47.0`，移除上游作者元数据并保持 `publish = false`；版本管理只使用本地 Git。
- 将项目升级到 Rust 1.97.1 与 Rust 2024 edition；`Cargo.toml` 通过 `rust-version` 声明最低版本，`rust-toolchain.toml` 固定本地 `rustc`、Cargo、Clippy 和 Rustfmt 工具链。
- 将 Dufs 服务端收敛为仅支持 64 位 Linux；仓库根目录的 `build.rs` 会明确拒绝其他 Cargo 目标。运行时还要求 Linux 5.6 或更高版本提供 `openat2`，禁用该系统调用或不支持的环境会在启动阶段失败，不会降级。
- 明确每台系统只运行一个 Dufs 实例；文档不再设计多实例协调或跨进程文件锁。新增进程内子树路径协调器，PUT、PATCH、DELETE、mkdir 和 move 的源/目标统一参与：同路径及祖先/后代写操作串行，互不为祖先的不同子树可以并行，多路径租约统一排序避免死锁。等待租约期间只要文件系统版本变化，就重新解析符号链接语义键；别名从一个目录改指另一个目录时不会沿用过期身份。它支持个人多设备同时进行无冲突操作，但不约束 shell 或其他进程。
- 服务启动时长期持有共享根目录 fd；`RootedFs` 的最终文件打开、创建、替换、移动和删除通过 `openat2` 以及父目录 fd 上的 `openat/mkdirat/renameat2/renameat/unlinkat` 完成，目录持久化使用 fd 上的 `fsync`。可能创建祖先目录的操作共享短临界区，并覆盖最终目录发布和父目录同步，避免兄弟请求同时创建共同父目录时由尚未完成的另一请求承担落盘责任。相关调用固定使用 `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`：解析后仍在根内的相对符号链接可用，绝对链接和根外目标会被拒绝；悬空或成环的根内相对链接可在目录中显示，并可用 DELETE 删除或 PUT 替换，GET 仍返回 `404`。
- move 的 `overwrite: false` 使用父目录 fd 上的 Linux `renameat2(RENAME_NOREPLACE)` 原子提交：最终目标存在时返回 `409`，Linux 内核或文件系统不支持该原语时失败关闭，不回退到可能覆盖目标的普通 rename。`overwrite: true` 使用 `renameat` 原子替换；两种成功路径都在 `204` 前 `fsync` 源和目标父目录。
- 移除了生产运行的全部 `DUFS_*` 环境变量配置入口；运行配置现在只来自可选 YAML 和命令行，命令行值覆盖 YAML。Playwright 内部测试变量不属于生产配置，继续保留。
- YAML 配置启用严格未知字段检查；拼写错误以及 `allow-symlink` 等已经删除的字段会指出配置文件和字段并阻止启动，不再被静默忽略。
- 移除了内置文件预览和文本编辑器；共享文件改为以附件形式下载。
- 简化了认证机制，使每个已配置账号都拥有共享根目录的完整访问权限；账号配置固定为 `user:<argon2id PHC>`。
- 移除了匿名路径规则、账号级角色、`-A` 以及上传、删除、搜索、归档和哈希权限开关；同时彻底移除 `--allow-symlink` 及 YAML 同名配置，所有账号虽然拥有完整文件管理能力，但始终不能通过符号链接越过共享根。
- 移除了匿名访问；启动时现在要求至少配置一个账号。
- 用内置中文 HTML 表单（`GET/POST __dufs__/login`）、真正的 `POST __dufs__/logout` 端点和 Argon2id 密码验证取代了 Basic/Digest 认证；账号配置只接受完整、有效的 Argon2id PHC，明文、SHA-crypt、其他 Argon2 变体和无效 PHC 均会被拒绝。配置与登录表单共用 128 字节用户名上限。`dufs hash-password` 可交互生成所需 PHC，配置错误只报告账号序号和错误类型，不回显完整账号输入。
- 登录页采用与 UnionC 通用内容块一致的 3:2 六行圆角卡片：账号、密码、错误提示和登录操作各占固定行，并保留现代桌面浏览器的浅色、深色适配。
- 登录页保留账号和密码的必填语义，但关闭浏览器原生校验气泡；空账号或空密码由服务端拒绝，并将“请填写账号和密码”显示在卡片第五行。
- 登录失败改为 POST/Redirect/GET：失败 POST 保存一条最多存活 60 秒的一次性错误状态，并以 `303` 跳转到携带随机 256 位 nonce 的登录页；首次 GET 原子消费并显示错误，刷新只重复 GET 且不再显示提示。内存状态最多 1024 条，只包含固定错误类型和创建时间，不保存账号或密码。
- 增加了随机 256 位服务端会话，其令牌仅以摘要形式存储；会话具有 30 分钟空闲过期时间、12 小时绝对过期时间和 1024 个会话的数量上限，并会在进程重启时有意失效。
- 通过带有 `Secure`、`HttpOnly`、`SameSite=Strict`、`Path=/` 且不设置 `Domain` 的 `__Host-dufs-session` Cookie 设置会话；文件、ZIP 归档和单段范围下载现在直接使用此 Cookie，不再使用 URL 令牌或自定义下载令牌。
- 每个受保护的 `POST`、`PUT`、`PATCH` 和 `DELETE` 请求现在都必须提供有效、随机、256 位且与会话绑定的 CSRF 令牌，并采用常量时间比较以及 `Origin`/`Sec-Fetch-Site` 来源检查；移除了 `CHECKAUTH`、自定义 `LOGOUT`、Basic/Digest、MD5/SHA-crypt、Ed25519 和 URL 令牌认证路径。
- 禁止 DELETE 共享根目录：普通文件路由在 metadata 和文件类型判断前拒绝根目标并返回 `403`，内部 mkdir/move 路径解析复用相同根守卫；回归测试覆盖默认根、`path-prefix`、编码等价路径、越界符号链接和普通子目录删除。
- 将文件和目录 DELETE 改为持久化可见删除：目标先在同一父目录内原子移动为 `.dufs-upload-delete-<UUID>.trash`，父目录 `fsync` 完成后才返回 `204`。递归空间回收在后台进行；失败或中断只留下不可见内部项，不恢复原名称。服务启动时立即清扫遗留 delete trash，运行期间维护任务每小时重试达到 1 小时阈值的遗留项。单个巨大 trash 一旦进入同步 `remove_dir_all` 就不能在目录树中途取消，`204` 不代表空间已释放，普通停机也可能等待该调用完成。
- 将并发 Argon2id 登录校验严格限制为两个：`OwnedSemaphorePermit` 由 blocking 任务持有到计算真正结束，外层请求取消不能提前释放槽位。密码校验与会话创建已经拆分，只有校验结果返回到仍存活的请求后才创建会话；满容量会话轮换会在同一把锁内预检新令牌、替换旧会话，不会额外淘汰无关会话。部署时仍必须由网关依据可信的真实客户端 IP 进行速率限制、保留 `Host`、强制使用外部 HTTPS，并隔离后端端口。
- 目录列表不再统计每个子目录，并以 `416` 拒绝多段范围请求；仍支持单段范围下载。
- 目录列表、搜索和 ZIP 不再把遍历、目录项读取、metadata 或非 UTF-8 名称错误静默转换成部分成功；并发消失、类型变化等遍历竞争返回可重试 `409`。本项目据此明确只支持 UTF-8 Linux 文件名，现有非 UTF-8 名称必须先在系统侧重命名。递归预算在单项 metadata 解析前执行，超大单目录也不会先被完整收集。ZIP 会先写入权限为 `0600` 且自动清理的临时文件，完整 `finalize` 后才返回 `200`、准确的 `Content-Length` 和归档正文，不再边生成边发送可能截断的成功响应。
- 文件下载改为从根 fd 打开一次文件，并从同一句柄取得 metadata、可选 MIME 样本和正文；已知二进制扩展名不再读取内容样本。metadata 预检后被另一设备并发删除或移动导致的 `ENOENT`/`ENOTDIR` 返回 `404`。显式 Range end 超出文件末尾时截断，超长 suffix 返回完整文件，多段仍返回 `416`。ETag 改为包含设备号、inode、长度和纳秒级 mtime/ctime 的弱验证器；带 `If-Range` 的请求返回完整 `200`，不带它的合法单段请求返回 `206`。
- 对所有登录和认证响应统一强制执行 `Cache-Control: private, no-store`，覆盖文件 GET/HEAD、单段 Range、条件响应、目录 ZIP、上传、API、错误和内部 `500`，且不依赖文件验证器是否可用；只有成功返回的版本化内置脚本、样式和图标保留公共长期缓存。下载文件名改为固定安全 ASCII `filename` 回退名和 UTF-8 `filename*` 真实名称，避免引号、反斜线、分号、控制字符及非 ASCII 名称形成歧义响应头。
- 将上传改为 Linux 崩溃持久性协议：完整 PUT 请求先暂存在目标文件旁，随后执行文件同步、同文件系统原子 rename 和父目录 `fsync`。这三个最终步骤成功即决定 HTTP 成功；后续检查点删除或其目录同步失败只记录告警并交给 TTL 维护重试，不再把已提交文件错误报告为 `500`。覆盖语义明确为发布新 inode、只复制普通 permissions；不保留 owner/group、POSIX ACL、扩展属性或硬链接身份，其他硬链接继续读取旧 inode 内容。该保证仍取决于 Linux 文件系统、NFS 等网络存储、设备和固件正确兑现同步请求。
- 整个上传处理从开始起进入独立 mutation task，并持有请求体、路径租约、活跃 stage/state 租约和清理责任；浏览器断开或网关取消外层 HTTP future 不会提前释放租约。mkdir、move 和可见删除的最终文件系统变更使用相同跟踪机制。新增取消回归测试验证外层等待被取消后，内层写事务仍阻塞冲突祖先路径直至实际结束。
- 将浏览器重试改为使用 UUID 上传会话：PUT 必须携带 `X-Dufs-Upload-Id` 和 `X-Dufs-Upload-Length`，PATCH 还必须携带与服务端持久化检查点完全一致的 `X-Dufs-Upload-Offset`。持久化辅助文件绑定初始总长度并只记录已同步的偏移量；HEAD 返回该检查点，PATCH 在恢复上传前截断所有未写入检查点的尾部数据。
- 删除浏览器跨刷新 `localStorage` 续传身份：文件名、长度和 `lastModified` 不能证明内容相同，重新选择文件始终使用新 ID 完整 PUT，避免把不同内容拼接到旧检查点。同一页面内的同一个 `File` 对象仍可保留 upload ID，失败重试先 HEAD 验证服务端持久化 offset，再 PATCH。
- PUT 或 PATCH 首次收到带 `X-Dufs-Auth-Error: csrf` 标记的 `403` 时会原子暂停整个前端队列，显示统一提示并禁止失效页面继续发出请求。普通网络错误、`5xx` 和没有认证错误标记的业务失败保留当前页面内的重试；刷新或重新登录后重新选择会建立全新上传。PUT 到已有目录返回 `409`，不会被误判成认证过期。
- 移除了拖放上传及非标准的 `webkitGetAsEntry` 目录递归实现；页面只保留文件拖入的默认导航拦截，实际上传改由独立的多文件选择器和现代浏览器文件夹选择器触发。
- 服务端 stage/state 采用 7 天 TTL：维护任务在启动时立即扫描一次，随后每小时扫描；扫描从启动时持有的共享根 fd 逐级枚举，不会因运行中替换启动路径而切换到新目录。活跃上传使用“父目录设备号/inode + 内部文件名”语义键登记，根内符号链接别名不会导致维护任务误删；每个候选在删除前重新取得活跃集合锁并完成复核和删除，严格跳过已登记的活跃项；清理后同步父目录并记录日志。
- PUT/PATCH 缺少 upload ID 或总长度、PATCH 缺少 offset 时返回 `400`；upload ID 不存在时返回 `404`；总长度或 offset 与持久化状态不一致时返回 `409`。所有写入只使用当前会话式上传路径。
- 移除了 CORS 配置和响应头注入；内置浏览器 UI 现在仅作为同源客户端运行。
- 移除了 `--assets`、运行时浏览器 UI 覆盖功能和自定义 `404.html`；编译时内置的 UI 现在是唯一的管理界面。JavaScript、CSS 和图标共同生成 64 位十六进制 SHA-256 内容摘要前缀，资源内容改变即更换 URL；只有成功返回的已知摘要资源可以长期公共缓存，未知资源 404 使用 `private, no-store`。
- 移除了 WebDAV 及其兼容接口：`OPTIONS`、`PROPFIND`、`PROPPATCH`、`COPY`、`MOVE`、`MKCOL`、`LOCK`、`UNLOCK`、DAV 响应头/XML、Microsoft MiniRedir 处理逻辑和 WebDAV 专用测试；同时删除单文件共享模式，`serve-path` 现在必须是现有目录，非目录会在监听前明确拒绝，目录内的文件下载统一经过正常的 GET/HEAD 方法分派。
- 将浏览器的目录创建及移动/重命名请求替换为 `__dufs__/api/` 下可识别路径前缀的同源 JSON `POST` 端点。
- 内部浏览器 API 仍仅接受 `application/json`，并继续限制请求正文大小、要求认证会话、校验会话专属 CSRF 令牌以及确保路径位于根目录内。
- 移除了遗留的 `?json`、`?hash`、`?simple` 和 `?noscript` 输出模式，以及无 JavaScript 的用户代理回退机制。
- 移除了静态站点渲染模式及其 `--render-index`、`--render-try-index` 和 `--render-spa` 选项。
- 移除了命令行补全生成、`--completions` 选项，以及现已不再使用的 WebDAV/补全依赖。
- 移除了 Unix socket 监听器；`--bind` 和 YAML `bind` 现在仅接受 IP 地址，网关或反向代理通过环回或私有网络 TCP 端口连接 Dufs。
- 默认监听地址改为唯一的 IPv4 通配地址 `0.0.0.0`；IPv6 继续支持通过 `--bind ::` 或其他明确 IPv6 地址启用。
- TCP `accept` 失败现在会记录监听地址、错误分类、I/O 类型、系统错误码和重试延迟；连续失败按 50 ms 到 1 s 的上限指数退避，下一次成功接收后重置为 50 ms。
- Hyper 连接失败现在会分类记录而不再静默丢弃，连接诊断中还会包含时间戳、日志级别和 TCP 对端地址。
- 增加 Linux SIGINT 与 SIGTERM 分阶段优雅停机：首次信号停止接收并通知 Hyper 排空连接；上传从开始处理起位于提交跟踪器。30 秒后取消普通任务，并以 force token 让仍在接收正文的上传停止接收、保存有效检查点或清理暂存；服务继续等待其收尾，以及 mkdir、move、最终 rename/目录 `fsync` 和可见删除提交。新增真实半包 HTTP 上传测试，验证 30 秒宽限结束后保存 20 MiB 持久化检查点且不提前发布目标。
- 目录页和搜索页改为小型 HTML 骨架；受认证的 list API 默认分页 200 项、最多 500 项，使用绑定目录 inode/时间戳和查询条件的不透明 cursor，Dufs 内目录项结构变化返回 `409`。直接目录枚举只保留 `limit + 1` 个候选，名称排序键只构造一次；搜索和 ZIP 的 fd-relative 遍历运行于受跟踪的 blocking 任务，并在外层请求超时后继续持有并发 permit，直到 worker 真正退出。
- 按固定部署模型删除内置 TLS 参数、feature、Rust TLS 依赖和专用测试；Dufs 仅提供内网 HTTP/TCP，HTTPS 由网关终止。Playwright 使用 Node HTTPS 测试网关代理到动态回环 HTTP 后端。
- 增加连接、普通请求、上传、列表/搜索和 ZIP 的统一硬预算，包括并发数、请求头时限、正文空闲/总时限、上传声明长度、遍历项数、ZIP 实际源字节和实际输出字节。上传与 ZIP 通过按 Linux `st_dev` 分桶的共享追踪器联合核算最低磁盘水位；ZIP permit 从阻塞遍历持续到响应正文完成或取消。满载或越界返回明确的 `408`、`413`、`429`、`504`、`507`。
- 修复 Tokio 文件写入队列尚未完成时立即读取 metadata 会偶发误报上传不完整的问题；成功接收正文后先 `flush`，再验证真实长度，最终仍由 `sync_all` 和父目录 `fsync` 提供耐久化屏障。接收器同时拒绝声明剩余长度之外的任何正文。
- 将日志输出移到容量 4096 的有界队列和独立写线程，按 250 ms 周期批量刷新；HTTP 日志在渲染和入队阶段都限制为 16 KiB，自定义格式限制为 4096 字节、128 个元素。队列满时丢弃最新记录，运行中至多每秒输出一次聚合告警；正常停止 flush 最多等待 5 秒。所有动态错误和请求字段统一转义为单个物理日志行。
- 增加 `lib.rs`、`RequestContext`、`AppError` 和可注入的 `StorageDurability` 提交边界；上传最终同步与发布支持确定性故障注入测试。继续把服务拆分为明确的路由、会话、列表、下载、上传、根 fd 和存储模块。
- 前端生产脚本拆分为 API、路径、DOM、分页列表、文件操作、上传队列和入口 ES modules；动态数据只使用安全 DOM API，主要控件使用原生按钮、中文可访问名称和 live region。登录页与目录页统一增加最小 `Permissions-Policy`。
- 直接依赖迁移到 `serde_yaml_ng`，删除不再使用的 `smart-default`、`if-addrs`、`walkdir`、`urlencoding` 和 TLS 依赖；URL 组件统一由 `percent-encoding` 处理。新增 `scripts/check.sh` 作为 Rust、浏览器、依赖审计和本地 Git 清洁状态的统一检查入口。
- `$remote_user` 日志现在仅使用通过表单登录或会话成功验证的身份；未认证和认证失败的请求会将该字段记录为 `-`。自定义 `$http_...` 变量中位于固定 `$http_` 前缀后的请求头名称会先规范化为 ASCII 小写，再用于敏感分类、请求头查询和日志数据存储；Authorization、Proxy-Authorization、Cookie 和 CSRF 的名称部分使用全小写、全大写及混合大小写时都会记录为 `[REDACTED]`，普通请求头仍按 HTTP 的大小写不敏感语义记录。
- 修复内置资源访问日志过滤：只有 `GET`、精确匹配已知内置 JavaScript/CSS/图标路径且最终返回 `200` 的请求会跳过普通访问日志；资源 `HEAD`、未知资源、错误响应及其他页面、健康检查、登录、下载和 API 请求仍完整记录。
- 将过大的 `src/server.rs` 实现拆分为浏览器 API、目录列表、下载、会话、路径协调、根 fd 文件系统操作、上传维护和存储同步模块。
- 增加按认证、浏览、可访问性、上传和文件操作拆分的本地 Playwright HTTPS 测试。Chromium 与 Firefox 都通过测试网关使用真实中文登录表单，并覆盖 Cookie、注销、分页、文件/ZIP/单段 Range、CSRF 写操作、同页 HEAD→PATCH 续传、取消/排队重试、禁用拖放上传、文件夹选择器和忽略旧 localStorage 身份；可选命令使用正式 Microsoft Edge 通道。
- 将锁定的 `crossbeam-epoch`、`quinn-proto` 和 `anyhow` 版本更新至可解决当前 RustSec 安全通告的版本；更新后的锁文件通过 `cargo audit`，无漏洞或警告。
- 修复了包含特殊字符时的搜索 URL 构造、上传重试并发计数，以及删除失败提示中引用未定义文件名的问题。
- 将被 CSP 阻止的内联上传状态样式替换为编译后的 CSS 类，使浏览器导航测试等待应用触发的重定向，并让 Playwright 测试套件在出现意外 CSP 控制台违规时失败。
- 将桌面浏览器定义为受支持的 Web 平台；测试矩阵有意排除移动端视口。

## [0.46.0] - 2026-05-07

### 新功能

- 增加 --allow-hash 选项，用于允许或禁止文件哈希 (#657)
- 支持在文件路径上使用 `?json` (#686)
- 支持自定义 404 页面 (#688)
- 增强日志格式 (#692)
- WebUI 上传过程中退出时要求确认 (#693)
- HEAD 请求跳过目录遍历 (#701)

### 问题修复

- 修复符号链接损坏导致部分搜索结果缺失的问题 (#665)
- 对 ?simple 输出中的文件名进行转义 (#669)
- 确保符号链接位于服务根目录内 (#670)
- 调整认证逻辑 (#689)
- 修复 HTTP 范围计算下溢问题 (#690)
- 对日志所记录 URI 和请求头中的控制字符进行转义 (#691)
- 修复 WebUI 在 Safari 中的上传速度显示问题 (#695)

### 重构

- 更新依赖 (#655)
- 改进 UI 按钮标题 (#656)
- 改进 WebUI 文件大小格式 (#698)

## [0.45.0] - 2025-09-03

### Bug Fixes

- Perms on `dufs -A -a @/:ro` (#619)
- Login btn does not work for readonly anonymous (#620)
- Verify token length (#627)

### Features

- Make dir urls inherit `?noscript` params (#614)
- Log decoded uri (#615)

## [0.44.0] - 2025-08-02

### Bug Fixes

- No authentication check if no auth users (#497)
- Webui can't handle hash property of URL well (#515)
- Incorrect dir size due to hidden files (#529)
- Webui formatDirSize (#568)
- Follow symlinks when searching/archiving (#572)
- Incorrect separator for zip archives under windows (#577)
- Unexpected public auth asking for login info (#583)

### Features

- Higher perm auth path shadows lower one (#521)
- Add cache-control:no-cache while sending file and index (#528)
- Support multipart ranges (#535)
- Limit sub directory item counting (#556)
- Tolerate the absence of mtime (#559)
- Support noscript fallback (#602)
- Support  downloading via token auth (#603)

### Refactor

- Change description for `--allow-archive` (#511)
- Removes clippy warnings (#601)
- Update deps (#604)
- Fix typos (#605)

## [0.43.0] - 2024-11-04

### Bug Fixes

- Auth failed if password contains `:` (#449)
- Resolve speed bottleneck in 10G network (#451)

### Features

- Webui displays subdirectory items (#457)
- Support binding abstract unix socket (#468)
- Provide healthcheck API (#474)

### Refactor

- Do not show size for Dir (#447)

## [0.42.0] - 2024-09-01

### Bug Fixes

- Garbled characters caused by atob (#422)
- Webui unexpected save-btn when file is non-editable (#429)
- Login succeeded but popup `Forbidden` (#437)

### Features

- Implements remaining http cache conditionalss (#407)
- Base64 index-data to avoid misencoding (#421)
- Webui support logout (#439)

### Refactor

- No inline scripts in HTML (#391)
- Return 400 for propfind request when depth is neither 0 nor 1 (#403)
- Remove sabredav-partialupdate from DAV res header (#415)
- Date formatting in cache tests (#428)
- Some query params work as flag and must not accept a value (#431)
- Improve logout at asserts/index.js (#440)
- Make logout works on safari (#442)

## [0.41.0] - 2024-05-22

### Bug Fixes

- Timestamp format of getlastmodified in dav xml (#366)
- Strange issue that occurs only on Microsoft WebDAV (#382)
- Head div overlap main contents when wrap (#386)

### Features

- Tls handshake timeout (#368)
- Add api to get the hash of a file (#375)
- Add log-file option (#383)

### Refactor

- Digest_auth related tests (#372)
- Add fixed-width numerals to date and size on file list page (#378)

## [0.40.0] - 2024-02-13

### Bug Fixes

- Guard req and destination path (#359)

### Features

- Revert supporting for forbidden permission (#352)

### Refactor

- Do not try to bind ipv6 if no ipv6 (#348)
- Improve invalid auth (#356)
- Improve resolve_path and handle_assets, abandon guard_path (#360)

## [0.39.0] - 2024-01-11

### Bug Fixes

- Upload more than 100 files in directory (#317)
- Auth precedence (#325)
- Serve files with names containing newline char (#328)
- Corrupted zip when downloading large folders (#337)

### Features

- Empty search `?q=` list all paths (#311)
- Add `--compress` option (#319)
- Upgrade to hyper 1.0 (#321)
- Auth supports forbidden permissions (#329)
- Supports resumable uploads (#343)

### Refactor

- Change the format of www-authenticate (#312)
- Change the value name of `--config` (#313)
- Optimize http range parsing and handling (#323)
- Propfind with auth no need to list all (#344)

## [0.38.0] - 2023-11-28

### Bug Fixes

- Unable to start if config file omit bind/port fields (#294)

### Features

- Password can contain `:` `@` `|` (#297)
- Deprecate the use of `|` to separate auth rules (#298)
- More flexible config values (#299)
- Ui supports view file (#301)

### Refactor

- Take improvements from the edge browser (#289)
- Ui change the cursor for upload-btn to a pointer (#291)
- Ui improve uploading progress (#296)

## [0.37.1] - 2023-11-08

### Bug Fixes

- Use DUFS_CONFIG to specify the config file path (#286)

## [0.37.0] - 2023-11-08

### Bug Fixes

- Sort path ignore case (#264)
- Ui show user-name next to the user-icon (#278)
- Auto delete half-uploaded files (#280)

### Features

- Deprecate `--auth-method`,  as both options are available (#279)
- Support config file with `--config` option (#281)
- Support hashed password (#283)

### Refactor

- Remove one clone on `assets_prefix` (#270)
- Optimize tests
- Improve code quality (#282)

## [0.36.0] - 2023-08-24

### Bug Fixes

- Ui readonly if no write perm (#258)

### Testing

- Remove dependency on native tls (#255)

## [0.35.0] - 2023-08-14

### Bug Fixes

- Search should ignore entry path (#235)
- Typo __ASSERTS_PREFIX__ (#252)

### Features

- Sort by type first, then sort by name/mtime/size (#241)

## [0.34.2] - 2023-06-05

### Bug Fixes

- Ui refresh page after login (#230)
- Webdav only see public folder even logging in (#231)

## [0.34.1] - 2023-06-02

### Bug Fixes

- Auth logic (#224)
- Allow all cors headers and methods (#225)

### Refactor

- Ui checkAuth (#226)

## [0.34.0] - 2023-06-01

### Bug Fixes

- URL-encoded filename when downloading in safari (#203)
- Ui path table show move action (#219)
- Ui set default max uploading to 1 (#220)

### Features

- Webui editing support multiple encodings (#197)
- Add timestamp metadata to generated zip file (#204)
- Show precise file size with decimal (#210)
- [**breaking**] New auth (#218)

### Refactor

- Cli positional rename root => SERVE_PATH(#215)

## [0.33.0] - 2023-03-17

### Bug Fixes

- Cors allow-request-header add content-type (#184)
- Hidden don't works on some files (#188)
- Basic auth sometimes does not work (#194)

### Features

- Guess plain text encoding then set content-type charset (#186)

### Refactor

- Improve error handle (#195)

## [0.32.0] - 2023-02-22

### Bug Fixes

- Set the STOPSIGNAL to SIGINT for Dockerfile
- Remove Method::Options auth check (#168)
- Clear search input also clear query (#178)

### Features

- [**breaking**] Add option --allow-archive (#152)
- Use env var for args (#170)
- Hiding only directories instead of files (#175)
- API to search and list directories (#177)
- Support edit files (#179)
- Support new file (#180)
- Ui improves the login experience (#182)

## [0.31.0] - 2022-11-11

### Bug Fixes

- Auth not works with --path-prefix (#138)
- Don't search on empty query string (#140)
- Status code for MKCOL on existing resource (#142)
- Panic on PROPFIND // (#144)

### Features

- Support unix sockets (#145)

## [0.30.0] - 2022-09-09

### Bug Fixes

- Hide path by ext name (#126)

### Features

- Support sort by name, mtime, size (#128)
- Add --assets options to override assets (#134)

## [0.29.0] - 2022-08-03

### Bug Fixes

- Table row hover highlighting in dark mode (#122)

### Features

- Support ecdsa tls cert (#119)

## [0.28.0] - 2022-08-01

### Bug Fixes

- File path contains special characters (#114)

### Features

- Add table row hover (#115)
- Support customize http log format (#116)

## [0.27.0] - 2022-07-25

### Features

- Improve hidden to support glob (#108)
- Adjust digest auth timeout to 1day (#110)

## [0.26.0] - 2022-07-11

### Bug Fixes

- Cors headers (#100)

### Features

- Make --path-prefix works on serving single file (#102)

## [0.25.0] - 2022-07-06

### Features

- Ui supports creating folder (#91)
- Ui supports move folder/file to new path (#92)
- Check permission on move/copy destination (#93)
- Add completions (#97)
- Limit the number of concurrent uploads (#98)

## [0.24.0] - 2022-07-02

### Bug Fixes

- Unexpected stack overflow when searching a lot (#87)

### Features

- Allow search with --render-try-index (#88)

## [0.23.1] - 2022-06-30

### Bug Fixes

- Safari layout and compatibility (#83)
- Permissions of unzipped files (#84)

## [0.23.0] - 2022-06-29

### Features

- Use feature to conditional support tls (#77)

### Ci

- Support more platforms (#76)

## [0.22.0] - 2022-06-26

### Features

- Support hiding folders with --hidden (#73)

## [0.21.0] - 2022-06-23

### Bug Fixes

- Escape name contains html escape code (#65)

### Features

- Use custom logger with timestamp in rfc3339 (#67)

### Refactor

- Split css/js from index.html (#68)

## [0.20.0] - 2022-06-20

### Bug Fixes

- DecodeURI searching string (#61)

### Features

- Added basic auth (#60)
- Add option --allow-search (#62)

## [0.19.0] - 2022-06-19

### Features

- [**breaking**] Path level access control (#52)
- Serve single file (#54)
- Ui hidden root dirname (#58)
- Reactive webpage (#51)
- [**breaking**] Rename to dufs (#59)

### Refactor

- [**breaking**] Rename --cors to --enable-cors (#57)

## [0.18.0] - 2022-06-18

### Features

- Add option --render-try-index (#47)
- Add slash to end of dir href

## [0.17.1] - 2022-06-16

### Bug Fixes

- Range request (#44)

## [0.17.0] - 2022-06-15

### Bug Fixes

- Webdav propfind dir with slash (#42)

### Features

- Listen both ipv4 and ipv6 by default (#40)

### Refactor

- Trivial changes (#41)

## [0.16.0] - 2022-06-12

### Features

- Implement head method (#33)
- Display upload speed and time left (#34)
- Support tls-key in pkcs#8 format (#35)
- Options method return status 200

### Testing

- Add integration tests (#36)

## [0.15.1] - 2022-06-11

### Bug Fixes

- Cannot upload (#32)

## [0.15.0] - 2022-06-10

### Bug Fixes

- Encode webdav href as uri (#28)
- Query dir param

### Features

- Add basic dark theme (#29)
- Add empty state placeholder to page(#30)

## [0.14.0] - 2022-06-07

### Bug Fixes

- Send index page with content-type (#26)

### Features

- Support ipv6 (#25)
- Add favicon (#27)

## [0.13.2] - 2022-06-06

### Bug Fixes

- Filename xml escaping
- Escape path-prefix/url-prefix different

## [0.13.1] - 2022-06-05

### Bug Fixes

- Escape filename (#21)

### Refactor

- Use logger (#22)

## [0.13.0] - 2022-06-05

### Bug Fixes

- Ctrl+c not exit sometimes

### Features

- Implement more webdav methods (#13)
- Use digest auth (#14)
- Add webdav proppatch handler (#18)

## [0.12.1] - 2022-06-04

### Features

- Support webdav (#10)
- Remove unzip uploaded feature (#11)

## [0.11.0] - 2022-06-03

### Features

- Support gracefully shutdown server
- Listen 0.0.0.0 by default

## [0.10.1] - 2022-06-02

### Bug Fixes

- Panic when bind already used port

## [0.10.0] - 2022-06-02

### Bug Fixes

- Remove unzip file even failed to unzip
- Rename --no-auth-read to --no-auth-access
- Broken ui

### Documentation

- Refactor readme

### Features

- Change auth logic/options
- Improve ui

### Refactor

- Small improvement

## [0.9.0] - 2022-06-02

### Documentation

- Improve readme

### Features

- Support path prefix
- List all ifaces when listening 0.0.0.0
- Support tls

## [0.8.0] - 2022-06-01

### Bug Fixes

- Some typos
- Caught 500 if no permission to access dir

### Features

- Cli add allow-symlink option
- Add some headers to res
- Support render-index/render-spa

## [0.7.0] - 2022-05-31

### Bug Fixes

- Downloaded zip file has no.zip ext in firefox
- Unzip override existed file in uploadonly mode
- Miss file 500
- Not found dir when allow_upload is false

### Features

- Drag and drop uploads, upload folder

## [0.6.0] - 2022-05-31

### Features

- Delete confirm
- Distinct upload and delete operation
- Support range requests

### Refactor

- Improve code quality

## [0.5.0] - 2022-05-30

### Features

- Add mime and cache headers to response
- Add no-auth-read options
- Unzip zip file when unload

## [0.4.0] - 2022-05-29

### Features

- Replace --static option to --no-edit
- Add cors

## [0.3.0] - 2022-05-29

### Documentation

- Update readme demo png

### Features

- Automatically create dir while uploading
- Support searching

### Refactor

- Handler zip

### Styling

- Optimize css

## [0.2.1] - 2022-05-28

### Bug Fixes

- Cannot upload in root
- Optimize download zip

### Documentation

- Improve readme

### Features

- Aware RUST_LOG

## [0.2.0] - 2022-05-28

### Documentation

- Update demo png
- Improve readme

### Features

- Add logger
- Download folder as zip file

## [0.1.0] - 2022-05-26

### Bug Fixes

- Caught server error when symlink broken

### Documentation

- Improve readme
- Update readme

### Features

- Add basic auth and readonly mode
- Support delete operation
- Remove parent path

### Styling

- Cargo fmt
- Update index page

### Build

- Remove dev deps

### Ci

- Init ci

<!-- generated by git-cliff -->
