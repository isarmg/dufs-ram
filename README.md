# Dufs 浏览器文件管理器

Dufs 是一个使用 Rust 编写的轻量级浏览器文件管理器。启动单个 Linux 可执行文件后，即可通过 Edge、Firefox 等现代桌面浏览器浏览和管理指定目录，不需要单独部署前端服务。

本项目面向个人或受控局域网环境。推荐部署约定是一台系统只运行一个 Dufs 进程，并由该进程管理一个共享根目录；代码强制保证的锁粒度则是“每个共享根唯一实例”：程序会在共享根目录 FD 上取得非阻塞独占锁，指向同一根的第二实例会启动失败，不同根目录之间不会互相加锁。该 advisory lock 不能阻止 shell、宿主机或其他程序写入；本文描述的一致性保证要求共享根由 Dufs 和停服后的受控运维流程独占写入。账号只决定能否登录；创建账号后即拥有整个共享根的完整文件管理权限。

## 支持范围

- 浏览目录并按名称、修改时间或大小排序；
- 下载单个文件并支持单段 Range 断点下载；
- 通过文件选择器上传文件，通过文件夹选择器上传目录；
- 显示上传速度、进度和预计剩余时间；
- 当前页面内的失败任务可校验服务端持久化检查点并继续上传；
- 新建文件和目录，移动、重命名及删除文件或目录；
- 从当前目录开始按文件名递归搜索；
- Argon2id 密码、英文登录页、服务端会话和 CSRF 防护；
- 资源预算和异步访问日志；
- 编译内置的原生 HTML、CSS、JavaScript 管理页面。

本项目仅支持 Linux 服务端和现代桌面浏览器，不提供匿名访问、账号分级权限、手机 Web、WebDAV、无 JavaScript 客户端、拖放上传、在线预览或编辑、静态网站托管、运行时页面资源覆盖、URL 子路径部署、用户自定义隐藏规则、Unix socket、CORS 或环境变量配置。

## 文档分工

- [从零读懂 Dufs：新手教学手册](docs/beginner-guide/README.md)：按十章课程从运行环境、Rust/HTTP 基础一路讲到前后端、上传状态机、测试和生产运维；
- [项目工作流程与流程树](docs/project-workflow.md)：说明当前代码的启动、认证、浏览、上传、下载、持久化和停机流程；
- [文档导航](docs/README.md)：区分当前规范、教学资料与历史审查记录；
- [十项优化 TODO 与完成记录](docs/history/browser-only-optimization-review.md)：记录历次质量优化及其与当前实现的同步结果；
- [完整功能与取舍清单](docs/feature-inventory-and-tradeoffs.md)：逐项列出当前功能、依赖、删除影响和可精简候选；
- [生产部署、备份、升级与回滚](docs/operations.md)：给出经过语法验证的 systemd/nginx 基线、健康检查、备份恢复演练和制品验证流程；
- [安全策略](SECURITY.md)：说明支持边界、私密报告要求和事件响应信息；
- [本地变更记录](CHANGELOG.md)：记录从 0.46.0 起已经完成的改动及更早版本历史。

本 fork 托管在 `https://github.com/isarmg/dufs-ram`。仓库有只读 GitHub Actions 门禁，但有意不提供自动 GitHub Release 工作流：远程任务不会签名、创建 tag、发布 release 或取得仓库写权限。`0.48.0` 当前是尚未发布的源码版本，缺少精确 `v0.48.0` tag 是有意状态；只有源码、审查和发布准备全部完成后才应创建与 Cargo 版本一致且精确指向目标 commit 的 tag。正式制品仍使用本地 Git 和 `scripts/package-release.sh` 生成带 Git SHA、checksum、CycloneDX SBOM、`BUILD-ENVIRONMENT.txt` 构建环境记录和签名的可验证发布目录。输出目录经所有者/权限校验后以 FD 锁定，stage 创建、构建、清理、最终 rename 和目录同步都从该 FD 派生；全部制品先在同文件系统私有 stage 中验证并同步，在支持 Linux `RENAME_NOREPLACE` 的发布文件系统上再由一次 no-clobber 目录 rename 原子公开。Rust/Node 依赖仍来自锁文件指定的包仓库；首次构建、`npm ci` 和发布脚本的依赖 vendoring 需要可用仓库或已经填充的本地缓存，完成 vendoring 后的隔离 release 构建才强制离线。

发布包完整保留仓库的 `docs/` 层次，并携带教程本地链接所引用的 `assets/`、`src/`、`tests/`、`scripts/`、部署样例和构建配置；这些支持材料用于离线阅读与核对，不是运行 Dufs 的额外依赖。打包和 `--self-test` 会先用包内文档检查器验证所有本地链接，再把除 `SHA256SUMS` 自身外的全部普通文件写入清单；此后只做只读覆盖校验，使 checksum 成为包内最后一次内容变更。根目录的 `CODE_REVIEW_REPORT.md` 只作为旧包名兼容副本，规范位置仍是 `docs/history/code-review-report.md`。

## 环境要求

- 自动 CI、部署样例和正式制品的验证基线是 `x86_64-unknown-linux-gnu`；`build.rs` 也允许其他 64 位 Linux 源码构建，但 aarch64 等目标在加入等价 CI/部署矩阵前只属于未验证的 best effort；
- 运行内核必须提供 `openat2`；不支持时程序会拒绝启动，二进制还必须匹配 CPU、libc 和动态加载器 ABI；
- Rust、rustc 和 Cargo 1.97.1，源码使用 Rust 2024 edition；
- 建议使用 rustup；`rust-toolchain.toml` 已固定工具链并包含 Clippy、Rustfmt；
- Node.js 18 或更高版本用于前端/文档门禁和发布 SBOM 规范化，不是生产运行依赖；
- 本地开发门在安装 ShellCheck 时执行 `--severity=warning`，缺失时明确跳过而不会联网安装；远程 CI 按 SHA-256 固定并强制使用 ShellCheck 0.11.0，正式发布也要求 ShellCheck 可用；
- 本地签名发布还要求可用的 `/proc/self/fd`、OpenSSL、`cargo-cyclonedx 0.5.9`、`cargo-audit 0.22.2`、支持 `mv --update=none --no-copy` 的 GNU coreutils、支持 Linux `RENAME_NOREPLACE` 的发布文件系统，以及固定 Rust 1.97.1 sysroot 中经过摘要审核的标准库版权文件；脚本只在 source 消失且 destination 仍是同一设备号/inode 的实体目录时确认发布，并把 `--update=none` 的静默跳过判为碰撞失败。这些不是 Dufs 生产进程依赖。

`build.rs` 会在编译期拒绝非 Linux 或非 64 位目标，但这个编译边界不等于每个 64 位架构都经过支持矩阵验证。`Cargo.lock` 中出现上游依赖的其他平台条件包属于 Cargo 的完整依赖图，也不表示本项目支持这些平台。

## 编译

```sh
cargo build --release --locked
```

生成的可执行文件位于：

```text
target/release/dufs
```

也可以直接从当前本地源码安装：

```sh
cargo install --locked --path .
```

## 快速开始

先生成密码哈希：

```sh
./target/release/dufs hash-password
```

再把下面的 `$argon2id$…` 替换为命令输出的完整 PHC 字符串：

```sh
install -d -m 0700 /专用状态目录
./target/release/dufs \
  -p 5000 \
  -a 'admin:$argon2id$…' \
  --state-dir /专用状态目录 \
  /需要管理的目录
```

未指定 `--bind` 时，Dufs 默认只监听 `127.0.0.1:5000`。需要从其他主机上的网关回源时，必须显式指定内网 IP，并通过防火墙或 ACL 限制来源；需要 IPv6 时可显式使用 `--bind ::1` 或其他 IPv6 地址。CLI/YAML 至少要提供一个监听地址，空的 `bind: []` 会在创建任何运行时资源前报错退出。多个 listener 只让已经接受的连接持有全局连接许可，不会因空闲地址预占许可而让其他地址饥饿。

浏览器会话 Cookie 带有 `Secure` 属性，前端生成上传 UUID 使用的 `crypto.randomUUID()` 也要求安全上下文，因此浏览器入口必须使用 HTTPS。通常应在浏览器中打开网关提供的地址：

```text
https://files.example.com/
```

服务至少需要一个账号；没有通过命令行或 YAML 配置账号时会拒绝启动。登录后无需开启其他能力开关，即可使用全部文件管理功能。

## 账号与登录

账号格式为：

```text
用户名:$argon2id$...
```

配置多个账号时重复使用 `--auth`：

```sh
./target/release/dufs \
  -b 127.0.0.1 \
  -a 'admin:$argon2id$…' \
  -a 'user:$argon2id$…' \
  --state-dir /专用状态目录 \
  /需要管理的目录
```

使用要求：

- 原始密码必须非空且最多 1024 个 UTF-8 字节；`dufs hash-password`、登录表单解析和 Argon2id 哈希入口共用这一字节上限，配置中保存的是该命令生成的完整 PHC 字符串；
- 哈希包含 `$`，在 Shell 中应使用单引号；
- 重复用户名和非 Argon2id 格式会阻止启动；
- 每个账号均可浏览、上传、覆盖、移动、删除及搜索整个共享根；
- 登录成功后使用服务端内存会话；空闲 30 分钟或创建满 12 小时后失效；
- 每个账号最多保留 32 个会话，全局最多 1024 个；重复登录优先淘汰同账号最久未活动的会话；
- 程序重启会清空会话，浏览器需要重新登录；
- 登录 POST 使用独立的同源来源检查；注销和文件写入还必须通过当前会话绑定的 CSRF Token 校验。

登录 POST 在读取正文前同时消耗全局 burst 16/每秒补充 1 个及单来源 IP burst 8/每秒补充 1 个的令牌；正在读取的登录正文还受全局 32、每 IP 4 个并发许可、4 KiB 上限和 10 秒总时限约束。解析账号后再按“来源 IP + 账号摘要”组合键应用失败退避，并受全局最多两个 Argon2id 计算槽约束；一个来源对账号的失败不会在密码校验前锁定其他来源。`Retry-After` 会把剩余退避时间向上取整到完整秒，并只出现在重定向后的最终 `429` 登录错误页，不会被误解释为延迟跟随 `303`。应用限制与网关限流是叠加关系，不能互相替代。

未认证 GET/HEAD 只有在逐个 `Accept` 字段、逐个逗号项解析后发现精确的 `text/html` media type，且可选 `q` 值语法有效并大于 0 时，才会 `303` 到登录页。`text/htmlx`、`text/html;q=0`、重复或畸形 `q` 不会被当成页面导航，仍返回接口式 `401`。

## 浏览器文件管理

### 浏览、下载与搜索

- 点击目录名进入下一级目录；
- 点击文件名或下载按钮下载文件；
- 点击表头按名称、修改时间或大小排序；
- 搜索框从当前目录开始递归匹配文件名。

当前版本不提供目录打包下载。为避免旧脚本把 HTML 目录页误保存为归档，目录 GET/HEAD 中只要存在 `zip` 查询 key（包括 `?zip` 和 `?zip=1`）就返回 `410 Gone` 与稳定 code `directory_archive_unsupported`；HEAD 不发送正文。升级前还必须从 YAML/命令行移除 `compress`、`max-zip-entries`、`max-zip-uncompressed-size`、`max-zip-output-size` 和 `max-concurrent-zips`；YAML 对已删除字段保持严格拒绝，不会静默忽略。

普通文件始终作为附件下载，不提供在线预览或编辑。数据句柄以 `O_NONBLOCK` 从共享根 fd 打开，并在同一 fd 上确认仍是普通文件后才读取，所以路由分类后被外部写者换成 FIFO 不会把打开操作无限阻塞。服务只接受一个 `Range` 请求头中的一个字节范围；重复 `Range` 请求头或逗号分隔的多段 Range 都返回 `416`。完整 GET 和单段响应的正文均严格限制为打开文件时声明的长度，外部进程随后向同一 inode 追加内容不会使本次响应越界；出现 `If-Range` 时则保守忽略 Range 并返回完整 `200`。目录列表和搜索只接受 UTF-8 文件名；遇到非 UTF-8 Linux 文件名时整个操作会失败，需要先在系统侧重命名。

目录页只返回骨架，文件项通过受认证的分页 API 每次最多加载 500 项，默认 200 项。首屏会在受跟踪的阻塞任务中一次物化，并使用稳定、可中断的归并排序；排序的合并和最终置换都会持续检查停机标志与 deadline，不再只在整轮排序前后检查。递归搜索边遍历边转换结果并累计实际字符串与结构容量，在形成超预算向量前终止。列表和搜索最多检查 100000 个目录项；递归遍历深度及工作集另受 1024 层和约 32 MiB 限制，搜索结果向量也独立受约 32 MiB 限制。active-ancestor `HashSet` 按最大深度一次性预留并保守预检；`Vec` 和名称字符串扩容在分配前同时核算旧、新缓冲区的瞬时峰值，只有峰值仍在预算内才采用几何增长。后续页只切分同一不可变结果集，不会重复扫描整个目录。游标使用服务端密钥认证并绑定账号摘要、目录身份、查询、排序和页大小；快照绝对寿命为 120 秒，进程内缓存总计最多 32 个/约 64 MiB、每账号最多 8 个/约 32 MiB。游标编码/版本无效、跨账号使用或其他请求绑定不匹配返回 `400`；认证标签不匹配、快照未知/过期/淘汰或直接目录变化返回 `409` 并要求从第一页重载。

首屏构造期间，直接列表会前后复核当前目录；递归搜索会复核每个即将访问的目录，并在完成后再次复核所有访问过的目录。目录在这些检查间发生可观察变化时返回可重试的 `409`，前端会丢弃已加载页并从第一页重载。该机制只能检测遍历期间的目录身份/元数据变化，并不等于文件系统原子快照：检查间发生又恢复的变化、未反映到目录元数据的文件内容变化，以及最终复核后的变化仍可能不可见。需要文件系统级强一致导出时，应从只读存储快照或等价的版本化源生成结果。

Dufs 不提供用户自定义隐藏规则。目录列表和递归搜索会处理共享根内的所有普通文件和目录；Dufs 自身使用的上传暂存、上传状态及删除回收项属于内部保留项，仍不会显示，也不能通过普通浏览器路径访问。

### 上传与续传

- `Upload files` 按钮可选择一个或多个文件；
- `Upload folder` 按钮会保留所选目录中的相对路径，但不会创建空目录；
- 单次选择最多接受 512 个文件和合计 256 KiB 的 UTF-8 逻辑路径；当前页最多保留 512 个等待或执行中的任务，队列支持常数时间取消，完成/失败/未知/取消历史只保留最近 200 行并报告被隐藏的旧结果，避免大批选择无界占用 DOM 和内存；
- 入队前会对这一批最终绝对逻辑路径做有界预检；没有重名时直接上传，只有已存在且初步符合替换条件的目标才询问是否覆盖，不可替换的目标会明确跳过；`replaceable` 只是低成本提示，完整 metadata/xattr 检查仍可能在提交前拒绝；
- 预检不是提交锁。新建默认使用原子 no-replace，已确认覆盖则绑定预检返回的不透明 target revision；真正发布时目标若新出现或发生变化，服务端会保留已同步的 stage，页面只对该文件再次询问“覆盖、跳过或取消后续队列”；确认覆盖使用同一 upload ID 的空 PATCH 发布 stage，不重传文件数据；
- 页面显示每个任务的速度、进度、预计剩余时间和最终结果；
- 上传成功会使当前分页目录视图进入“内容已变化”状态，以 live status 提示刷新；旧游标不会继续追加，下一次加载操作会从第一页刷新；
- 当前页面内发生可确认的可重试失败后，`Retry upload` 操作会先核对同一上传任务的服务端检查点，再决定换新 ID 完整上传或从已持久化偏移继续；结果未知或认证失效会暂停队列，不提供该 Retry；
- 页面刷新后不会自动恢复旧任务，也不会读取 `localStorage` 续传记录；重新选择文件会创建全新上传 ID，避免仅凭文件名、大小和时间戳把不同内容拼接在一起；
- 拖入文件只会被阻止触发浏览器导航，不会开始上传。

服务端不会直接改写最终文件。stage 始终以 `0600` 创建在目标同目录，且不会出现在 Dufs 的列表和搜索中。上传会话以账号摘要和 UUID 为键，在统一 SQLite state store 中记录根内相对的目标/stage 路径、声明长度、durable offset、stage dev/inode、已确认的 target revision 以及 `Running/CommitStarted/AwaitingConfirmation/Committed/Rejected/Unknown` 内部状态；共享根内不再写入、读取或导入 JSON 上传状态文件。对外响应使用 `running/awaiting-confirmation/committed/rejected/not-seen/not-started/unknown`。`awaiting-confirmation` 表示全部字节已持久化到 stage，但目标在原子发布边界不再符合已确认的条件；它只能由携带当前 revision 的空 PATCH 发布，或经 discard API 明确丢弃。`not-started` 表示请求头中的合法 ID/长度已经绑定到响应，但本次尝试在任何上传 mutation 前就因保留/越界路径、路径或路由 metadata 超时、或未取得上传槽等原因停止；它不证明该 ID 没有先前记录。为限制慢文件系统准备工作占用的资源，服务端按“路径租约 → 上传槽 → 受跟踪的路由 metadata → owner state/上传处理”进入请求；槽满会直接返回绑定的 `429 not-started`，已有检查点仍原样保留，前端显示可重试失败，但点击 Retry 必须先 HEAD 查询原 ID 后才会取得其真实状态。`rejected` 表示确定没有发布；若已有 mutation 会话则清理并尽力持久化该终态，早期策略拒绝也可能只有本次响应，因此重试同样先 HEAD，再按 `rejected/not-seen` 换新 ID。查询其他账号的 ID 与不存在、畸形或不可信的数据库记录一样返回 `404 not-seen`，不会泄露记录是否存在。

`POST /__dufs__/api/upload/preflight` 只返回当下观测结果；真正的覆盖权限由 `X-Dufs-Upload-Overwrite: true` 与 `X-Dufs-Target-Revision` 共同表达，revision 绑定账号、规范根内路径和目标完整 CAS 身份。缺省或 `false` 为原子 no-replace；无效、过期或属于其他路径的 revision 不会影响已有目标。若已保留 stage 携带从旧目标重放的 uid/gid/mode/xattr，而目标随后消失，服务端会以 `upload_metadata_preservation_refused` 失败关闭；前端先调用 `POST /__dufs__/api/upload/discard`，再用新 ID 和 no-replace 完整上传，不会把旧 metadata 发布到一个新文件。

上传查询只读 state store，并把库中的根内相对路径当作不可信输入重新校验。首个 durable offset 按 stage 文件同步、stage 父目录同步、SQLite 提交的顺序建立；活跃 stage 路径跨账号唯一，UUID 或 owner-scoped DB miss 不构成删除权限。部分 `running` 记录还会用 PATCH 实际采用的同一个可写 no-follow stage fd 校验普通文件、单链接、至少达到 durable offset，以及 dev/inode 与最后一次已同步检查点一致；失败清理同样只接受 live fd 或 DB 记录的身份。上传总 deadline 覆盖路径等待、正文接收、磁盘写入、flush 和进入不可取消提交点之前的步骤；正文发送完成后，浏览器会显示独立的提交等待状态。只有完成文件同步、同文件系统原子发布、目标父目录同步并写入 `Committed` 终态后才返回成功；客户端还会核对终态及精确长度/偏移。发布前先同步完整 stage，再持久化 `CommitStarted`；该记录是歧义屏障，对外归为 `unknown`，进程重启时也会恢复为显式 `Unknown`，不会因 stage 已被 rename、缺失或路径被复用而降格为 `not-seen`。

在受支持的 Linux 本地文件系统及存储正确兑现同步请求的前提下，成功响应表示文件已经按崩溃持久化语义提交。提交错误会区分 rename 前确定未发布与发布后结果/持久性未知：前者安全清除 stage/checkpoint 并记录拒绝终态，后者或 `committed` 终态写入失败返回“结果未知”，不会把已经恢复成只读权限的 stage 广告成可续传检查点。fresh PUT 在创建祖先/stage/SQLite 会话前先从最近存在的父目录完成空间准入，空间不足不会留下目录；准入成功后若其他正文前准备失败，则自底向上回收仅由本请求创建且仍为空、身份未变的祖先目录，已经被并发请求使用的目录不会删除。

覆盖普通单链接文件时会保留 numeric owner/group、除 setuid/setgid 外的权限位，以及通过预算检查的非特权扩展属性。`security.*`、`trusted.*`（包括 capability、SELinux、IMA/EVM 和 overlay 元数据）或原目标的 setuid/setgid 位会导致覆盖被拒绝；`user.*` 与 `system.posix_acl_access` 可被精确重放。扩展属性名称列表最多 64 KiB、条目最多 1024 个、单值最多 64 KiB；服务先查询每个值的精确长度，再按需分配，索引容量、带 NUL 的名称和全部值合计最多 1 MiB，不会为每个空值或短值先分配 64 KiB。无法安全读取、删除 stage 上额外属性或重放任一项时同样拒绝。多硬链接以及 FIFO、Unix socket、设备、目录等非普通目标也返回冲突；目标先以 `O_PATH` 分类，普通文件再以 `O_NONBLOCK|O_NOFOLLOW` 重新打开并核对 inode。最终提交前会复核目标的 dev/inode、类型、链接数、大小、uid/gid、完整 mode 以及纳秒级 mtime/ctime 快照，并用同一组字段确认 stage 路径仍对应已打开 stage fd；原本不存在的目标必须仍不存在，原有可替换符号链接则必须保持其 nofollow 快照不变。检测到变化时失败关闭。提交前的策略、格式或权限冲突返回 `409`，未预期的底层 I/O 故障通过安全的 `5xx` 报告；文件同步、复核或 rename 失败都确定没有发布并清理会话。rename 已成功但父目录同步失败时，新 inode 可能可见而持久性未确认；即使父目录已同步，终态记录持久化失败也会保守返回 `unknown`，两者都不能视为已经回滚。

进程内路径租约只协调经过当前 Dufs 进程的请求。租约同时检查词法祖先/后代和解析后的 dev/inode 别名；一个较早请求仍在异步解析语义键时，只暂时阻塞词法上相交的后续请求，无关子树可以超车，不会被一次慢解析全局停住。解析完成后仍必须按语义键和协调 epoch 重新核对现有租约及更早冲突 waiter，符号链接别名不会因此并发提交。拥有共享根操作系统写权限的 shell、其他进程或 virtiofs 宿主机属于受信任的存储参与者；它们仍可在任意两个系统调用之间更换目录项，最终 rename 也必须按名称提交，因此上述身份复核不能把共享目录变成对恶意本地写者的隔离边界。生产环境应让专用服务账号和受控运维流程独占写入。强制断电、介质损坏、错误实现同步语义的存储以及后续位腐败仍需要可靠存储和备份处理。

上传协议、检查点、内部暂存、任务取消和目录同步的完整步骤见[项目工作流程](docs/project-workflow.md#9-持久化上传与断点续传)。

### 新建、重命名、移动与删除

- 点击 New folder 或 New empty file 会立即以原子不覆盖方式创建 `newfolder` 或 `newfile`，确定重名时依次尝试 `newfolder (2)` / `newfile (2)` 等名称；创建成功后直接在名称列进入行内编辑，不先显示命名弹窗；
- 点击 Rename 也直接在原名称位置编辑。Enter、Tab 或合法名称失焦会提交，Escape 只取消本次编辑；刚创建的对象会保留已经提交的默认名称。文件编辑默认选中最后一个扩展名前的主体，目录和无扩展名文件选中全名；
- 重命名和移动在页面及后端协议中是两个独立操作：`rename` 只接受新的单段名称并保留父目录，`move` 只接受已经存在的目标目录并保留原名称；需要新的目标目录时先使用 New folder；
- 不允许覆盖时使用原子不替换语义，并发出现同名目标会返回冲突；
- 允许覆盖时若不同名称其实是同一 dev/inode 的硬链接，服务会在预检和提交内再次 fd-relative 复核并返回 `409 source_equals_destination`，不会把 POSIX rename 的无变化成功误报为 `204`；
- move、rename、DELETE 与 fresh PUT 在持有语义路径租约后、任何本次文件系统/状态 mutation 前分页检查 SQLite 中仍有效的 upload/purge 路径义务；源或目标目录（包括根内符号链接别名）的变化会使这些相对路径失真时，以稳定 `409 *_state_conflict` 拒绝。状态检查暂不可用则在 mutation 前返回可重试 `503`；
- 共享根目录本身不能删除；
- mkdir、move、rename 和 DELETE 的受跟踪提交任务共用 64 个服务端并发许可；额外请求等待许可且仍受普通请求时限约束，不会无界启动后台文件系统 mutation；
- 删除会先持久化移除原名称，再异步回收磁盘空间；
- DELETE 在改名前先向 state store 写入 `Prepared` outbox 记录，其中保存账号、根内相对目标/trash 路径及源 dev/inode/类型；随后只有通过身份复核的同父目录 rename 和父目录 `fsync` 成功，才把 job 改为 `Ready` 并返回 `204`。purge outbox 容量为全局 4096、每账号 1024，满载在可见删除前以 `503 purge_backlog_full` 拒绝；
- 单 worker 从 outbox 原子 claim 到 `Claimed`，每次最多处理 256 个条目或 25 ms；未完成项在进程内轮转，I/O 失败则持久化返回 `Ready` 并从 100 ms 开始指数退避，最长 30 秒，不再因固定失败次数丢弃 job。若 SQLite 状态转换瞬时失败，worker 会有界保留该 claim，回读确认它仍为 `Claimed` 后再继续，避免把“提交成功但回复丢失”误当作待重做；重启会把遗留 `Claimed` 恢复为 `Ready`。独立 reconciler 在启动和运行期持续检查 `Prepared`，按已记录 inode 对比原路径和 trash，判定 rename 未发生、已发生或仍有歧义，绝不删除身份不匹配的路径；
- 一旦发现内部 trash 的身份与持久记录不一致，服务会把该对象原子改名为隐藏的 `.dufs-quarantine-<uuid>.hold` 并释放相应 purge 记录；quarantine 永不参与自动清理。运维人员必须先停止 Dufs，核对日志和对象内容/归属后再手工移除，不能把它当作普通 orphan trash；
- 启动及每小时的低频根内扫描只为未记账或其他 orphan trash 提供兜底；新 DELETE 的正常可靠性不再依赖扫描重新捕获。分片遍历仍只保存根内路径和 cursor，读到 EOF 后删目录若返回 `ENOTEMPTY` 会从 cursor 0 重扫，FD 数量不随嵌套深度增长；
- 内部删除暂存项不是回收站，不提供恢复或撤销功能。

统一 state store 当前使用文件型 SQLite schema v3，一并持久化管理 `operations`、`upload_sessions` 和 `purge_jobs`。CLI `--state-dir /var/lib/dufs` 或 YAML `state-dir: /var/lib/dufs` 是必填配置，数据库文件名固定为 `state.sqlite3`；不存在内存数据库或隐式临时状态模式。目录必须已经存在、由当前服务账号所有、权限为 `0700`、不是符号链接，且不能与共享根互为祖先/后代；固定数据库及 `-journal/-wal/-shm` sidecar 也不能与日志或配置文件冲突。同一个数据库绑定共享根设备号和 inode，不能拿给另一个共享根复用。现存文件会先以只读连接验证 DUFS application id、schema 版本、共享根绑定和完整性，通过后才允许 chmod、journal 配置或恢复写入；空白新库会直接建立 v3，经严格检查的 v2 会在写连接内事务迁移到 v3，其他版本则在零修改下明确拒绝。误指向其他数据库或非 SQLite 文件同样会零修改拒绝；部署时也不能通过 bind mount 等别名让数据库实际落回共享根。

文件型状态库启动恢复会删除 operation 中尚未进入文件系统提交边界的 `Reserved`，把 operation `CommitStarted` 转换为 `Completed/unknown`，把 upload `CommitStarted` 转换为 `Unknown`，并把 purge `Claimed` 重置为可立即重试的 `Ready`。operation 终态 TTL 为 15 分钟；upload session TTL 为 7 天，容量为全局 16384、每账号 4096；purge job 没有固定失败次数或 TTL 逃生口，容量为全局 4096、每账号 1024，直到安全完成或 reconciliation 确认 rename 未发生才删除记录。SQLite 使用 rollback journal `DELETE` 模式和 `synchronous=EXTRA`。SQLite 事务与文件系统 mkdir、rename、文件/目录 `fsync` 不是一个共同事务：operation/upload 在跨域缝隙中恢复为 `unknown`，purge 则依靠已持久化的根内路径和 dev/inode/类型对原目标与 trash 做保守 reconciliation，不会把数据库记录误当成文件系统事务证明。

公开的 `GET/HEAD /__dufs__/health` 只检查 HTTP liveness。受认证的 `GET/HEAD /__dufs__/ready` 会并行执行真实写路径探针：通过长期持有的共享根 fd 创建隐藏文件、写入并同步文件、删除后同步根目录；同时在 state-store actor 的现有 SQLite 连接上执行 `BEGIN IMMEDIATE`、写入探针行并 `ROLLBACK`。它还检查扣除进程预留后的最低磁盘水位和停机状态；任一步失败都返回 `503`，而不会把启动时缓存的健康标志当作当前可写证明。

客户端统一通过 `GET /__dufs__/api/jobs/<UUID>` 查询当前账号的 mutation job；响应使用 `job_id` 字段和 `running/succeeded/failed/unknown` 状态。

目录页为 Rename 和 Move 提供独立按钮。Rename 与新建后的改名使用名称列中的单一行内编辑器；Move、覆盖、删除确认和操作错误继续复用页面内原生 `<dialog>`，不依赖浏览器 `prompt`、`confirm` 或 `alert`。行内输入具有可访问名称、错误状态和明确焦点恢复；Enter 提交，Escape 取消编辑。Move 对话框输入目标目录，关闭后焦点返回准确的发起按钮；目录页和登录页也为系统 forced-colors/高对比模式保留关键控件、焦点和对话框边界。浏览器门禁以固定 `@axe-core/playwright` 扫描登录页、文件页、行内编辑器和打开的操作对话框；自动扫描不替代真实读屏或人工可访问性验收。

新建空文件也使用完整上传协议并为每个默认名称候选生成独立 UUID。前端只有在 fresh PUT 返回 `200` 或 `201`、同一 upload ID、`committed` 且 length/offset 都精确为 0 时才报告成功；只有绑定 ID/长度且确定为 `destination_exists + not-started/rejected` 的结果才尝试下一个名称。若提交竞态留下 `awaiting-confirmation` stage，必须先以同一路径和 ID 得到明确的 discard `204`，才能换用新 ID；清理结果未知时立即停止。`202`/`204` 即使声称 committed 也属于协议异常，客户端只携带原 ID 做一次 HEAD 确认，不会重放 PUT；任何无法排除已经提交的结果都使列表失效，也不会继续创建另一个候选。

目录页的 DELETE、MOVE、RENAME、MKDIR、空 PUT 和普通上传共用单一 mutation 失效通知。确认成功或客户端无法排除已经提交时，现有分页 snapshot、游标和 DOM 都会标记失效并要求从第一页刷新；明确的服务端拒绝、终态失败或分发前取消则不误触发刷新。对于已经分发后报 unknown 的空 PUT，即使一次随后 HEAD 暂时返回 `not-seen`，列表仍按可能与原请求竞态处理并保守失效。

目录页 JavaScript 主动发起的 Fetch 在 30 秒 deadline 内按流读取响应；原生导航、登录表单和文件下载不在该客户端边界内。Fetch 错误体最多 16 KiB、成功体最多 16 MiB，先检查声明长度并在累计越界时立即取消；允许范围内直接重放已校验分块，不再先合并为第二份连续缓冲区。Problem Details 的 `detail`/`title` 最多接受 1024 个 JavaScript UTF-16 code units，超限整条丢弃。上传正文专用 XHR 在响应头、下载进度和最终 UTF-8 长度三个阶段拒绝任何超过 16 KiB 的响应（正常成功响应应为空），但不证明浏览器在事件前从未缓冲额外网络块。16 MiB 成功上限为最多 500 项、接近 Linux PATH_MAX 且可能经 JSON 转义放大的合法目录页保留余量。

### 统一错误反馈

目录列表/搜索 API、浏览器写 API、operation 错误结果和可表达的上传错误共用 RFC 9457 Problem Details 形式，响应类型为 `application/problem+json`。这是机器可读的公开协议；文件系统路径、内核错误和完整内部 error chain 只进诊断日志，不进响应。一个典型错误体如下：

```json
{
  "type": "urn:dufs:problem:operation_registry_full",
  "title": "Service Unavailable",
  "status": 503,
  "detail": "Operation registry is temporarily full",
  "code": "operation_registry_full",
  "recovery": "retry",
  "retry_after": 1,
  "operation_id": "00112233-4455-6677-8899-aabbccddeeff",
  "state": "rejected"
}
```

`type` 固定为 `urn:dufs:problem:<code>`，`title` 是 HTTP 状态摘要，`status` 必须与实际 HTTP 状态一致，`detail` 是可安全展示的具体说明，`code` 是稳定的小写机器标识。可选的 operation 扩展只使用平铺的 `operation_id`/`state`/`http_status`，上传扩展只使用平铺的 `upload_id`/`upload_state`/`upload_length`/`upload_offset`。前端只解析这一 canonical `application/problem+json` 结构，不接受旧 `message`、纯文本错误、vendor JSON 或嵌套/驼峰别名。

`recovery` 只能表达下列安全的下一步：

| 值 | 客户端含义 |
| --- | --- |
| `retry` | 本次失败已知可重试；若同时有 `retry_after`/`Retry-After`，不应更早开始 |
| `retry_with_new_id` | 原 operation/upload ID 不应重用，用新 ID 重新开始 |
| `resume_upload` | 从服务端确认的 durable offset 续传 |
| `query_job` | 只查询原 operation ID 对应的 job 状态 |
| `query_upload` | 只用 HEAD 查询原 upload ID 的持久状态 |
| `refresh_target` | 重新加载目标目录/资源后再由用户决定 |

未携带 `recovery` 表示服务端没有宣告安全的自动恢复动作；HTTP `5xx` 本身不等于可重试。即使错误体带 `retry`，只要权威 operation/upload 状态是 `unknown`，首方前端也不会自动重放写请求，而是查询或刷新核对。

响应冲突时，实际 HTTP 状态码的优先级高于错误体的 `status`；operation 的 `X-Dufs-Operation-Id`/`X-Dufs-Operation-State` 响应头的优先级高于体内副本；上传的 `X-Dufs-Upload-Id`/`X-Dufs-Operation-State`/`X-Dufs-Upload-Length`/`X-Dufs-Upload-Offset` 响应头同样是权威值，覆盖冲突还要求严格解析 `X-Dufs-Target-Revision` 与 `X-Dufs-Target-Replaceable`。体内扩展用于统一显示和日志关联，不能覆盖传输层事实。

Problem Details 不强行改写所有 HTTP 表示：登录导航和表单错误仍可返回 HTML，原生文件下载及其错误保持下载/纯文本语义。首方 API 的认证和 CSRF 失败也使用 Problem Details，但客户端仍先以 HTTP 状态和 `X-Dufs-Auth-Error` 分类。HEAD 响应不发送正文，上传状态必须从头读取；`204 No Content` 成功响应也不会为了“统一”而增加 JSON 体。

解析后仍位于共享根内的相对符号链接可以使用。绝对符号链接和指向根外的符号链接会被隐藏并拒绝访问。需要管理其他磁盘或 virtiofs 导出目录时，应先将其挂载到共享根内。DELETE 可以处理挂载文件系统内部的普通对象，但后台目录回收不会跨越被删目录中的下一层 bind mount 或嵌套挂载；遇到该边界会保留并退避 purge job，卸载后继续清理，而不会递归删除另一存储域的内容。

### 文件系统大小写说明

Dufs 按 Linux 大小写敏感语义处理路径、路径租约和内部暂存名称。共享根建议使用未启用目录 casefold（`+F`）的 ext4；使用 virtiofs 时，宿主导出目录也应能区分仅大小写不同的名称。若底层目录不区分大小写，`Foo` 与 `foo` 可能指向同一对象；普通路径操作不会探测、拒绝或兼容这种挂载。

## 命令行参数

```text
用法：dufs [选项] [共享目录] [命令]
```

| 参数 | 说明 |
| --- | --- |
| `[serve-path]` | 要管理的现有目录，默认当前目录；非目录会拒绝启动 |
| `-c, --config <file>` | YAML 配置文件 |
| `--state-dir <dir>` | 必填的 SQLite 状态目录；固定使用 `<dir>/state.sqlite3`，目录须为服务账号所有的 `0700` 非符号链接目录并与共享根分离 |
| `-b, --bind <ips>` | 监听一个或多个 IPv4/IPv6 地址，默认 `127.0.0.1`；配置结果不能为空 |
| `-p, --port <port>` | 监听端口，默认 `5000` |
| `-a, --auth <account>` | 添加一个拥有完整共享目录权限的账号 |
| `--log-format <format>` | 自定义 HTTP 访问日志格式 |
| `--log-file <file>` | 将日志写入文件 |
| `--max-upload-size <bytes>` | 单文件最大声明长度，默认 100 GiB |
| `--upload-idle-timeout <seconds>` | 上传正文最大空闲时间，默认 60 秒，最大 365 天 |
| `--upload-total-timeout <seconds>` | 单次上传总时限，默认 24 小时，最大 365 天 |
| `--max-concurrent-uploads <count>` | 同时上传数，默认 4 |
| `--min-free-space <bytes>` | 上传期间必须保留的可用空间，默认 1 GiB |
| `--max-connections <count>` | 活跃 TCP 连接上限，默认 256 |
| `--max-search-entries <count>` | 单次搜索最多检查的目录项，默认 10000 |
| `--max-concurrent-searches <count>` | 同时执行的列表或搜索数，默认 2 |
| `--request-timeout <seconds>` | 普通请求处理并生成响应头的时限，默认 300 秒、最大 365 天；不包含响应头发出后的文件流传输 |
| `-h, --help` | 显示帮助 |
| `-V, --version` | 显示版本和构建来源 Git SHA |

当前命令为 `hash-password`，用于交互生成配置所需的 Argon2id PHC；`help` 显示顶层或子命令帮助。

`--bind` 只接受 IP 地址，可以重复使用或用逗号分隔；主机名、文件路径和 Unix socket 路径会被拒绝。命令行参数覆盖 YAML 中的值。

`--request-timeout` 在响应头生成完成时结束计时。普通文件和单段 Range 正文传输没有总时长或最低速率限制，但服务端套接字连续 30 秒没有写入进展会关闭连接；它们仍受活跃连接上限约束。公网部署仍应由网关施加符合业务需求的响应总时长、最低速率和空闲策略。

三个 timeout 配置都必须大于零、不超过 31536000 秒，并且能由当前平台的单调时钟表示；不满足条件会在监听端口前阻止启动。

上传通过按 Linux `st_dev` 分桶的记账保护 `--min-free-space`，不同文件系统的预留互不影响。文件逻辑长度和约 1 MiB + 64 KiB 的 xattr/checkpoint/目录项等保守元数据余量分别按文件系统分配单元向上取整后预留；逻辑写入余量只按真实字节递减，分配单元的取整 slack 不会被当作额外容量。文件系统报告的 block 数与 fragment size 相乘或任一预算计算若溢出，会失败关闭而不会折返。`fstatvfs` 在 blocking 任务中取得空间快照，期间不持有共享预留锁；返回后只在同设备 revision 未变化时提交，最多重取 8 次，持续竞争时以 `WouldBlock` 失败关闭。该保证覆盖 Dufs 进程内部并发；外部进程、virtiofs 宿主机或存储侧变化仍可能竞争空间，生产配置应保留额外余量。

Dufs 后端使用明文 Hyper HTTP/1 handler，接受 HTTP/1.0 和 HTTP/1.1，不支持 HTTP/2 prior knowledge 或 `Upgrade: h2c`。10 秒请求头读取时限、64 KiB 接收缓冲上限和 `--max-connections` 因此适用于全部后端 HTTP 连接；HTTP/2 或 HTTP/3 应只终止在外部 HTTPS 网关，生产网关固定用 HTTP/1.1 回源。

## YAML 配置

```yaml
serve-path: /需要管理的目录
state-dir: /var/lib/dufs
bind:
  - 127.0.0.1
port: 5000
auth:
  - 'admin:$argon2id$…'
log-format: '$time_iso8601 $log_level $remote_addr $remote_user "$request" $status operation_id=$operation_id operation_state=$operation_state'
log-file: ./dufs.log
max-upload-size: 107374182400
upload-idle-timeout: 60
upload-total-timeout: 86400
max-concurrent-uploads: 4
min-free-space: 1073741824
max-connections: 256
max-search-entries: 10000
max-concurrent-searches: 2
request-timeout: 300
```

启动：

```sh
./target/release/dufs --config ./dufs.yaml
```

YAML 会拒绝未知字段和空的 `bind` 列表。`state-dir` 必须由 YAML 或命令行提供，目录必须满足上述私有目录约束，固定数据库目标不能是符号链接或目录；命令行显式指定时会覆盖 YAML 中的值。`max-search-entries` 必须位于支持的正数范围内，硬上限与直接列表的 100000 项保护一致。生产配置只来自命令行和 YAML，Dufs 不读取 `DUFS_*` 环境变量。

## 网关与反向代理

推荐部署拓扑：

```text
Edge / Firefox
      │ HTTPS
      ▼
网关或反向代理
      │ 内网 TCP
      ▼
Dufs
      │
      ▼
共享目录
```

默认回环监听适合网关与 Dufs 位于同一主机的部署，无需额外传入 `--bind`：

```sh
./target/release/dufs \
  -p 5000 \
  -a 'admin:$argon2id$…' \
  /需要管理的目录
```

网关位于其他主机时，可显式绑定服务器内网 IP；只有确有多网卡监听需求时才使用 `0.0.0.0`，并应使用主机防火墙只允许网关访问该端口。

Dufs 只支持部署在独立主机名的根路径 `/`，不支持 `/files/` 等 URL 子路径。网关必须把外部根路径原样转发到 Dufs 根路径；推荐浏览器入口形如 `https://files.example.com/`。

仓库中的 nginx 样例要求 nginx 1.24.0 或更高版本，编译时启用 HTTP SSL 与 HTTP/2 模块，并链接仍由上游或操作系统发行商提供安全更新的 OpenSSL；新部署优先使用 OpenSSL 3.5 LTS。`scripts/check-deployment.sh` 从包含空格、`&`、`#` 和反斜杠的真实 checkout fixture 读取部署文件，将运行时副本映射到安全名称后再启动隔离的真实 nginx 与 mock upstream。它不只做语法检查，还分别验证规范重定向、Host/SNI 拒绝、固定回源头与真实客户端 IP 覆盖、登录别名 4 KiB 限制，以及连接/请求速率限流的拒绝和恢复放行。

部署检查会在执行 `nginx -t` 前把生产 upstream 及全部 IPv4/IPv6 `80/443` 监听逐一改写到私有 Unix socket，并核对替换数量及无网络端点残留；因此检查不要求 root 权限，也不会占用宿主生产端口。

网关配置要求：

- 浏览器只访问网关提供的 HTTPS 地址，不能绕过网关直连后端；
- Dufs 后端只提供 HTTP；HTTPS 证书、TLS 协议和公网安全策略全部由网关负责；
- 网关到 Dufs 的回源协议必须固定为 HTTP/1.1，不能使用 h2c；浏览器到网关仍可使用 HTTP/2 或 HTTP/3；
- 只接受配置的规范 Host；未知 HTTP/HTTPS Host 应由默认 server 在握手或请求阶段拒绝，HTTP 到 HTTPS 的跳转必须使用固定规范域名，不能把客户端 `$host` 拼入 Location；
- 只接受规范域名，并以这个固定规范值覆盖上游 `Host`，同时把独立域名的根路径原样转发到后端，否则同源检查会失败；
- 从可信网关传递单值 `X-Forwarded-Proto: https` 和单值真实客户端 `X-Forwarded-For`；Dufs 只在直接 peer 为回环地址时采用 `X-Forwarded-For` 作为登录限流地址。`Host` 和 `X-Forwarded-Proto` 用于同源校验，因此后端监听地址必须通过回环绑定、私网 ACL 或防火墙保证只有覆盖这些头的可信网关可达；
- 不缓存登录、认证文件、Range、上传、API 或错误响应；
- 保留上游的 `Cache-Control: private, no-store`；
- 内部协议只使用规范 URI；尾斜杠、重复斜杠或非规范百分号编码不会被当成登录/API 的等价别名；
- 保留 Dufs 在读取登录正文前的来源 IP admission、短正文总 deadline、全局 token bucket、按“客户端 IP + 账号摘要”组合键的失败退避和向上取整的 `Retry-After`；网关同时应对登录路由族设置 `limit_req`、`limit_conn`、短正文时限和明确的 `429`；
- 强制把 HTTP 入口重定向到 HTTPS，并在确认域名只提供 HTTPS 后启用 HSTS；
- 必须使用独立主机名，不能与不可信应用共享同一主机名。

本项目不再提供内置 TLS。若网关不在同一主机，必须使用隔离私网、主机防火墙或等效 ACL，避免其他设备绕过 HTTPS 网关直接访问后端端口。

## 访问日志

常用变量：

| 变量 | 含义 |
| --- | --- |
| `$time_iso8601` | 请求完成时的 ISO 8601 时间 |
| `$log_level` | 本条访问日志的级别 |
| `$remote_addr` | 与 Dufs 建立 TCP 连接的客户端地址；经网关时通常是网关地址 |
| `$remote_user` | 已成功认证的用户名；未认证或认证失败时为 `-` |
| `$request` | 完整请求行 |
| `$status` | HTTP 状态码 |
| `$operation_id` | 写操作的规范 UUID；没有时为 `-` |
| `$operation_state` | 普通 operation 或上传响应的状态，可为 `running/succeeded/failed/rejected/unknown/committed/not-seen/not-started`；没有时为 `-` |
| `$http_...` | 请求头，例如 `$http_user_agent` |

Authorization、Proxy-Authorization、Cookie 和 CSRF 请求头会在自定义日志变量中脱敏。连接处理错误会记录 TCP peer、错误类别和系统错误码，便于定位网关 `502`、超时和协议问题。

示例：

```sh
./target/release/dufs \
  -a 'admin:$argon2id$…' \
  --log-format '$time_iso8601 $log_level $remote_addr $remote_user "$request" $status operation_id=$operation_id operation_state=$operation_state' \
  --log-file ./dufs.log \
  /需要管理的目录
```

设置 `--log-format=''` 可以关闭 HTTP 访问日志。

`--log-file` 使用不跟随符号链接的追加方式打开，只接受由当前服务用户拥有、且仅有一个硬链接的普通文件；新建和已有日志都会固定为 `0600`。进程不会在轮转重命名后自动重新打开路径，长期运行时应使用 journald、`copytruncate`，或在安全创建新日志后重启服务。

## 停止服务与 systemd

首次收到 SIGINT 或 SIGTERM 时，Dufs 会停止接受新连接，并给予普通任务及提交 30 秒宽限；到期后取消可取消任务、让停滞上传保存检查点或清理，再给予正在收尾的受跟踪工作最多 10 秒。若约 40 秒的进程内硬截止仍未完成，进程不再刷新日志，立即以状态 1 强制退出，不能保证卡住的提交已经落盘或尾部日志已经写出；第二次停止信号同样不刷新日志，立即以对应的 130/143 退出，SIGKILL 也会立即终止。正常路径完成受跟踪清理后，由专用命名 OS thread 只执行一次、最多 5 秒的日志刷新，再显式 `exit(0)`，避免 Tokio blocking pool 或 runtime drop 等待已取消但卡在内核/FUSE 的工作而突破上述时限；主任务在刷新期间仍优先监听第二信号并立即强退。

systemd 的停止超时应大于应用约 40 秒的硬截止并留出服务管理器余量：

```ini
[Service]
TimeoutStopSec=45s
KillSignal=SIGTERM
```

`45s` 只是最低余量示例；仓库提供的完整基线使用 120 秒并包含服务用户、只写共享根和 systemd 沙箱约束。调大 systemd 超时不会延长 Dufs 内建的约 40 秒截止，慢存储仍应通过容量规划、监控与演练控制，见 [`deploy/dufs.service`](deploy/dufs.service) 与[生产运维文档](docs/operations.md)。

## 内置页面

`assets/` 中的 HTML、CSS、JavaScript 和图标会在编译期固定写入可执行文件。运行时不读取外部页面目录，也不支持自定义 `404.html`。资源内容变化会生成新的摘要 URL，只有已知版本化资源使用长期缓存。

生产运行不需要 Node.js 或前端打包步骤；Node.js 仅在质量门禁和签名发布阶段使用。

## 本地检查

确认工具链：

```sh
rustc --version
cargo --version
cargo audit --version
```

若尚未安装依赖审计工具和门禁固定版本的覆盖率工具：

```sh
cargo install cargo-audit --version 0.22.2 --locked
cargo install cargo-llvm-cov --version 0.8.6 --locked
```

Rust 检查：

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo llvm-cov --locked --all-targets --all-features --fail-under-lines 70
cargo audit
```

审查文档记录的一次 `0.48.0` 验收快照中，Rust 行覆盖率为 77.40%（13,165 行中 2,975 行未覆盖）；后续代码会改变该固定数字，当前结论必须以本次 `scripts/check.sh` 的即时输出为准。门禁底线保持 70%，为平台错误分支和工具版本的轻微行号变化保留余量，同时防止大幅覆盖率回退。

首次准备前端测试：

```sh
npm ci
npm run test:frontend:install
```

如果 Playwright 报缺少 Linux 浏览器系统库，应先按其诊断安装依赖；在支持的发行版上可使用具有系统管理权限的 `npx playwright install-deps chromium firefox`，再重新执行上述浏览器安装命令。

运行桌面浏览器自动化测试：

```sh
npm run check:js
npm run check:types
npm run check:docs
npm run test:frontend:unit
npm run test:frontend
npm audit --audit-level=high
```

当前 Playwright 必需矩阵覆盖 Chromium 和 Firefox；已安装 Microsoft Edge 时可执行 `npm run test:frontend:edge`。测试通过本地 HTTPS 网关转发到 Dufs 的 HTTP 动态端口，与生产部署边界一致。`tests/data/key_pkcs8.pem` 是公开、固定且仅供 localhost 自动化使用的测试私钥，绝不能作为生产网关密钥部署。

完整本地检查可使用：

```sh
./scripts/check.sh
```

该门禁还会用生产解析器校验 YAML 示例，以占位可执行文件做 systemd 静态验证，并让真实 nginx 对 mock upstream 执行隔离行为测试；它不启动真实 systemd unit 与 Dufs/nginx 组合，生产数据副本上的启动、readiness 和 CRUD 冒烟仍是发布/部署必做项。门禁还执行原子发布目录的 no-clobber、Git replace/private-attributes 来源替换、许可证生成和 npm cache 播种自测、强制 Rust 行覆盖率基线，并以保守源码门检查 Markdown 的 inline/reference-style 本地链接和标题锚点；围栏代码块不参与链接解析，检查树中的符号链接会失败。JavaScript 安全检查使用固定的 Acorn 8.17.0 解析 AST，并以词法常量模型识别字符串拼接、模板、`join`、别名、反射及动态全局属性访问；动态 computed 解构的属性名无法静态求值时会失败关闭，变量声明、赋值表达式、默认参数、嵌套模式和 const alias 都有内置负例。TypeScript 5.9.3 另以 `allowJs + checkJs + strict + noEmit` 检查全部生产 JavaScript；请求、错误、上传协议、传输与 DOM 边界都用 JSDoc 从 `unknown` 显式收窄，显式或隐式 `any` 都不能绕过门禁。这是在保留原生 JavaScript 部署方式下的完整 strict 检查，但仍不等价于迁移为 `.ts`、ESLint 或完整跨过程污点证明。本地开发门在缺少 ShellCheck 时仍保持离线可用，但正式发布会失败关闭。Playwright 保留一次重试来收集诊断，但 `failOnFlakyTests` 会让“首轮失败、重试通过”仍然阻断门禁。发布包构建、签名验证、备份、升级和回滚步骤见[生产运维文档](docs/operations.md)。

`.github/workflows/read-only-ci.yml` 只在 `pull_request`、`push` 或人工触发时读取源码：工作流权限固定为 `contents: read`，checkout 不持久化凭据，所有 Action 固定到完整 commit SHA。静态层固定 Node 24.8.0、TypeScript 5.9.3 和经 SHA-256 校验的 ShellCheck 0.11.0；兼容层复验声明的 Node 18 下限；Rust 层固定 1.97.1；质量层运行 70% 行覆盖率、真实 nginx/mock upstream 部署行为、发布脚本自测和 release binary smoke；浏览器层按 lockfile 的 Playwright 1.61.1 与 `@axe-core/playwright` 4.12.1 分开运行 Chromium 和 Firefox。Playwright 会在 runner 工作目录生成 retain-on-failure trace，但当前工作流不向 GitHub 上传该诊断目录；启用远程 artifact 需要另行明确授权并重新审查其中可能包含的请求与页面数据。独立依赖审计工作流在 lockfile/manifest 的 push、PR、每周计划或人工触发时联网运行固定的 cargo-audit 0.22.2 与 npm audit，避免漏掉直接推送同时不让无关变更承担审计数据库网络噪声。runner 使用 `ubuntu-24.04` 标签，并在日志记录实际 `ImageOS`、`ImageVersion` 和工具版本；GitHub 托管镜像中的 Bash、Git、curl、内核和系统库并没有被仓库逐包钉死。远程门不接触签名密钥、不创建发布，也不替代发布 tag 上的完整 `scripts/check.sh`。

质量层把覆盖率、部署、发布脚本自测和 release binary smoke 作为独立步骤；只要各自前置条件成功且工作流未被取消，前一项实质检查失败不会跳过后面的独立检查，使一次运行尽量同时报告全部根因且避免缺少工具产生级联报错。

提交前还应执行：

```sh
git diff --check
git status --short
```

创建版本时应先确认工作树干净，再用发布脚本从与 Cargo 版本一致、精确指向 `HEAD` 的 Git tag 构建。脚本不会直接在可变 checkout 中跑发布门禁：它先从摘要锁定的 bare façade 生成并验证目标 commit archive，在没有 `.git` 的私有副本中以 `env -i`、固定工具路径/工具链及独立 HOME、Cargo home/target、npm cache 和临时目录强制执行完整 `scripts/check.sh`。Cargo 依赖先 vendor 后离线使用；npm 播种器只从 `package-lock.json` 的 HTTPS URL 与 SHA-512 integrity 接受并重新散列宿主 cache 内容，随后使用私有 cache 与 `prefer-offline`，缺失包和 `npm audit` 仍可能需要网络。发布门固定要求 cargo-audit 0.22.2。宿主 RustSec Git 数据库只有在 origin 是官方仓库、HEAD 与 FETCH_HEAD revision 一致且最近 7 天验证过上游时才会以 `--no-hardlinks` 复制并用 `--no-fetch` 离线审计；过期或不存在时强制在隔离 Cargo home 中联网刷新，网络不可用即失败关闭。门禁后通过独立 snapshot index 复验 tracked 内容/mode 和非忽略新增路径，再丢弃整棵质量树，从同一 commit 全新解包用于签名构建。

源树预检、隔离快照和每次解包检查会拒绝 symlink、submodule 及任何非普通文件/目录条目。脚本还会拒绝 Git replace refs、legacy grafts 和仓库私有 attributes；façade 只使用摘要锁定的最小 local config，所有 Git 命令清空 system/global 配置并禁用额外 attributes/replace。检查后、签名前和发布前都会重新确认 commit/tag/版本及原 checkout 的干净状态；前后两份源码 archive 还会复核 commit、tree、mode、额外路径和 SHA-256。

发布包包含经固定 `cargo-cyclonedx 0.5.9` 离线生成并规范化的 `dufs.cdx.json`，以及从 vendored、可达的非开发依赖生成的 `THIRD_PARTY_LICENSES.txt`。每个第三方包都必须声明非空 SPDX `license` 表达式；`license_file` 只能提供上游正文，不能替代缺失的表达式或充当分类 fallback。表达式按 `WITH > AND > OR` 的真实 SPDX AST 解析，只接受审核清单内的 identifier/exception，并要求存在完整 permissive 选择：`OR` 任一分支可选，`AND` 两侧都必须满足；只有明确列出的 Cargo 遗留写法会映射为 `OR`。生成器收集依赖声明的 `license_file` 及包根下全部常规 LICENSE/COPYING/NOTICE 文件；每个候选必须是该 vendored 包自身目录内的 no-follow 普通文件，并通过 UTF-8、非 NUL、非空与路径边界校验。项目 `LICENSE-MIT`/`LICENSE-APACHE` 不会替代缺失的上游文本；正文按 SHA-256 去重。

固定 Rust 1.97.1 sysroot 的 `share/doc/rust/COPYRIGHT-library.html` 还必须是 sysroot 内的 no-follow 普通文件，并精确匹配已审核 SHA-256 `0a65bb747c49c7bb816cbc7188319bd6e4e8d08091c1190b8a3c0971c47968ed`；未知工具链没有审核摘要时发布失败。它以 `RUST-STANDARD-LIBRARY-COPYRIGHT.html` 进入包内。`BUILD-ENVIRONMENT.txt` 使用 `dufs-build-environment-v2` 格式，记录完整源码 SHA、版本、`SOURCE_DATE_EPOCH`、host target、本次实际使用的 cargo-audit、RustSec advisory DB revision/最近 fetch epoch，以及 Bash、Rust、Cargo、cargo-cyclonedx、Node、npm、Git、OpenSSL、tar、gzip、mv 和 sha256sum 版本；它用于复现与差异诊断，不表示这些宿主工具已全部由仓库钉死。该清单、标准库 notice、项目双许可证、第三方 notice 和 SBOM 均纳入包内 `SHA256SUMS`；SBOM 规范化会移除本地构建路径并给 Dufs 组件绑定完整源提交，source revision 只接受恰为 40 或 64 位的小写十六进制对象 ID，但这不替代完整 CycloneDX schema 校验。签名私钥只在所有构建、SBOM、notice、归档和 checksum 工作完成后短暂打开，并只接受 Ed25519、Ed448、RSA ≥3072 bit 或审核曲线 `prime256v1`/`secp384r1`/`secp521r1`；弱 RSA、DSA、未审核 EC 曲线及非签名算法会失败关闭。生产发布仍应把构建和签名放在不同账号、主机或 HSM 信任域中，因为同一 UID 的恶意构建代码不可能仅靠 Shell FD 管理得到彻底隔离。不要把未经签名的临时二进制与正式制品混用。

## 目录结构

服务端共享对象按 `ContentServices`、`DurableStateServices`、`AdmissionControl` 和 `ServerLifecycle` 四类职责组合；路径策略、公开 wire protocol、请求分类/分发，以及 SQLite actor/database 和 operation/upload/purge 仓储也各自位于专门模块。该分层用于隔离内容访问、持久控制面、容量准入和生命周期所有权，不改变公开 HTTP 协议。

```text
.
├── assets/                         # 编译内置的浏览器页面源码
│   └── modules/                    # 无打包器的原生 ES modules
│       ├── shared/                 # DOM、路径和跨功能 mutation 契约
│       ├── http/                   # Fetch、Problem Details、响应预算与头解析
│       ├── listing/                # 分页列表、窗口化 DOM 与行内编辑
│       ├── operations/             # 文件操作和应用内对话框
│       └── upload/                 # 选择、协议、队列、传输、视图与任务编排
├── docs/
│   ├── README.md                           # 当前规范、教程与历史资料导航
│   ├── project-workflow.md                  # 当前实现流程与 Mermaid 流程树
│   ├── feature-inventory-and-tradeoffs.md   # 完整功能、边界与精简决策清单
│   ├── operations.md                        # 部署、备份、升级与回滚
│   ├── history/                             # 历史审查与整改记录
│   │   ├── code-review-report.md
│   │   └── browser-only-optimization-review.md
│   └── beginner-guide/                      # 从零理解项目的十章教程
├── deploy/                        # 经语法验证的 systemd、nginx 和 YAML 示例
├── scripts/                       # 质量门禁、部署校验和签名发布脚本
├── src/
│   ├── main.rs                     # 启动、监听和连接生命周期
│   ├── args.rs                     # 命令行与 YAML 配置
│   ├── auth.rs                     # 账号、会话与 CSRF
│   ├── server.rs                   # 服务共享状态与模块协调
│   └── server/
│       ├── assets.rs               # 内置资源注册、摘要与响应
│       ├── blocking_io.rs          # 全局有界阻塞文件系统准入
│       ├── browser_api.rs          # 新建目录、独立移动与重命名协议
│       ├── delete.rs               # DELETE 持久意图与文件系统提交事务
│       ├── disk_space.rs           # 按文件系统计算上传空间预留
│       ├── download.rs             # 文件下载、MIME 与 Range
│       ├── listing.rs              # 目录、搜索、排序与响应流程
│       ├── listing/
│       │   ├── snapshot.rs         # 共享快照、HMAC 游标与显式缓存生命周期
│       │   ├── tests.rs            # 列表、排序与遍历单元测试
│       │   └── walk.rs             # 有界递归遍历、快照复核与 worker
│       ├── login_rate_limit.rs     # 登录 token bucket 与失败退避
│       ├── identity.rs             # owner/root 等稳定身份类型
│       ├── internal_names.rs       # stage/trash 等内部保留名称
│       ├── maintenance.rs          # 上传与删除内部项的统一后台维护
│       ├── operation_registry.rs    # 普通写操作幂等状态与重放
│       ├── path_coordinator.rs      # 进程内路径写租约
│       ├── path_policy.rs           # 逻辑路径与内部路由策略
│       ├── problem.rs               # RFC 9457 错误表示
│       ├── protocol.rs              # operation/upload 公开状态 wire vocabulary
│       ├── purge.rs                # 持久 purge outbox、恢复/退避与分片 worker
│       ├── rooted_fs.rs            # 共享根 fd 与 Linux 文件操作
│       ├── rooted_fs/
│       │   ├── purge.rs            # fd-relative 分片递归删除执行器
│       │   └── tests.rs            # RootedFs 单元与边界回归测试
│       ├── router.rs               # 请求生命周期、超时与错误映射
│       ├── router/
│       │   ├── dispatch.rs         # 认证后端点与文件请求分发
│       │   └── request.rs          # 单次解析的请求分类与 mutation 进度
│       ├── session.rs              # 登录、注销与写请求校验
│       ├── state_store.rs           # schema v3 文件 SQLite 统一控制面 API
│       ├── state_store/
│       │   ├── actor.rs             # 有界命令 actor 与 live readiness 探针
│       │   ├── database.rs          # 数据库打开、schema 与恢复
│       │   ├── model.rs             # 三类仓储共享的领域模型与校验
│       │   ├── operation.rs         # operation 行为、查询与 row codec
│       │   ├── upload.rs            # upload session 行为、查询与 row codec
│       │   └── purge.rs             # purge job 行为、查询与 row codec
│       ├── storage.rs              # 可注入的持久化提交边界
│       ├── tests.rs                # Server 协调与 purge outbox 单元测试
│       ├── upload.rs               # 上传 façade、共享事务类型与阶段装配
│       └── upload/
│           ├── prepare.rs          # 路径准入、会话准备与 checkpoint 恢复
│           ├── target.rs           # 目标 identity、revision、响应头与冲突
│           ├── transfer.rs         # 正文接收、磁盘写入与 deadline
│           ├── commit.rs           # 元数据复核、原子发布与持久化终态
│           ├── failure.rs          # 空间、I/O、超时与 unknown 结果收口
│           ├── protocol.rs         # 上传头、选项与协议解析
│           ├── record.rs           # SQLite 上传状态与检查点
│           └── tests.rs            # 上传状态机与维护单元测试
├── tests/
│   ├── frontend/                   # Playwright 与前端单元测试
│   ├── http.rs + http/             # HTTP 集成测试入口与主题子模块
│   ├── browser_api.rs + browser_api/ # 浏览器 API 集成测试入口与主题子模块
│   └── *.rs                        # 其他 Rust 集成测试
├── Cargo.toml
├── Cargo.lock
├── LICENSE-APACHE
├── LICENSE-MIT
├── SECURITY.md
├── package.json
├── playwright.config.js
└── rust-toolchain.toml
```

## 许可证

Copyright (c) 2022 sigoden 及 Dufs contributors。

本项目按 [MIT License](LICENSE-MIT) 或 [Apache License 2.0](LICENSE-APACHE) 双重许可，使用者可任选其一。
