# 更新日志

本文件记录本项目的重要变更，并按版本从新到旧排列。

## 阅读约定

- [未发布]记录最新版本之后尚未发布的重要变更；带版本号的条目只描述对应 tag 的最终行为，较新的条目优先于与其冲突的旧记录。
- 版本 `X.Y.Z` 对应 Git tag `vX.Y.Z`。版本标题链接到相邻 tag 的源码比较页，首个版本链接到其源码树。
- 存在 changelog 条目或 Git tag 不等于存在可下载制品；公开二进制、SHA-256 文件及实际发布时间以对应 GitHub Release 页面和版本内的“发布状态”为准。
- 本 fork 从 `0.46.0` 起以中文整理发布说明；`0.45.0` 及更早的上游历史保留原有语言和分类，避免为统一样式改写历史含义。

## [未发布]

### 安全强化

- 状态目录启动校验现在沿完整祖先链复核目录类型与所有者；祖先必须由 root 或当前有效服务用户拥有，组/其他用户可写的祖先还必须具备 sticky bit，且其中通向状态库的受保护子项由可信用户拥有，拒绝可由非受信本地用户换名或替换的状态库路径。
- 原始 SQLite 主库校验快照限制为 1 GiB，并在复制前、复制期间和复制完成时核对大小边界与一致性，避免异常或竞态增长的状态库无界占用系统临时空间。
- 远程依赖审计与常规本地质量门现在先用 `cargo fetch --locked` 填充锁图所需的 crates.io 索引项，再执行 `cargo audit --deny yanked`；正式发布在封存 advisory 预审计后，以每次全新创建的私有 Cargo home 完成相同检查。缺少 registry 网络、索引项、抓取失败或锁图包含已撤回 crate 都失败关闭。

### 问题修复

- 修复多个未知单位的 `Range` 字段被误判为不可满足范围的问题：未知单位继续按 HTTP 语义忽略并返回完整 `200` 响应，重复的 `bytes` 范围仍按既有严格策略拒绝。
- 浏览器在确认提交返回契约完整、满 offset 的 `AwaitingConfirmation/query_upload` 响应时，会用同一 upload ID 查询 HEAD checkpoint 并重新取得覆盖确认；phase、length、offset、状态或 recovery 不一致的响应仍失败关闭，不会误提交不完整暂存。

### 发布与维护

- 新增严格的版本发布说明提取器，只接受 `CHANGELOG.md` 中唯一、非空且与目标语义版本精确匹配的版本段；GitHub Release 说明改为来自 tagged changelog，不再根据前一 tag 自动生成。
- GitHub 便捷二进制工作流拆分为无写权限的验证/构建 job 与唯一持有 `contents: write` 的发布 job；前者等待同一 tag/SHA 的只读 CI、依赖审计和正式包 E2E，后者不 checkout、不调用仓库脚本或执行输入，并以精确 artifact ID、固定 Action 和聚合摘要绑定二进制、SHA-256 与发布说明。
- 发布阶段使用分页 REST 状态、远端 digest/size 及实际回下载逐字节核验资产；受校验字段匹配的草稿或已公开 Release 可幂等续跑，普通异摘要、额外资产、受校验元数据或说明漂移一律拒绝，只有连续复核为同一 ID、`starter` 状态且零字节的中断上传残留才会按 ID 清理后重试。
- 新增只读的正式签名包 E2E：在版本 tag、每周计划或人工触发时，于含 shell 元字符的隔离 clone 中生成临时 Ed25519 密钥，真实运行未缩短的 `package-release.sh`，并从外部复核四项制品、checksum、签名/公钥、包内 `SHA256SUMS` 及二进制完整版本与源码 SHA；它不引用生产或自定义 secrets，也不上传测试制品。
- 正式发布隔离门即使位于含空格和 shell 元字符的输出路径，也会按 inode、属主和权限绑定发布 stage 内私有临时目录的锚点与实体路径，避免把 `/proc/self/fd` 魔术链接路径交给 SQLite `NOFOLLOW`；门禁前后会重新核对绑定，Nginx/sed 部署探针仍限定到 `/tmp` 下的私有随机目录。

### 测试

- 集成测试服务析构改为先发送 TERM、最多等待 5 秒，再以 KILL 兜底，使 LLVM 覆盖率 profile 有机会完整落盘；覆盖率门在仓库总行覆盖率至少 70% 之外，新增每个被插桩源码文件至少 1% 的下限。
- 新增回归测试覆盖重复未知 Range 单位、待确认上传的可信查询与畸形 offset、可换名状态目录祖先、状态库快照预先超限与复制中增长；发布说明提取、发布工作流及 yanked 审计准备顺序另有静态自测和安全退化 mutation fixtures。
- 配置解析的单元测试与部署样例集成测试都会把临时文件系统路径编码为兼容 YAML 的双引号标量，确保含空格、`&`、`#` 的正式发布隔离路径不会被注释语法截断。

### 文档

- 同步 README、运维手册、项目工作流、功能权衡清单和入门测试指南，准确说明 exact tag/SHA 门禁、发布权限隔离与草稿恢复、正式包 E2E、yanked 依赖审计、逐文件覆盖率门禁及远程自动发布的能力边界。
- 澄清 `0.49.2` 首屏目录冲突恢复的适用范围和有界重试条件，补充 `0.49.x` 的实际发布状态与升级要求。
- 增加更新日志阅读约定和全部版本比较链接，并说明本 fork 维护记录与上游历史的边界。

## [0.49.2] - 2026-08-23

> **发布状态：** [Dufs 0.49.2 GitHub Release](https://github.com/isarmg/dufs-ram/releases/tag/v0.49.2) 已公开；附注标签 `v0.49.2` 精确指向提交 `14bf8d307a4bb74764d79b72494acd5cdb90d7f3`，提供 `x86_64-unknown-linux-gnu` 二进制及配套 SHA-256 文件。

### 问题修复

- 不带 cursor 的目录首屏请求，包括初始列表、搜索首屏、显式刷新及 mutation 后刷新，若收到 HTTP 与 problem 状态均为 `409`、code 为 `directory_changed` 且 recovery 为 `refresh_target` 的完整错误，会自动重放同一幂等 GET 一次；这会吸收删除后后台隐藏回收等场景造成的单次目录快照冲突。
- 自动恢复严格限制为一次：第二次相同冲突、携带 cursor 的后续分页冲突，以及状态、code 或 recovery 不完全匹配的其他 `409` 都不会自动重放，而会停在显式 Retry 状态，避免无限请求或复用失效游标。

### 升级说明

- 从 `0.49.0` 或 `0.49.1` 升级不需要修改 CLI、YAML、SQLite schema 或外部 API；替换二进制并按正常流程重启即可。
- 从 `0.48.x` 或更早版本直接升级时，仍必须完成 `0.49.0` 的全部破坏性迁移，尤其是把 `--auth`/`-a` 迁入受保护的 YAML `auth`，并为反向代理显式配置窄范围 `trusted-proxies`。

### 测试

- 新增确定性浏览器回归：单次首屏冲突必须恰好重试并恢复列表，连续冲突只能请求两次且保留人工 Retry。

## [0.49.1] - 2026-08-23

> **发布状态：** 附注标签 `v0.49.1` 仅保留为不可变历史记录，没有公开 GitHub Release 或便捷二进制；其变更已包含在 `0.49.2` 中。

### 测试

- 在暂存替换测试夹具返回 Tokio 文件前显式完成异步写入，避免并行 CI 从另一文件描述符偶发读到尚未 flush 的空文件；该修复只稳定测试时序，不改变生产服务行为。

## [0.49.0] - 2026-08-23

> **发布状态：** 附注标签 `v0.49.0` 仅保留为不可变历史记录，没有公开 GitHub Release 或便捷二进制；其变更已包含在 `0.49.2` 中。

### 破坏性变更

- **破坏性安全变更：** 不再接受或展示 `--auth`/`-a`，以免 Argon2id PHC 通过 argv、`/proc`、服务管理器或 CI 日志泄露；所有账号必须迁移到权限受严格校验的 YAML `auth`，并通过 `--config` 启动。旧参数会在读取配置及创建运行时资源前以固定脱敏错误拒绝，不会静默覆盖 YAML。曾传递真实 PHC 的部署应清理 shell、服务和 CI 历史并轮换凭据；官方 systemd 样例已只使用配置路径，无需修改启动参数。
- 不再因直连 TCP peer 是回环地址而隐式信任 `X-Forwarded-For` / `X-Forwarded-Proto`；新增可重复的 `--trusted-proxy <IP[/CIDR]>` 与 YAML `trusted-proxies`，默认空并失败关闭。现有 HTTPS 反向代理部署必须显式声明直连网关地址；官方同机样例已配置 `127.0.0.1/32`。

### 安全强化

- 配置文件改为单次 no-follow、有界读取，并严格核验普通文件类型、所有者、权限、硬链接数、POSIX ACL 以及读取前后的身份和 metadata 稳定性；配置、日志、共享根、状态库与 SQLite 热 sidecar 之间的目录项和对象别名冲突会在启动前失败关闭。
- 既有日志文件必须预先是服务用户拥有的单硬链接 `0600` 普通文件；上传暂存文件使用私有权限，本地敏感运行文件不再进入版本控制。
- 状态库严格核验受支持 SQLite v2–v5 的 application id、共享根绑定、表、列、约束、索引、迁移结果和额外对象；任何连接打开前还会锚定并复核主库及 `-journal/-wal/-shm`，拒绝侧写文件遮蔽、身份替换及用合法 WAL 洗白外来主库。
- 代理信任配置拒绝单个或组合覆盖完整 IPv4/IPv6 地址空间的网段，避免误把任意直连来源视为可信网关。

### 问题修复

- 系统性修复续传和 `AwaitingConfirmation` 的长度、正文、磁盘空间、metadata、目标 revision、重启期限与条件清理语义，避免超量正文截断、误删检查点、污染目标版本或丢失可恢复暂存；过期会话维护与重启恢复也不再发生饥饿或无限延长期限。
- 恢复重启后的 DELETE 重试队列，保留停机清理凭据，并把深层目录清理改为有界迭代，避免失败回收永久遗留或耗尽调用栈。
- 修正 Range 单位大小写、未知单位、HEAD 与续传响应长度语义；下载阻塞读取统一经过并发门控和源读取空闲时限，防止请求超时后过早释放稀缺许可。
- 多监听地址改为全部原子绑定成功后再初始化运行时，拒绝重复地址并严格限制已接受连接；启动初始化、入口锁等待和停机排空会及时响应信号。
- 访问日志补全请求行，并在响应正文真正结束、失败或被丢弃后记录结果；日志队列回退、刷新、stdout 提示及启动失败路径不再丢失诊断或触发 panic。
- 修复分页窗口焦点丢失、不可寻址浏览路径处理，以及目录创建破坏续传状态的问题。

### 部署与维护

- 部署检查会在创建临时资源前安装清理 trap，对不退出的子进程实施有界 TERM/KILL，并为全部 HTTP 探针设置连接和总时限；不安全 `TMPDIR` 及清理残留现在会使检查明确失败。
- 构建版本会正确跟随工作树变化；移除不再使用的下载流依赖特性，并统一格式化审查修复代码与测试断言。

### 文档

- 同步 `v0.48.0` GitHub Release、受保护 tag 和自动便捷二进制已经公开的文档状态。

## [0.48.0] - 2026-08-22

### 变更

- 新增受版本标签触发的 GitHub 便捷二进制发布工作流：只接受与 Cargo 版本和 workflow commit 精确一致的 `v<version>` tag，等待同一提交的完整只读 CI 成功后，用固定 Rust 工具链构建嵌入完整 Git SHA 的 `x86_64-unknown-linux-gnu` 二进制并生成 SHA-256。工作流先在草稿 Release 中核对资产再公开，拒绝覆盖同名 Release，不创建或移动 tag，也不接触本地正式发布私钥；文档明确区分该无独立签名的直接下载制品与含 SBOM、许可证、构建环境记录及独立公钥签名的本地正式发布包。
- 将 New folder / New empty file 改为 Windows 式立即创建和行内命名：分别从 `newfolder` / `newfile` 开始，只有服务端原子证明重名时才以新 Operation/Upload ID 尝试 `(2)`、`(3)` 等后缀；成功后在名称列选中文字直接编辑。Rename 按钮也改为原位编辑，Enter、Tab 或合法失焦提交，Escape 取消编辑但不删除已经创建的默认项；文件默认只选中最后一个扩展名前的主体。单一编辑器继续受 1000 行 DOM 窗口约束，路径 mutation 使用稳定名称而非会因置顶而变化的索引。零字节 PUT 的晚到冲突若留下 `awaiting-confirmation` stage，必须先以同一路径/ID 得到明确 discard `204` 才能换候选；unknown、清理不可信或上传状态不绑定时立即停止，不会产生第二个可能重复的文件。
- 将移动与重命名从页面到后端协议完整拆分：每个目录项分别显示 Rename 与 Move 按钮；`POST /__dufs__/api/rename` 只接受单段 `name` 并保留父目录，`POST /__dufs__/api/move` 只接受已存在的 `directory` 并保留原名称。两条协议独立校验和报告状态，但复用路径租约、原子 rename、显式覆盖确认、Operation ID 幂等与结果未知处理；Move 不再隐式创建缺失的目标目录。
- 移除目录 ZIP 下载：删除页面级和目录行上的归档入口、ZIP 规划/生成/磁盘预留实现、专属并发与容量配置、相关测试，以及 `async-deflate-zip` 和 `unicode-normalization` 直接依赖。任何存在 `zip` 查询 key 的目录 GET/HEAD（包括 `?zip` 和 `?zip=1`）在兼容窗内稳定返回 `410 directory_archive_unsupported`，防止旧客户端把 HTML 列表误当归档；HEAD 不发送正文。本节下方的 ZIP 修复条目保留为移除前的实施历史，不再描述最终工作树功能。
- 在不改变外部行为和协议的前提下继续按职责整理目录：上传主流程拆为 `prepare`、`target`、`transfer`、`commit`、`failure`，内部名称与存储维护提升为服务端中性模块；Rust 内联单元测试迁入对应 `tests.rs`，大型 HTTP/Browser API 集成测试按主题拆分。前端按 `shared`、`http`、`listing`、`operations`、`upload` 分区，并提取 mutation 通知、对话框和上传选择等纯逻辑；TypeScript 配置与源码检查改为递归覆盖全部前端模块。文档增加导航页并把历史报告归档到 `docs/history/`；发布包完整保留 `docs/` 层次及教程引用的源码、测试和脚本支持树，除清单自身外全部普通文件进入 `SHA256SUMS`，发布自测会实际装配并检查包内链接。原有 HTTP 路径、方法、状态码、响应头、状态机和用户操作保持不变，且未新增运行时或开发依赖。
- state-store actor 只在连接/生命周期级故障时终止；单条 SQLite 命令返回错误后仍继续服务后续命令。`Abandon` 或未送达 reservation 的清理失败会按 operation ID 去重并延后，在后续命令边界重试，不再因一次清理错误杀死 actor 或永久遗留进程内清理责任。
- Library 配置与运行状态现在显式分层：`Args` 在唯一公开的 `ServerBuilder` 初始化路径中转换为不可构造的 `ValidatedConfig`，配置文件以 no-follow 普通文件方式做 1 MiB 有界读取；`Args::auth` 只含账号/PHC 的 `AuthConfig`，每个 Server 再建立独立 `AccessControl` 会话存储。`ServerBuilder`/`ServerRuntime` 要求活动 Tokio runtime，自动启动并收束 maintenance、停机 token 与 tracker；默认共享进程级列表缓存，也可显式选择实例隔离缓存。删除旧的公开 `Server::init` 和绕过请求策略的 `Server::handle`，CLI 与外部嵌入方都使用 builder/runtime，不保留 library API 兼容层；直接用 `AccessControl` 构造 `Args.auth` 的 struct literal 同样不再兼容。
- 文件系统阻塞工作统一经过 64 槽全局准入，permit 存活于实际 blocking closure，外层超时不能在 syscall 退出前提前释放。上传入口重构为 `UploadTransaction → TransferredUpload → ReadyUpload` 阶段所有权；快照页用 `Arc<[PathItem]> + Range` 避免逐页克隆字符串，游标改为标准 HMAC-SHA256 v3；operation registry 使用过期最小堆与 owner 计数，路径租约不再因无关 lease 启动全局失效，访问日志只计算格式实际引用的动态字段。
- 浏览器端上传拆为 queue/transport/view，等待队列支持 O(1) 取消、批量让出与可配置并发；每批最多 512 个最终绝对逻辑路径、解码后的 UTF-8 路径合计最多 256 KiB，预检 JSON wire body 最多 2 MiB，当前页最多 512 个 pending 行，终态历史仅保留最近 200 行并报告淘汰数。批次在入队前通过 `POST /__dufs__/api/upload/preflight` 按原顺序取得每个目标的存在性、可替换提示和 revision：没有冲突时零确认直接上传；只有已存在且可替换的目标进入覆盖/跳过/取消对话框，不能替换的目标不会被自动覆盖。DELETE、MOVE、RENAME、MKDIR、空 PUT 和普通上传统一通过 `committed/outcome-unknown/not-committed` 通知列表：成功或可能已提交时使分页 snapshot、游标和 DOM 失效，确定拒绝或分发前取消不误触发刷新。所有 fresh/resume/checkpoint 响应共用 phase-specific 协议矩阵。列表先完整验证页面再事务提交 DOM，并以 1000 行可访问窗口限制节点数；普通 API 按 JSON/no-content/HEAD/错误正文分离消费契约。登录样式移入内容寻址 `login.css`，CSP 移除 `style-src 'unsafe-inline'`，新增 Node 纯函数测试和 Chromium/Firefox 回归。
- 上传发布改为显式条件协议：`X-Dufs-Upload-Overwrite: false`（缺省同义）使用原子 no-replace，绝不会静默覆盖；`true` 必须同时携带 64 位小写十六进制 `X-Dufs-Target-Revision`，该 revision 绑定账号摘要、规范根内目标路径和完整目标 CAS identity，并在真正 rename 前再次核对。目标在预检后出现或变化时返回稳定冲突，不把旧确认扩展为无条件覆盖；未知、格式错误或无法证明的结果也不会触发自动覆盖。
- 完整 stage 在最终条件提交时发现目标变化，会以同一 upload ID、满 offset 和 `AwaitingConfirmation/awaiting-confirmation` 持久保留。浏览器只对该文件再次显示确认；接受时以同一 ID、满 offset、空正文 PATCH 和最新 revision 发布，不重传文件，若目标再次变化则继续失败关闭并再次确认；跳过时调用 `POST /__dufs__/api/upload/discard` 明确删除 stage。若该 stage 已重放旧目标 metadata、而目标随后消失，服务端以 `upload_metadata_preservation_refused` 拒绝把它当新文件发布，浏览器必须先 discard，再以新 ID 和完整正文执行 create-only PUT。
- 将本轮未发布整改版本更新为 `0.48.0`，恢复作者、主页和仓库元数据；正式发布必须由精确指向制品源码提交的 `v0.48.0` tag 生成，历史 `v0.47.0` 不再被误作当前整改版本。
- 默认监听地址从 IPv4 通配地址收紧为 `127.0.0.1`；跨主机网关和多网卡部署必须显式配置内网监听地址。空的 YAML `bind` 列表现在会在启动前被拒绝；多个 listener 不再在 `accept` 前长期预占全局连接许可，低连接上限不会让已经公布的监听地址永久饥饿。
- 上传空闲、上传总时限和普通请求时限现在统一要求不超过 365 天且能由平台单调时钟表示，极端值会在监听端口前阻止启动。会话增加每账号 32 个的公平上限；达到账号上限或全局容量时优先淘汰同账号最久未活动会话，避免单账号重复登录持续驱逐其他账号。
- `--log-file` 改为使用 `O_NOFOLLOW|O_APPEND|O_NONBLOCK|O_CLOEXEC` 安全打开，只接受当前服务用户拥有、单硬链接的普通文件，并将权限固定为 `0600`；新增符号链接、权限、属主、硬链接和 FIFO 非阻塞回归测试。
- 修复目录 ZIP 的跨平台路径穿越条目：归档名称现在只从真实相对路径组件逐段构造，在创建临时文件前预检全部待写文件，并拒绝 ASCII C0/DEL 控制字符、反斜杠、盘符、冒号/NTFS 数据流、Windows 不兼容字符 `< > " | ? *`、Windows 保留设备名（包括 `CONIN$`、`CONOUT$`、`COM0`、`LPT0` 及其扩展名形式）、组件前导/尾随 ASCII 空格或尾随点、非普通路径组件，以及经过 Unicode canonical normalization 和不区分大小写处理后的命名空间碰撞；不安全名称使目录 ZIP GET 返回 `409`，不会被静默改写。新增真实 `%5C` 上传攻击链、大小写、NFC/NFD、设备名文件/目录冲突以及 ZIP 本地头/中央目录原始名称回归测试。
- 后端 HTTP 协议收敛为 Hyper HTTP/1 handler（接受 HTTP/1.0/1.1），移除 `hyper-util server-auto`、明文 HTTP/2 prior knowledge 及其 `h2` 生产依赖；连接预算现在与单连接串行请求模型一致。生产网关到 Dufs 固定使用 HTTP/1.1，浏览器到网关仍可使用 HTTP/2 或 HTTP/3。新增真实 TCP connection preface 拒绝及合法 HTTP/1.1 `Upgrade: h2c` 不获 `101` 的回归测试。
- 上传总 deadline 现在从路径租约等待开始，覆盖正文帧、磁盘写入、flush、metadata 恢复和进入不可取消提交点之前的全部步骤；路径租约先于全局上传槽取得，热点路径排队不再占满无关上传容量。不可取消的文件同步/原子发布超出 deadline 时返回带 upload ID 的“结果未知”，后台任务继续持有租约直至安全结束。
- 浏览器上传新增 `transferring`、`submitting` 和 `unknown` 阶段：正文发送完成即停止传输 idle/total timer，单独等待服务端提交确认。普通 API 在分发前发现调用方信号已经取消时会明确报告未发起请求且不调用 `fetch`；一旦请求已分发，写请求的网络错误、取消或超时仍保守标记为结果未知。普通上传 XHR 一经发出，网络错误、取消或 idle/total/提交确认超时同样暂停队列；只有服务端明确返回 `running/rejected/not-started`，或人工 Retry 后的 HEAD 查询发生网络、取消或超时失败时，才保留 Retry，不会盲目重传可能已提交的内容。
- 前端上传头名、允许状态码及长度绑定解析集中到 `upload/protocol.js`，由普通上传、新建空文件和状态复核共享，避免协议矩阵在调用方间漂移；只保留按当前文件总长度验证的单一解析入口，不再导出宽松兼容解析器。Fetch 有界响应在验证全部分块后直接构造重放流，不再额外合并为第二份连续 `Uint8Array`，同时保持现有 `text()`、`json()` 和 `clone()` 行为。
- mkdir、move、rename 和 DELETE 必须携带客户端 UUID `X-Dufs-Operation-Id`；缺失或非 canonical UUID 会在 mutation 前返回 `400 invalid_operation_id`。服务端有界 registry 按账号隔离并校验请求指纹，在路径等待/业务校验前先建立 `Reserved` 记录：相同请求运行中返回 `202`、完成后幂等重放，不同请求复用 ID 返回 `409 rejected`。明确的 pre-commit 失败登记 `failed`，pre-commit guard 异常丢弃会移除预留并允许安全重试；只有显式进入 `CommitStarted` 后的异常丢弃才登记 `unknown`。容量全局 4096、每账号 1024，满额在 mutation 前返回 `503 rejected`；实际非上传 mutation task 另共用 64 个全局 admission permit，额外请求等待且仍受普通请求 deadline 约束。首方前端验证回显 ID 和 `running/succeeded/failed/rejected/unknown`；状态查询记录本身仍为四个非 rejected 状态，结果未知时只查询一次且不自动重放。
- 状态库只使用文件型 SQLite schema v3，统一持久化管理 `operations`、`upload_sessions` 和 `purge_jobs`；operation 只持久化稳定错误 code，不持久化英文展示文案。CLI `--state-dir` 或 YAML `state-dir` 是必填配置，固定使用私有 `0700` 目录内的 `state.sqlite3`；删除进程内 SQLite 和 `--state-db` / `state-db` 兼容入口。store 绑定共享根 dev/inode，使用 rollback journal `DELETE` 与 `synchronous=EXTRA`。schema v2 是唯一支持的旧版本，并在一个 `BEGIN IMMEDIATE` 事务内增加上传 revision/确认状态后升级为 v3；其他旧版本仍零修改拒绝。operation 容量为全局 4096/每账号 1024、终态 TTL 15 分钟；upload 容量 16384/4096、TTL 7 天；purge 容量 4096/1024 且未完成 job 无 TTL 逃生口。SQLite 与文件系统不属于共同事务，operation/upload 通过 `unknown`、purge 通过根内路径和 inode reconciliation 保守恢复。
- 新增统一 `GET /__dufs__/api/jobs/<UUID>` JSON 状态查询，首批直接复用现有 mutation operation registry；删除旧 `/__dufs__/api/operations/<UUID>` 路径。
- move overwrite 现在对不同名称但同一 dev/inode 的硬链接在预检和受跟踪 commit 内分别进行 fd-relative 复核，返回稳定的 `409 source_equals_destination`，不再把 POSIX rename 的无变化成功误报为 `204`。
- SQLite 中仍有物理路径义务时，命名空间 mutation 不再让根内相对路径静默失真：持有语义路径租约后，move/rename 在提交前检查源和派生目标，DELETE 在创建本次 `Prepared` intent 前检查目标，fresh PUT 在创建本次 stage/checkpoint 前检查目标。检查以有界 keyset 页覆盖活跃 upload target/stage、`Prepared` purge target/trash 以及 `Ready/Claimed` trash，并用已解析目录身份识别根内符号链接别名；冲突分别返回稳定的 `move_state_conflict`、`rename_state_conflict`、`delete_state_conflict` 或 `upload_state_conflict`，不会执行文件系统 mutation。
- DELETE 后台空间回收改为 SQLite durable outbox：可见 mutation 前持久化含相对目标/trash 路径和源 dev/inode/类型的 `Prepared`，通过身份复核的 rename 与父目录 fsync 后才转 `Ready`；容量全局 4096/每账号 1024，满载在 rename 前返回 `503 purge_backlog_full`。单 worker 原子 claim 为 `Claimed`，每片最多 256 项/25 ms；I/O 失败持久化回 `Ready` 并从 100 ms 指数退避到最长 30 秒，不再因固定失败次数丢 job。defer/complete 的 state-store 命令瞬时失败时会有界保留本地 claim，回读确认数据库仍为 `Claimed` 后再继续，避免回复丢失造成重复处理；重启将 `Claimed`→`Ready`。独立受跟踪 reconciler 在启动及运行期持续按路径+inode 处理 `Prepared`，瞬时 state-store 拒绝不再要求重启恢复。分片 cursor 只在内存，可从已记账 trash 根重建；递归打开使用 `RESOLVE_NO_XDEV`，不会越过 trash 根下的嵌套/bind mount。低频 maintenance 扫描只为未记账 orphan trash 兜底。
- 覆盖上传保留目标 numeric owner/group、除 setuid/setgid 外的 mode 及允许的非特权 xattr；原目标带 setuid/setgid 或任何 `security.*`/`trusted.*`（capability、SELinux、IMA/EVM、overlay 等）时拒绝覆盖。`user.*` 和 `system.posix_acl_access` 可精确重放；名称列表最多 64 KiB、条目最多 1024 个、单值最多 64 KiB，索引容量、名称和精确长度值的总分配最多 1 MiB。读取时先查询每个值长度再按需分配，不再为每个空值或短值预分配 64 KiB，并先移除 stage 上多余属性。目标/stage 身份和完整 stat 快照仍在 rename 紧前复核；策略、格式、权限冲突、多硬链接及非普通目标返回 `409`。fresh PUT 的祖先创建、空间准入和身份化回滚保持失败关闭；拥有共享目录写权限的外部进程仍属于明确信任边界。
- 上传会话由 schema v3 `upload_sessions` 管理，以 owner 摘要+UUID 绑定根内相对目标/stage 路径、总长度、durable offset、stage dev/inode、可选 target revision 和 `Running/CommitStarted/AwaitingConfirmation/Committed/Rejected/Unknown`。文件型 SQLite 是唯一状态权威，不再写入、读取或导入共享根内的 JSON 上传状态文件。首个检查点按 stage flush+fsync、stage 父目录 fsync、SQLite 提交的顺序建立，后续检查点复核已记录 inode；活跃 stage 路径跨 owner 唯一，PATCH 和清理只接受 DB 记录的同一 dev/inode，UUID 本身不作为删除能力。rename 前先持久化满 offset `CommitStarted`，重启转为 `Unknown`，发布与父目录 fsync 成功后才写 `Committed`；条件冲突则回到持久的 `AwaitingConfirmation`，发布后持久性或终态写入失败会尽力写 `Unknown`。会话容量全局 16384/每账号 4096，每次更新后 TTL 7 天；过期 `Running/AwaitingConfirmation` 只在库行、路径和 inode 一致时删除 stage，终态/歧义行不根据 stage 推断目标。前端使用 `running/awaiting-confirmation/committed/rejected/not-seen/not-started/unknown`；Retry 始终先 HEAD 原 ID，unknown 不自动重放。
- SQLite 现存文件会先以只读连接验证 application id、schema version、共享根绑定和完整性；当前 schema v3 可直接恢复写入，合法 schema v2 会在同一事务内迁移为 v3，只有这一个旧版本允许迁移。其他 schema、其他应用数据库、错误共享根或非 SQLite 文件均在零修改下拒绝。状态 actor 的有界队列满或已停止属于“命令未接收”：operation begin 与尚未进入 mutation 的命名空间 admission 以 `503 rejected + Retry-After` 明确返回；可能已经存在检查点的上传遇到其他 dispatch 失败时仍保持 `unknown/query_upload`。已入队后失联也继续按各协议保守处理。
- 目录列表/搜索、浏览器写 API、operation 错误结果和上传错误统一使用 RFC 9457 `application/problem+json`；标准 `type/title/status/detail` 外保留稳定 `code`，并只接受平铺 snake_case 的 operation/upload 扩展，不输出或解析旧 `message`、纯文本、vendor JSON 和嵌套别名。`recovery` 使用 `query_job` 查询未知 mutation。实际 HTTP 状态及 operation/upload 响应头仍是权威信号；首方 API 的认证/CSRF 错误同样结构化，但认证分类先于正文解析。HTML 登录、原生文件/ZIP 下载、HEAD 空正文和 `204` 成功响应保持各自的 HTTP 表示。
- 未预期 metadata、root guard 和文件系统错误不再伪装成 `404`；只有明确的不存在、非目录、安全隐藏的链接逃逸等情况返回未找到。`AppError` 统一把 I/O 类型映射到安全的 `400/403/404/409/504/507/500`，稳定 JSON API 错误使用机器可读 `code` 与面向用户的 `detail`。
- 完整文件 GET 与 Range 现在以 `O_NONBLOCK` 从共享根 fd 打开数据句柄，并在同一 fd 上确认仍为普通文件；路由分类后被外部写者换成 FIFO 不会阻塞 open。正文严格限长为该句柄的 metadata size；外部进程随后向同一 inode 原地追加不会使响应正文超过已声明 `Content-Length`。只接受一个请求头中的一个 Range，重复头和逗号多段均返回 `416`；`If-Range` 保守回退完整 `200`。
- 后端响应套接字增加 30 秒写入无进展超时；文件、Range 和已生成 ZIP 仍没有应用内总传输时长或最低速率，公网策略继续由网关补充。停机改为明确硬截止：首次信号后的 30 秒宽限同时等待普通工作和提交，force 后最多再等 10 秒；约 40 秒仍卡住时不再刷新日志并立即以状态 1 退出，第二信号也不刷新日志并立即以 130/143 退出。正常完成 tracked cleanup 后由专用命名 OS thread 只执行一次、最多 5 秒的日志 flush，再显式 `exit(0)`，避免 runtime drop 或耗尽的 Tokio blocking pool 等待卡住工作；主任务在 flush 期间以 biased select 继续优先响应第二信号。
- 目录列表和递归搜索改为账号隔离的服务端结果缓存：首屏只扫描和排序一次，后续页按 offset 切片。排序使用稳定索引归并算法，在索引构造、合并与置换的每个有界步骤检查取消/deadline。直接列表前后复核当前目录；递归搜索在访问前并于整轮完成后复核所有访问目录，可观察变化返回 `409`。DFS 深度/工作集为 1024/32 MiB，搜索结果另受 32 MiB 限制；active-ancestor `HashSet` 按最大深度预留，结果/计划 `Vec` 和名称字符串扩容前同时核算旧、新缓冲区峰值，预算允许时才几何增长。条目硬上限 100000，进程内缓存总计 32 份/64 MiB、每账号 8 份/32 MiB、TTL 120 秒。这些复核不是原子文件系统快照。
- 上传和 ZIP 的 `fstatvfs` 改由 blocking 容量任务异步复查；查询在共享 mutex 外执行，返回后只按同设备 revision 验证，最多重试 8 次并在持续竞争时失败关闭。block 数与 fragment size 相乘或预算计算发生整数溢出时同样失败关闭。上传把逻辑长度及约 1 MiB + 64 KiB 元数据余量分别按 `f_frsize` 取整后预留。ZIP 在 `std::env::temp_dir()` 实际文件系统上先持有约 1 MiB + 64 KiB 元数据预留，创建临时文件后核对 device；临时文件创建或写入在外部竞争下实际遇到 `ENOSPC`/`EDQUOT` 时稳定返回 `507`。逻辑输出 extent 独立按 `f_frsize` 取整且不把 rounding slack 当作逻辑余量。ZIP 整个 handler、规划/校验/生成 worker 和异步预留 waiter 都有 work tracker owner 并持有 permit；外层请求取消不会留下孤儿任务，归档就绪后才释放槽。
- ZIP 配置和运行时条目硬上限统一为 100000；blocking DFS 直接生成 `ZipEntryPlan`，不再先收集中间目录项向量，完整计划及预算化 `Vec`/`BTreeMap` Windows 名称空间索引合计受 64 MiB 限制，索引节点、key 字符串和扩容瞬时峰值均计入；递归工作集另受 32 MiB 限制。
- ZIP 计划为每个文件和目录保存 dev/inode、类型、链接数、大小、mode 及纳秒级 mtime/ctime。生成时文件用 `O_NONBLOCK` 打开并从同一 fd 在复制前后复核完整快照；目录条目和全部访问目录也在 append/finalize 前复核。外部替换、FIFO 或可观察版本变化整体返回 `409`，不会把不同对象或版本静默混入成功归档。
- ZIP 现在显式写入空目录，移除服务器 numeric UID/GID 和特殊权限泄漏，并把目录、普通文件及可执行文件 mode 规范化为可移植安全值。
- 登录在读正文前同时使用全局 burst 16/每秒补充 1 个及来源 IP burst 8/每秒补充 1 个的 token bucket；正文读取另受全局 32/每 IP 4 个并发许可、4 KiB 上限和 10 秒 deadline 约束，解析后再按“客户端 IP + 账号摘要”组合键计算失败指数退避，避免一个来源用错误密码把同一账号在其他来源全局锁定。`Retry-After` 对剩余时间向上取整，并由重定向后的最终 `429` 错误页返回；POST 的 PRG `303` 不再携带会被解释为“延迟跟随重定向”的该字段。只从回环代理信任严格单值的真实 IP 头。内部端点拒绝尾斜杠、双斜杠和非规范百分号编码，因此 `/__dufs__/login/` 不能绕过官方网关的登录 location。同源校验同时比较外部 scheme 和 authority，并严格拒绝多值或畸形转发头。每账号会话上限、应用限流、网关 `limit_req`/`limit_conn` 和真实 IP 限流共同生效。
- 原始密码边界统一为非空且最多 1024 个 UTF-8 字节：`hash-password` CLI、公开哈希入口、服务端登录解析和浏览器端 `TextEncoder` 校验复用同一限制。登录输入不再用 UTF-16 code unit 语义的 `maxlength`；精确内容的校验脚本由 CSP SHA-256 hash 单独授权，并有 ASCII/多字节边界和 Chromium/Firefox 回归测试。
- 未认证 GET/HEAD 的 HTML 导航判断改为逐个 `Accept` 字段、逐个逗号项精确匹配 `text/html`，且可选 `q` 必须语法有效并大于 0；`text/htmlx`、`text/html;q=0`、重复或畸形 `q` 不再触发登录重定向，而是返回 `401`。
- `RootedFs` 启动时在共享根目录 FD 上取得非阻塞独占锁，第二实例会拒绝启动；递归删除改为纯 `openat`/`unlinkat`、`O_NOFOLLOW` 的 FD-relative 实现，不再依赖 `/proc/self/fd`。路径协调器的语义键解析错误不再被 `.ok()` 吞掉后退化成纯词法锁，也不无限重试：失败请求改用与全部路径冲突的共享根 wildcard 租约，随后由实际根边界或文件系统检查返回错误。较早 waiter 仍在解析语义键时只阻塞词法祖先/后代，无关路径可超车；解析完成后仍按语义键和 epoch 重验，保证别名冲突安全且避免慢解析形成全局队头阻塞。
- 健康检查拆为公开、无敏感信息的 `/__dufs__/health` liveness 和受认证的 `/__dufs__/ready` readiness；后者通过锚定共享根 fd 创建隐藏文件、写入并同步文件、删除并同步根目录，同时让 state-store actor 在现有 SQLite 连接上执行 `BEGIN IMMEDIATE`、写探针并 `ROLLBACK`，还检查扣除进程预留后的磁盘水位及停机状态。它验证当前读写路径而不只读取启动时健康标志，也不把业务配额当作完整接受性预测。内置摘要资源的 HEAD 返回与 GET 一致的状态和头但无正文；目录 ZIP 以 `405 Allow: GET` 拒绝 HEAD。目录页 JavaScript 主动发起的 Fetch 统一使用 30 秒 deadline；原生导航、登录表单和文件/ZIP 下载不在该客户端边界内。Fetch 对错误/成功响应分别执行 16 KiB/16 MiB 流式硬上限；上传 XHR 在声明长度、progress 和最终 UTF-8 长度三层拒绝任何超过 16 KiB 的响应。tracked operation 成功必须回显同一 ID 和 `succeeded`；普通上传只在 ID、状态和精确长度/offset 均匹配时报告成功，任何 unknown 都暂停队列而不盲目重放。
- 将 ZIP 管线从 `listing.rs` 提取到独立 `listing_zip.rs`，分页结果、ZIP、安全登录限流和 operation registry 均形成专门模块。JavaScript 门禁固定使用 Acorn 8.17.0 解析 AST，再以词法常量分析覆盖字符串拼接、模板、`join`、别名、反射、动态全局属性访问及 `alert/confirm/prompt` 原生模态别名；任何 computed 解构在变量声明、赋值表达式以及默认参数（含嵌套/const alias）中无法静态求值时都失败关闭，并用含运行时传入 `globalThis` 的内置正负对抗样例验证。TypeScript 5.9.3 以 `allowJs + checkJs + strict + noEmit` 检查全部生产 JavaScript，外部/解析输入保持为 `unknown` 并经守卫收窄，生产源码不保留显式或隐式 `any`；五个 Bash 源始终执行 `bash -n`，本地已安装 ShellCheck 时执行 warning 门，CI 则固定并强制使用 0.11.0。Acorn 与 strict `checkJs` 分别承担防御纵深和类型门禁；后者无需先迁移 `.ts`，但不等价于完整跨过程污点证明或 ESLint。Markdown 本地链接、安全响应头与隔离 Playwright fixture 继续纳入检查。
- 新增只读分层 GitHub Actions：权限固定为 `contents: read`、checkout 不持久化凭据，Action 使用完整 commit SHA；静态、Rust、Chromium/Firefox 矩阵固定关键工具版本并记录托管 runner 实际环境。工作流不取得签名/发布权限，不替代 exact tag 上的完整本地质量与发布门。
- 构建版本嵌入 Git SHA；发布脚本先从摘要锁定 bare façade 取得经 tree/mode 校验的 commit archive，在无 `.git` 私有副本中以 `env -i`、固定工具链和隔离 HOME/Cargo/npm/target/tmp 强制执行完整 `scripts/check.sh`。Cargo vendor 后离线；npm cache 只按 lockfile HTTPS+SHA-512 重新验证播种并 prefer-offline；可用 RustSec DB 以无硬链接私有 clone 配合 `--no-fetch`。门禁后 snapshot index 复验 tracked 内容/mode 和非忽略新增路径，丢弃质量树，再 fresh extract 构建；检查后、签名前和发布前反复验证干净 HEAD、精确 tag 与版本。源预检、隔离快照和解包树拒绝 tracked symlink、submodule 及任何特殊文件；同时拒绝 replace refs、legacy grafts 和私有 attributes，并双重核对构建/打包 archive 的 tree/mode/额外路径/SHA-256。
- 固定 `cargo-cyclonedx 0.5.9` 离线生成规范化 SBOM；从 vendored 可达非开发依赖生成 `THIRD_PARTY_LICENSES.txt`。每个包必须有非空、经审核的 SPDX `license` 表达式，`license_file` 只用于收集文本，不能替代缺失表达式或作为分类 fallback；真实 SPDX AST 还会验证审核 identifier/exception 和完整 permissive 分支。生成器收集依赖自身 `license_file` 与全部常规 LICENSE/COPYING/NOTICE，拒绝项目许可证 fallback、symlink、逃逸、非 UTF-8、NUL、空文本并按内容摘要去重。固定 Rust 1.97.1 sysroot 标准库 copyright 必须是根内 no-follow 普通文件并匹配审核 SHA-256，以 `RUST-STANDARD-LIBRARY-COPYRIGHT.html` 入包；未知工具链无审核摘要即拒绝。发布包新增 checksum 覆盖的 `BUILD-ENVIRONMENT.txt`，记录源码 SHA/版本/epoch/target 和本次 Bash、Rust/Cargo、Node/npm、Git、OpenSSL 及归档/coreutils 工具版本，用于复现诊断而不冒充全宿主链钉扎。SBOM 只接受精确 40 或 64 位小写十六进制 source revision，并连同构建环境清单、项目双许可证和两类 notice 纳入包内 checksum；规范化不替代完整 schema 验证。签名密钥仅在全部构建/notice/checksum 和 exact-source 检查后短暂打开，只允许 Ed25519、Ed448、RSA ≥3072 bit 或审核的 P-256/P-384/P-521 ECDSA，所有校验/签名/验签失败显式传播；发布目录仍以一次 rename 原子 no-clobber 提交。部署检查从包含特殊字符的真实 checkout fixture 启动隔离 nginx/mock upstream，分别断言未知 SNI、合法 SNI 下未知 Host、伪造入站 XFF 被 `$remote_addr` 覆盖，以及连接/请求限流先 `429` 后恢复 `200`。
- 发布目录的 no-clobber 提交兼容 Ubuntu 24.04 的 GNU coreutils 9.4：使用 `mv --update=none --no-copy`，再以 source 消失和 destination 设备号/inode 一致证明提交；空目标目录碰撞自测会把静默跳过判为失败。原子保证要求发布文件系统支持 Linux `RENAME_NOREPLACE`，CI 记录实际 `mv` 版本。
- 主页面移除 538 px 固定最小宽度；不超过 537 CSS 像素时工具栏换行、文件列表改为两行网格、长文本折行/截断，修改时间和大小移到第二行并保持可见。Playwright 在 320 CSS 像素断言这些字段与核心操作可达且页面无横向溢出，对应 1280 px 桌面 400% 缩放；这不改变手机 Web 不受支持的产品边界。
- 新建、移动、覆盖、删除和操作错误改用可访问的页面内原生 `<dialog>`，替代浏览器 `prompt`、`confirm` 和 `alert`；对话框具有标题、说明、显式输入标签、Enter/Escape 键盘语义，并在关闭后恢复触发控件焦点。目录页和登录页补充 forced-colors 样式与 Playwright 语义/焦点回归，并以固定 `@axe-core/playwright 4.12.1` 扫描登录页、文件页和打开的操作对话框；超过 128 个文件的串行上传用例仅把最终 129 行渲染断言预算设为 30 秒，没有放宽产品或全局超时。
- 将编译内置的登录页和目录文件管理界面的 HTML 语言声明、用户可见文案、状态、错误提示及可访问名称统一改为英文，并同步更新服务端登录错误和浏览器自动化断言；README 与说明文档正文继续使用中文。
- 移除 `--path-prefix`、YAML `path-prefix` 和 URL 子路径部署支持，Dufs 现在只部署在独立主机名的根路径 `/`；同时移除 `--hidden`、YAML `hidden`、名称 glob 过滤及其依赖，目录列表、搜索和 ZIP 现在包含所有普通文件与目录，Dufs 内部上传暂存、状态和删除回收项仍不可见。

### 文档

- 新增完整功能与取舍清单，逐项记录当前功能、默认值、安全与持久化边界、删除影响、直接依赖和可回溯的精简决策 ID；同时明确 HTTPS 安全上下文要求、锁定依赖的构建命令及内置资源摘要的准确长度。
- 新增生产运维手册、安全报告策略和完整整改状态表，部署、健康检查、备份、恢复演练、制品验证、升级与回滚不再只作为口头约定。

## [0.47.0] - 2026-07-24

### 变更

- 将本地包版本更新为 `0.47.0`，移除上游作者元数据并保持 `publish = false`；版本管理只使用本地 Git。
- 将项目升级到 Rust 1.97.1 与 Rust 2024 edition；`Cargo.toml` 通过 `rust-version` 声明最低版本，`rust-toolchain.toml` 固定本地 `rustc`、Cargo、Clippy 和 Rustfmt 工具链。
- 将 Dufs 服务端收敛为仅支持 64 位 Linux；仓库根目录的 `build.rs` 会明确拒绝其他 Cargo 目标。运行时还要求 Linux 5.6 或更高版本提供 `openat2`，禁用该系统调用或不支持的环境会在启动阶段失败，不会降级。
- 将每台系统只运行一个 Dufs 实例作为部署约定；文档不再设计多实例协调或跨进程文件锁，程序本身也不使用 PID 文件或共享根锁阻止第二实例。新增进程内子树路径协调器，PUT、PATCH、DELETE、mkdir 和 move 的源/目标统一参与：同路径及祖先/后代写操作串行，互不为祖先的不同子树可以并行；多路径先排序、去重，再在同一个临界区整体检查并登记。等待冲突租约期间，只要协调器租约集合版本变化，就重新解析符号链接语义键。它支持个人多设备同时进行无冲突操作，但不直接观察或约束 shell、宿主机及其他进程的变化。
- 服务启动时长期持有共享根目录 fd；`RootedFs` 的最终文件打开、创建、替换、移动和删除通过 `openat2` 以及父目录 fd 上的 `openat/mkdirat/renameat2/renameat/unlinkat` 完成，目录持久化使用 fd 上的 `fsync`。可能创建祖先目录的操作共享短临界区，并覆盖最终目录发布和父目录同步，避免兄弟请求同时创建共同父目录时由尚未完成的另一请求承担落盘责任。相关调用固定使用 `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`：解析后仍在根内的相对符号链接可用，绝对链接和根外目标会被拒绝；悬空或成环的根内相对链接可在目录中显示，并可用 DELETE 删除或 PUT 替换，GET 仍返回 `404`。
- move 的 `overwrite: false` 使用父目录 fd 上的 Linux `renameat2(RENAME_NOREPLACE)` 原子提交：最终目标存在时返回 `409`，Linux 内核或文件系统不支持该原语时失败关闭，不回退到可能覆盖目标的普通 rename。`overwrite: true` 使用 `renameat` 原子替换；两种成功路径都在 `204` 前 `fsync` 源和目标父目录。
- 移除了生产运行的全部 `DUFS_*` 环境变量配置入口；运行配置现在只来自可选 YAML 和命令行，命令行值覆盖 YAML。Playwright 内部测试变量不属于生产配置，继续保留。
- YAML 配置启用严格未知字段检查；拼写错误以及 `allow-symlink` 等已经删除的字段会指出配置文件和字段并阻止启动，不再被静默忽略。
- 移除了内置文件预览和文本编辑器；共享文件改为以附件形式下载。
- 简化了认证机制，使每个已配置账号都拥有共享根目录的完整访问权限；账号配置固定为 `user:<argon2id PHC>`。
- 移除了匿名路径规则、账号级角色、`-A` 以及上传、删除、搜索、归档和哈希权限开关；同时彻底移除 `--allow-symlink` 及 YAML 同名配置，所有账号虽然拥有完整文件管理能力，但始终不能通过符号链接越过共享根。
- 移除了匿名访问；启动时现在要求至少配置一个账号。
- 用内置中文 HTML 表单（`GET/POST __dufs__/login`）、真正的 `POST __dufs__/logout` 端点和 Argon2id 密码验证取代了 Basic/Digest 认证；账号配置只接受完整、有效的 Argon2id PHC，明文、SHA-crypt、其他 Argon2 变体和无效 PHC 均会被拒绝。配置与登录表单共用 128 字节用户名上限。`dufs hash-password` 可交互生成所需 PHC，配置错误只报告账号序号和错误类型，不回显完整账号输入。
- 登录页采用与 UnionC 通用内容块一致的 3:2 六行圆角卡片：账号、密码、错误提示和登录操作各占固定行，并保留现代桌面浏览器的浅色、深色适配。静态样式迁入内容寻址 `login.css`，CSP 收紧为 `style-src 'self'` 并移除 `'unsafe-inline'`。
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
- 文件下载改为从根 fd 打开一次文件，并从同一句柄取得 metadata 和正文；附件 MIME 只按扩展名映射，未知名称使用 `application/octet-stream`，不再读取样本、seek 或猜测 charset，因此移除 `content_inspector` 与 `chardetng`。Range 改用 64 KiB `ReaderStream`，不再直接依赖 `pin-project-lite`；固定十六进制编解码由本项目的小型边界测试实现承担，并移除 `hex`。metadata 预检后被另一设备并发删除或移动导致的 `ENOENT`/`ENOTDIR` 返回 `404`。显式 Range end 超出文件末尾时截断，超长 suffix 返回完整文件，多段仍返回 `416`。ETag 改为包含设备号、inode、长度和纳秒级 mtime/ctime 的弱验证器；带 `If-Range` 的请求返回完整 `200`，不带它的合法单段请求返回 `206`。
- 对所有登录和认证响应统一强制执行 `Cache-Control: private, no-store`，覆盖文件 GET/HEAD、单段 Range、条件响应、目录 ZIP、上传、API、错误和内部 `500`，且不依赖文件验证器是否可用；只有成功返回的版本化内置脚本、样式和图标保留公共长期缓存。下载文件名改为固定安全 ASCII `filename` 回退名和 UTF-8 `filename*` 真实名称，避免引号、反斜线、分号、控制字符及非 ASCII 名称形成歧义响应头。
- 将上传改为 Linux 崩溃持久性协议：完整 PUT 请求先暂存在目标文件旁，随后执行文件同步、同文件系统原子 rename 和父目录 `fsync`。这三个最终步骤成功即决定 HTTP 成功；后续检查点删除或其目录同步失败只记录告警并交给 TTL 维护重试，不再把已提交文件错误报告为 `500`。覆盖语义明确为发布新 inode、只复制普通 permissions；不保留 owner/group、POSIX ACL、扩展属性或硬链接身份，其他硬链接继续读取旧 inode 内容。该保证仍取决于 Linux 文件系统、NFS 等网络存储、设备和固件正确兑现同步请求。
- 整个上传处理从开始起进入独立 mutation task，并持有请求体、路径租约、活跃 stage/state 租约和清理责任；浏览器断开或网关取消外层 HTTP future 不会提前释放租约。mkdir、move 和可见删除的最终文件系统变更使用相同跟踪机制。新增取消回归测试验证外层等待被取消后，内层写事务仍阻塞冲突祖先路径直至实际结束。
- 将浏览器重试改为使用 UUID 上传会话：PUT 必须携带 `X-Dufs-Upload-Id` 和 `X-Dufs-Upload-Length`，PATCH 还必须携带与服务端持久化检查点完全一致的 `X-Dufs-Upload-Offset`。持久化辅助文件绑定初始总长度并只记录已同步的偏移量；HEAD 返回该检查点，PATCH 在恢复上传前截断所有未写入检查点的尾部数据。
- 删除浏览器跨刷新 `localStorage` 续传身份：文件名、长度和 `lastModified` 不能证明内容相同，重新选择文件始终使用新 ID 完整 PUT，避免把不同内容拼接到旧检查点。同一页面内的同一个 `File` 对象仍可保留 upload ID，失败重试先 HEAD 验证服务端持久化 offset，再 PATCH。
- PUT 或 PATCH 首次收到带 `X-Dufs-Auth-Error: csrf` 标记的 `403` 时会原子暂停整个前端队列，显示统一提示并禁止失效页面继续发出请求。普通网络错误、`5xx` 和没有认证错误标记的业务失败保留当前页面内的重试；刷新或重新登录后重新选择会建立全新上传。PUT 到已有目录返回 `409`，不会被误判成认证过期。
- 移除了拖放上传及非标准的 `webkitGetAsEntry` 目录递归实现；页面只保留文件拖入的默认导航拦截，实际上传改由独立的多文件选择器和现代浏览器文件夹选择器触发。
- 服务端 stage/state 采用 7 天 TTL：维护任务在启动时立即扫描一次，随后每小时扫描；扫描从启动时持有的共享根 fd 逐级枚举，不会因运行中替换启动路径而切换到新目录。活跃上传使用“父目录设备号/inode + 内部文件名”语义键登记，根内符号链接别名不会导致维护任务误删；每个候选在短暂持锁时复核并登记 maintenance marker，实际 open/unlink 在锁外执行。新上传等待 marker 时同时遵守 deadline 与 force-shutdown；清理后同步父目录并记录日志。
- PUT/PATCH 缺少 upload ID 或总长度、PATCH 缺少 offset 时返回 `400`；upload ID 不存在时返回 `404`；总长度或 offset 与持久化状态不一致时返回 `409`。所有写入只使用当前会话式上传路径。
- 移除了 CORS 配置和响应头注入；内置浏览器 UI 现在仅作为同源客户端运行。
- 移除了 `--assets`、运行时浏览器 UI 覆盖功能和自定义 `404.html`；编译时内置的 UI 现在是唯一的管理界面。JavaScript、CSS 和图标共同生成完整 256 位 SHA-256（64 个十六进制字符）内容摘要前缀，资源内容改变即更换 URL；只有成功返回的已知摘要资源可以长期公共缓存，未知资源 404 使用 `private, no-store`。
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

<!-- 版本比较链接由本 fork 手工维护。 -->

[未发布]: https://github.com/isarmg/dufs-ram/compare/v0.49.2...main
[0.49.2]: https://github.com/isarmg/dufs-ram/compare/v0.49.1...v0.49.2
[0.49.1]: https://github.com/isarmg/dufs-ram/compare/v0.49.0...v0.49.1
[0.49.0]: https://github.com/isarmg/dufs-ram/compare/v0.48.0...v0.49.0
[0.48.0]: https://github.com/isarmg/dufs-ram/compare/v0.47.0...v0.48.0
[0.47.0]: https://github.com/isarmg/dufs-ram/compare/v0.46.0...v0.47.0
[0.46.0]: https://github.com/isarmg/dufs-ram/compare/v0.45.0...v0.46.0
[0.45.0]: https://github.com/isarmg/dufs-ram/compare/v0.44.0...v0.45.0
[0.44.0]: https://github.com/isarmg/dufs-ram/compare/v0.43.0...v0.44.0
[0.43.0]: https://github.com/isarmg/dufs-ram/compare/v0.42.0...v0.43.0
[0.42.0]: https://github.com/isarmg/dufs-ram/compare/v0.41.0...v0.42.0
[0.41.0]: https://github.com/isarmg/dufs-ram/compare/v0.40.0...v0.41.0
[0.40.0]: https://github.com/isarmg/dufs-ram/compare/v0.39.0...v0.40.0
[0.39.0]: https://github.com/isarmg/dufs-ram/compare/v0.38.0...v0.39.0
[0.38.0]: https://github.com/isarmg/dufs-ram/compare/v0.37.1...v0.38.0
[0.37.1]: https://github.com/isarmg/dufs-ram/compare/v0.37.0...v0.37.1
[0.37.0]: https://github.com/isarmg/dufs-ram/compare/v0.36.0...v0.37.0
[0.36.0]: https://github.com/isarmg/dufs-ram/compare/v0.35.0...v0.36.0
[0.35.0]: https://github.com/isarmg/dufs-ram/compare/v0.34.2...v0.35.0
[0.34.2]: https://github.com/isarmg/dufs-ram/compare/v0.34.1...v0.34.2
[0.34.1]: https://github.com/isarmg/dufs-ram/compare/v0.34.0...v0.34.1
[0.34.0]: https://github.com/isarmg/dufs-ram/compare/v0.33.0...v0.34.0
[0.33.0]: https://github.com/isarmg/dufs-ram/compare/v0.32.0...v0.33.0
[0.32.0]: https://github.com/isarmg/dufs-ram/compare/v0.31.0...v0.32.0
[0.31.0]: https://github.com/isarmg/dufs-ram/compare/v0.30.0...v0.31.0
[0.30.0]: https://github.com/isarmg/dufs-ram/compare/v0.29.0...v0.30.0
[0.29.0]: https://github.com/isarmg/dufs-ram/compare/v0.28.0...v0.29.0
[0.28.0]: https://github.com/isarmg/dufs-ram/compare/v0.27.0...v0.28.0
[0.27.0]: https://github.com/isarmg/dufs-ram/compare/v0.26.0...v0.27.0
[0.26.0]: https://github.com/isarmg/dufs-ram/compare/v0.25.0...v0.26.0
[0.25.0]: https://github.com/isarmg/dufs-ram/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/isarmg/dufs-ram/compare/v0.23.1...v0.24.0
[0.23.1]: https://github.com/isarmg/dufs-ram/compare/v0.23.0...v0.23.1
[0.23.0]: https://github.com/isarmg/dufs-ram/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/isarmg/dufs-ram/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/isarmg/dufs-ram/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/isarmg/dufs-ram/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/isarmg/dufs-ram/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/isarmg/dufs-ram/compare/v0.17.1...v0.18.0
[0.17.1]: https://github.com/isarmg/dufs-ram/compare/v0.17.0...v0.17.1
[0.17.0]: https://github.com/isarmg/dufs-ram/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/isarmg/dufs-ram/compare/v0.15.1...v0.16.0
[0.15.1]: https://github.com/isarmg/dufs-ram/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/isarmg/dufs-ram/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/isarmg/dufs-ram/compare/v0.13.2...v0.14.0
[0.13.2]: https://github.com/isarmg/dufs-ram/compare/v0.13.1...v0.13.2
[0.13.1]: https://github.com/isarmg/dufs-ram/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/isarmg/dufs-ram/compare/v0.12.1...v0.13.0
[0.12.1]: https://github.com/isarmg/dufs-ram/compare/v0.11.0...v0.12.1
[0.11.0]: https://github.com/isarmg/dufs-ram/compare/v0.10.1...v0.11.0
[0.10.1]: https://github.com/isarmg/dufs-ram/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/isarmg/dufs-ram/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/isarmg/dufs-ram/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/isarmg/dufs-ram/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/isarmg/dufs-ram/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/isarmg/dufs-ram/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/isarmg/dufs-ram/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/isarmg/dufs-ram/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/isarmg/dufs-ram/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/isarmg/dufs-ram/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/isarmg/dufs-ram/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/isarmg/dufs-ram/tree/v0.1.0
