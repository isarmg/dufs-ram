# 项目完整功能与取舍清单

本文以当前工作树（Cargo 版本 `0.50.1`）的实际代码为准，盘点 Dufs 当前所有对外可见、可配置，以及会显著影响安全性、正确性、性能和可维护性的功能。普通辅助函数和测试夹具不单独作为“功能”列出；最终发布身份必须以制品内 `dufs --version` 的完整 Git SHA 为准。

本文的用途是帮助判断后续应该保留、简化还是删除哪些能力。它不是删除计划；没有得到明确选择前，本文不会改变任何现有功能。

逐项开发决策请先查阅[规范化开发者决策矩阵](feature-decision-matrix.md)。矩阵为每项能力强制提供唯一 ID、代码锚点、规定分类、复杂度、删除后果和验证边界；本文继续承担完整协议语义与取舍背景。

## 1. 如何阅读本清单

### 1.1 建议级别

| 级别 | 含义 |
| --- | --- |
| 核心 | 直接构成“通过现代桌面浏览器完整管理文件”的能力，删除后项目目标会改变 |
| 保障 | 用户不一定直接看到，但负责认证、根目录隔离、并发正确性、真正落盘或资源保护，不应作为普通功能单独删除 |
| 建议保留 | 对个人多设备使用价值明显，但在接受功能损失后可以删除 |
| 可选 | 只在特定部署或使用习惯下有价值，可以优先判断是否需要 |
| 开发运维 | 不属于浏览器文件管理操作，但用于构建、诊断、测试或交付 |

### 1.2 删除复杂度

| 复杂度 | 含义 |
| --- | --- |
| 低 | 主要是一个页面入口、一个参数或很小的独立路由 |
| 中 | 同时涉及前端、路由、配置或测试，需要成组清理 |
| 高 | 涉及协议、后台维护、持久化或多个共享模块，不能只删一个按钮 |

边界和明确不提供项仍会在“验证与边界”或“删除后果”中记录，但它们不是第六种分类。

### 1.3 当前没有“角色或能力开关”

当前唯一角色是 Foundation `admin`。可配置多个 canonical 管理员 username，但任一管理员登录后都拥有共享根目录的全部能力；上传、单文件下载、搜索、移动和删除等功能没有按管理员区分，也没有运行时禁用开关。

`Cargo.toml` 当前也没有可选 feature 组合；发布二进制会编译进全部现有模块。`--log-format=''` 或某个较小预算只能改变具体运行行为，不能视为已经从程序中移除对应能力。

因此，若决定删除某项能力，应同时清理：

- 浏览器入口和前端调用；
- HTTP 路由与协议；
- 命令行和 YAML 配置；
- 后台维护与共享状态；
- 直接依赖；
- Rust、浏览器和文档测试。

仅隐藏按钮不能视为已经删除功能，因为已认证客户端仍可能直接调用后端路由。

### 1.4 覆盖范围与编号规则

本清单按当前工作树的下列闭包交叉核对，而不是只从 README 摘录功能：

- `src/args.rs` 中的全部子命令、CLI 参数、YAML 字段、默认值和组合校验；
- `Server::call`、私有登录/资源路由和通用文件方法分派中的全部 HTTP 入口，以及代码实际使用的状态码；
- 目录页和登录页 HTML/CSS/JavaScript 中的全部用户入口、传输方式和界面边界；
- 全部生产 Rust 模块，以及 `Cargo.toml` 的 30 个生产直接依赖；
- systemd/nginx/YAML 部署样例、质量门禁、本地发布、许可证、安全策略和运维材料；
- 已删除的高价值相邻能力、已知不足和可选精简项。

测试夹具和普通内部辅助函数只有在形成用户可见行为、资源上限、安全保证或交付保证时才单列。ID 是用于文档间引用的稳定标签；缺号表示对应能力已经退役并由第 7 节或第 16 节记录，不代表存在未盘点行，也不会为了连续编号而重排后续 ID。C-11、C-19 至 C-21、C-23、Z-01 至 Z-08、X-01 和 X-14 属于已经删除的目录归档能力；C-06/X-08、C-07/X-10 和 X-17 分别对应已经移除的 URL path-prefix、用户隐藏规则和 HTTP/2 回源能力。

## 2. 总体定位与运行边界

### 2.0 跨章节开发者决策索引

下表补足后续逐功能表的“实现入口、依赖和最低验证”信息，并确保配置、安全、协议、前端、持久化、部署和明确不提供的能力都能从一个索引定位。分类与复杂度使用第 1 节定义；删除时仍须阅读对应详细编号，不能把本索引当作隐藏按钮清单。

| ID | 功能闭包 | 实现/主要依赖 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证 |
|---|---|---|---|---|---|---|
| IDX-01 | 单共享根浏览与面包屑导航 | listing route、`clients/web/modules/listing` | 核心 | 高 | 无法查看目录和进入子目录 | 根/空目录/特殊字符/权限 |
| IDX-02 | 单文件、Range、条件下载 | file response、ETag/metadata | 核心 | 高 | 文件无法可靠下载或续传 | GET/HEAD/Range/If-* 矩阵 |
| IDX-03 | 新建目录 | MKCOL/operation、路径租约、Web | 核心 | 中 | 只能上传到既有目录 | 冲突、权限、深层路径 |
| IDX-04 | 重命名与移动 | operation protocol、renameat、revision | 核心 | 高 | 无法整理文件；只剩上传/下载 | 同/跨目录、覆盖、竞态 |
| IDX-05 | 删除和 durable purge | DELETE、purge queue、state SQLite | 核心 | 高 | 无法回收文件；直接简化会破坏崩溃语义 | 文件/目录、重启、故障注入 |
| IDX-06 | 递归搜索和有界快照分页 | search cache、cursor、scan slots | 建议保留 | 高 | 大目录只能逐层浏览 | 预算、过期 cursor、变更隔离 |
| IDX-07 | 上传预检/批量选择 | preflight route、Web selection | 建议保留 | 高 | 冲突和路径超限只能在传输后发现 | missing/existing/special/budget |
| IDX-08 | 可续传上传检查点 | upload_sessions、PATCH、HEAD | 建议保留 | 高 | 断线后必须完整重传，终态歧义更难处理 | offset、重启、owner、满 stage |
| IDX-09 | 条件覆盖与显式确认 | target revision、awaiting-confirmation | 保障 | 高 | 陈旧页面可盲目覆盖并发修改 | target changed、confirm/discard |
| IDX-10 | 上传 durability 与磁盘水位 | stage、fsync、rename、space ledger | 保障 | 高 | 成功后可能丢数据或并发写满磁盘 | 断电点、不同 device、并发预算 |
| IDX-11 | Foundation 管理员 username 与 Argon2id 认证 | `sarmg-admin-auth`、strict YAML、Foundation auth API | 核心 | 高 | 无认证则共享根暴露；私有身份/角色协议会让跨项目合同漂移 | candidate/canonical username、当前 PHC、错误状态、登录限流 |
| IDX-12 | 内存 Session/Cookie/CSRF/严格同源保护 | `src/auth.rs`、`sarmg-admin-auth`、router middleware | 保障 | 高 | 浏览器登录态可被重放、解析歧义或跨站利用 | token shape、TTL、撤销、重复安全头、unsafe method |
| IDX-13 | 根目录 fd 隔离和 `openat2` | filesystem layer、Linux kernel | 保障 | 高 | 路径竞态可越出共享根 | symlink/mount/rename race |
| IDX-14 | 路径租约与 mutation ordering | lease manager、operation state | 保障 | 高 | 冲突操作可交错并产生不可解释结果 | 父子路径、超时、公平性 |
| IDX-15 | SQLite 当前状态权威 | schema、sessions/uploads/purges/search | 保障 | 高 | 重启后丢失终态和安全绑定 | schema identity、sidecar、corruption |
| IDX-16 | YAML/CLI 严格配置 | args.rs、`config/dufs.yaml.example` | 保障 | 中 | 拼错字段可能静默生效或读取不安全配置 | precedence、unknown、mode/owner |
| IDX-17 | 连接/请求/正文/并发预算 | server admission、timeouts | 保障 | 高 | 慢连接和大请求可耗尽进程资源 | 各上限、恢复、公平性 |
| IDX-18 | trusted proxy 与真实来源解析 | CIDR、XFF/XFP、logging/auth | 保障 | 高 | 来源和 HTTPS 判断可被伪造或全部失真 | trusted/untrusted、IPv4/IPv6 |
| IDX-19 | 访问日志与 operation 可观测性 | logging、operation ID/state | 开发运维 | 中 | 无法关联用户请求和后台终态 | 格式、敏感字段、file safety |
| IDX-20 | 编译期 Web 嵌入与摘要 URL | `clients/web`、assets registry、CSP | 建议保留 | 高 | 改成外部静态树后需协调版本和缓存 | 双向注册、hash、GET/HEAD/CSP |
| IDX-21 | 键盘/焦点/错误恢复 UI | Web modules、Playwright/a11y | 建议保留 | 中 | 基本 API 仍在，但桌面可用性和无障碍下降 | keyboard、dialog、live region |
| IDX-22 | doctor/self-test 与当前合同检查 | CLI、state/config/root verifier | 开发运维 | 高 | 部署问题只能运行后发现 | 健康、坏权限、错 schema、锁 |
| IDX-23 | systemd/nginx 部署基线 | deploy、proxy headers、limits | 开发运维 | 高 | 操作者需自行重建 TLS/隔离/启动语义 | verify/nginx-t/isolated smoke |
| IDX-24 | 可复现 release、SBOM、签名和全树 checksum | package script、workflow | 开发运维 | 高 | 来源、依赖和制品完整性不可独立证明 | clean/tag/SHA/tamper/reproducible |
| IDX-25 | Rust/JS/浏览器/部署安全门禁 | tests、Acorn/TS、CI | 开发运维 | 高 | 路径、协议或动态 JS 绕过可能进入发行 | 全门禁和内置负例 |
| IDX-26 | 中文学习、流程、功能和运维文档 | README、`docs/` | 开发运维 | 低 | 新开发者难以定位复杂状态机边界 | 本地链接、命令和代码引用 |
| IDX-27 | 只接受单一当前配置/API/管理员/Schema 合同 | strict parser/router/schema；无 alias/fallback | 保障 | 中 | 若加入第二解析路径，测试矩阵和攻击面随格式数量增长 | 非当前路径/字段/身份/Schema 一律拒绝且不修改状态 |
| IDX-28 | 不提供内置 TLS | 明确由 nginx/gateway 负责 | 可选 | 高 | 若删除外部网关前提则不能安全公网部署；若内置需承担证书生命周期 | 新 TLS 威胁模型/部署测试 |
| IDX-29 | 不提供移动 Web、在线归档和任意插件 | 明确产品边界 | 核心 | 高 | 新增任一项都会改变资源预算、UI 或供应链模型 | 独立设计与端到端验证 |
| IDX-30 | Linux AMD64 GNU 与 `openat2` 支持边界 | `build.rs`、`sarmg-server-target`、fd-relative filesystem | 保障 | 高 | 扩平台需重做核心安全证明，不能只删除编译断言 | 精确 target 通过、其他 target 负向编译门、运行时 `openat2` 探测 |

| ID | 当前特性 | 当前行为 | 删除或改变后的影响 | 级别 | 复杂度 |
| --- | --- | --- | --- | --- | --- |
| P-01 | 浏览器文件管理器 | 浏览、下载、上传、新建、移动、重命名、删除和搜索一个共享目录 | 删除其中核心 CRUD 后不再是完整文件管理器 | 核心 | 高 |
| P-02 | 单进程、单共享根部署模型 | 一个进程管理一个根目录；启动时长期持有根目录 fd 并尝试非阻塞独占 `flock`，同一根目录上的第二个 Dufs 实例会启动失败 | 多根目录仍需分别运行进程；删除共享根锁会让误启的第二实例越过进程内协调和磁盘预留 | 保障 | 高 |
| P-03 | 唯一 Linux AMD64 GNU 服务端 | `build.rs` 与 `sarmg-server-target` 只允许 `x86_64-unknown-linux-gnu`；其他 CPU、OS、ABI、指针宽度全部在编译期失败 | 删除守卫会让未证明的平台进入构建；扩平台必须重新审计 fd 相对文件系统、工具链、浏览器、部署和正式制品 | 保障 | 高 |
| P-04 | Linux `openat2` 必需 | 启动时探测；缺少 `openat2` 时失败关闭，不使用不安全降级 | 删除要求会破坏当前根目录安全模型 | 保障 | 高 |
| P-05 | 现代桌面浏览器 | 面向 Chromium、Edge、Firefox 桌面环境；不承诺手机 Web | 恢复移动端需要重新设计布局、交互和测试矩阵 | 核心 | 中 |
| P-06 | 单个可执行文件 | HTML、CSS、JavaScript 和图标编译进 Rust 可执行文件 | 改成独立前端会增加部署单元和版本协调 | 建议保留 | 中 |
| P-07 | 外部网关终止 HTTPS | Dufs 只提供明文 HTTP/TCP，默认绑定回环地址；是否仅在内网可达由显式 bind、防火墙/ACL 和网关部署共同保证。证书、TLS、HSTS 和公网策略由网关负责 | 若恢复内置 TLS，会重新引入证书配置和 TLS 依赖 | 保障 | 高 |
| P-08 | 可验证交付 | 版本 tag 流程等待同 tag/SHA 的全部质量门，构建带完整源码 SHA 的 GNU/Linux x86-64 便捷二进制，并生成只绑定当前版本与提交的发布说明；仓库以 Apache-2.0 许可，并提供 SBOM、第三方许可证清单、标准库 notice、校验和与签名的正式发布链 | 自动便捷二进制没有独立发布者签名；正式信任链仍需本地签名包和独立渠道固定的公钥 | 开发运维 | 中 |
| P-09 | current-only | 只接受文档所列当前参数、Foundation 管理员 API/身份和当前 SQLite identity；YAML 未知字段直接报错 | 增加 alias/fallback 会扩大分支和维护成本；历史数据处理属于独立仓库 | 保障 | 中 |

## 3. 启动、命令行和 YAML 配置

运行时配置只来自命令行和可选 YAML。命令行值覆盖 YAML；服务进程不读取 `DUFS_*` 配置环境变量。构建阶段仅允许用 `DUFS_BUILD_GIT_SHA` 注入可验证的源码版本；YAML 使用严格未知字段检查。

| ID | 参数或入口 | 默认值/限制 | 功能与取舍影响 | 级别 |
| --- | --- | --- | --- | --- |
| C-01 | `[serve-path]` / `serve-path` | 当前目录；相对路径按进程 cwd 而不是 YAML 所在目录解析，随后 canonicalize；启动时必须已存在且为目录 | 决定唯一共享根，不能删除 | 核心 |
| C-02 | `dufs hash-password` | 交互输入和确认密码 | 生成符合固定策略的 Argon2id PHC；外部工具必须精确复现当前参数，删除收益有限 | 建议保留 |
| C-03 | `-c, --config` | 不指定则只用命令行；文件最多 1 MiB；路径及可解析别名必须在共享根外 | 以 `O_NOFOLLOW|O_NONBLOCK` 单次打开严格 YAML；只接受 root/euid 所有、精确 `0400/0440/0600/0640`、单硬链接且无扩展 POSIX access ACL 的普通文件，组读要求 gid 等于 egid。同一 fd 在 ACL 探测和读取前后复核完整身份、安全元数据、大小及 mtime/ctime；与日志、固定状态库及热 sidecar 同时比较规范目录项和已存在 dev/inode 身份；若始终使用固定 systemd 命令行，可考虑删除 YAML | 可选 |
| C-04 | `-b, --bind` / `bind` | `127.0.0.1`；可重复或逗号分隔；只接受 IP；最终列表不能为空 | 默认不暴露到外部网卡；跨主机网关必须显式绑定内网 IP，若始终单地址可简化 | 可选 |
| C-05 | `-p, --port` / `port` | `5000`；允许 `0` 供测试动态分配 | 决定内网 TCP 端口，必须保留某种端口配置 | 核心 |
| C-06 | `--trusted-proxy` / `trusted-proxies` | 默认空；IP/CIDR，可重复或逗号分隔；最多 128 个，拒绝单个或组合覆盖完整 IPv4/IPv6 地址空间 | 仅当直连 peer 匹配时接受单值 XFF/XFP；HTTPS 网关必须显式配置，列表本身不是代理身份认证 | 保障 |
| C-08 | YAML `auth` | 至少一个、最多 1024 个 `canonical-admin-username:<当前 Argon2id PHC>`；无 role 字段；配置文件须满足严格属主、权限和文件身份校验 | 配置唯一管理员角色；CLI 不定义账号参数，未声明选项统一拒绝；删除会使服务无法安全启动 | 核心 |
| C-09 | `--log-format` / `log-format` | `$time_iso8601 $log_level - $remote_addr "$request" $status operation_id=$operation_id operation_state=$operation_state` | 自定义访问日志；空字符串关闭访问日志 | 可选 |
| C-10 | `--log-file` / `log-file` | 不指定时全部日志输出到 stderr，stdout 只输出监听地址；路径及可解析别名必须在共享根外；已有文件必须是当前服务用户拥有、精确 `0600` 的单链接普通文件 | 与配置、`state.sqlite3` 及 `-journal/-wal/-shm` 比较规范目录项和已存在 dev/inode 身份；以 `O_NOFOLLOW|O_APPEND|O_NONBLOCK|O_CLOEXEC` 打开；新文件原子创建并固定 `0600`，已有文件权限不安全则保持不变并拒绝；仅使用 systemd/journald 时可以删除文件输出 | 可选 |
| C-12 | `--max-upload-size` | 100 GiB；允许设为 `0` | 单文件声明长度上限；`0` 表示只允许零字节上传，不是关闭限制 | 保障 |
| C-13 | `--upload-idle-timeout` | 60 秒；必须大于 0、最多 365 天且能由平台单调时钟表示 | 上传正文无进展时限 | 保障 |
| C-14 | `--upload-total-timeout` | 24 小时；不得为 `0`、小于空闲时限或超过 365 天，且必须能由平台单调时钟表示 | 每次 PUT/PATCH 在等待路径租约前建立绝对 deadline，覆盖租约等待、上传准备、正文、写入、flush、metadata 重放以及等待最终提交确认。受跟踪 task 的首次文件系统/上传状态 mutation 与 deadline 原子竞争：deadline 先赢则关闭边界、abort 并返回 `408 not-started + retry`；task 先越界后才以 `unknown + query_upload` 处理外层超时，进入不可取消的 rename/fsync 段后由后台安全收尾 | 保障 |
| C-15 | `--max-concurrent-uploads` | 4；必须大于 0 | 服务端上传并发上限 | 保障 |
| C-16 | `--min-free-space` | 1 GiB；允许设为 `0` | 上传暂存文件写入时保护的最低可用空间；`0` 会关闭保留水位 | 保障 |
| C-17 | `--max-connections` | 256；必须大于 0 | 所有监听器共享的活跃 TCP 连接上限 | 保障 |
| C-18 | `--max-search-entries` | 10000；必须为 1–100000 | 单次递归搜索最多检查的目录项；硬上限同时约束配置和运行时物化 | 保障 |
| C-22 | `--max-concurrent-searches` | 2；必须大于 0 | 普通目录或递归搜索的首个快照扫描共用并发槽；后续 cursor 页只读取缓存快照，不再占扫描槽 | 保障 |
| C-24 | `--request-timeout` | 300 秒；必须大于 0、最多 365 天且能由平台单调时钟表示 | 普通请求处理和响应头生成时限；不限制已开始的文件或 Range 正文总时长，但每个源分块读取及套接字写入分别有 30 秒 idle deadline | 保障 |
| C-25 | `-h/--help`、`-V/--version` | Clap 内置；版本同时显示构建源码 Git SHA，无法取得时显示 `unknown` | 基础命令行自描述和制品来源追踪，删除收益极低 | 开发运维 |
| C-26 | `--state-dir` / `state-dir` | 必填；固定使用私有 `0700` 目录内的 `state.sqlite3`，目录须由服务账号所有、非符号链接、与共享根分离，文件绑定共享根 dev/inode；数据库及 `-journal/-wal/-shm` 不得与配置/日志共享目录项或对象身份；只初始化空库并只接受五列 `product_metadata` 标识的当前应用版本/schema revision/统一指纹，不存在进程内数据库模式 | 文件型 store 在同一当前 schema 中持久化 operation 幂等结果、upload session 与 purge outbox；旧、无标记或漂移数据库在只读预检中零修改拒绝，未来稳定版本的格式转换只能由停服后的 `sarmg-upgrade` 精确迁移边负责 | 保障 |

所有信号量型配置还会拒绝超过 Tokio 最大 permit 数的值。上传时限互相矛盾、超过一年或平台单调时钟可表示范围的极端时限、零并发和零遍历上限都会阻止启动。严格 YAML 会拒绝任何未定义字段。

其他配置语义：

- YAML 的 `bind` 与 `trusted-proxies` 接受单个字符串或字符串列表，`bind` 列表非空，`auth` 使用列表；
- 命令行显式提供 `bind` 或 `trusted-proxy` 时会整体替换 YAML 中的对应值，不是追加；账号只接受受保护 YAML 的 `auth`；
- 大小和时限使用裸字节、裸秒数，不接受 `10GiB`、`5m` 等单位文本；
- 服务固定挂载在独立主机名的根路径 `/`，不支持 URL 子路径部署；
- 配置账号、路径、监听地址或其他启动项发生变化后必须重启，没有运行时配置管理 API。

## 4. 管理员、登录、会话和请求安全

本节每一行都是可独立审查的当前能力。分类只使用“核心/保障/可选/建议保留/开发运维”，复杂度同时计算后端、页面、配置、测试和运维的删除成本。

| ID | 功能/当前实现 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 验证与边界 |
| --- | --- | --- | --- | --- | --- | --- |
| A-01 | 强制管理员认证；至少一个管理员 username 是启动条件，除登录页、登录 API、公开 liveness 和内容寻址登录资产外，文件、页面、内部 API 与 readiness 都要求会话 | `src/args.rs::validate`、`src/server/router/dispatch.rs`、`src/auth.rs` | 核心 | 高 | 共享根会变成匿名文件服务，产品安全边界改变 | 无 `auth` 启动失败；匿名文件/API 为 401；HTML 导航按严格 `Accept` 转登录页 |
| A-02 | 唯一角色 `admin`；可配置多个管理员 username，但没有 role 字段、普通用户、只读角色、路径 ACL 或能力开关 | `sarmg-contracts::AdministratorSession`、`AuthConfig`、YAML `auth` | 核心 | 中 | 增加角色会扩展授权矩阵；删除多管理员会失去独立身份、审计和会话撤销域 | session JSON 必须 `role=admin`；全 CRUD 对每个已认证管理员一致；未知 role 不存在解析入口 |
| A-03 | Foundation current username 合同；配置只接受 3～64 个小写 ASCII 字节、首尾 alnum、字符 `[a-z0-9._-]` 的 canonical username，`@` 明确禁止、相邻分隔符允许；重复 username 拒绝 | `sarmg-admin-auth::{require_canonical_administrator_username,normalize_administrator_username}`、`src/auth.rs::AuthConfig::new` | 保障 | 中 | 自行定义身份会令跨项目合同漂移，并重新引入大小写、Unicode、trim 或字符集歧义 | 登录 candidate 限 1～64 bytes 且每字节 `0x20`～`0x7e`，ASCII trim/lowercase 后再校验；配置中的大写、空白、首尾分隔符、`@`、控制字符、Unicode、超限全部拒绝 |
| A-04 | 唯一当前 Argon2id PHC：v19、m=19456、t=2、p=1、salt 16 bytes、output 32 bytes | `sarmg-admin-auth::{hash_password,require_current_password_hash,verify_password}`、`dufs hash-password` | 保障 | 中 | 放宽后会承担多代哈希兼容分支；去掉哈希会暴露密码 | 当前参数正例；任一算法/版本/参数/salt/output 漂移启动失败；错误密码固定 401 |
| A-05 | 密码合同为 12–1024 UTF-8 bytes 且不含 ASCII control；CLI、服务端 JSON 和浏览器提示共用相同上下限 | `sarmg-admin-auth::validate_password`、`src/server/session.rs`、`clients/web/login.js` | 保障 | 低 | 会生成不能登录的哈希，或允许低成本/超大输入扩大攻击面 | 11/12/1024/1025 字节边界、多字节字符、NUL/DEL；策略无效请求为 400，正确形状错误凭据为 401 |
| A-06 | Foundation 三个固定认证端点：`POST /api/v2/auth/login`、`GET /api/v2/auth/session`、`POST /api/v2/auth/logout`；没有旧表单 POST 或路径 alias | `sarmg-contracts::{ADMIN_LOGIN_PATH,ADMIN_SESSION_PATH,ADMIN_LOGOUT_PATH}`、router dispatch | 核心 | 中 | 页面无法建立、探测或结束会话；添加 alias 会违反 current-only | exact method/path 正例；尾斜杠、重复斜杠、其他 method、未知字段和历史路径负例 |
| A-07 | 严格登录 JSON 与统一 session/error JSON；请求只能有 `username/password`，成功 session 只能有 authenticated/user_id/username/role/csrf_token，错误使用 Foundation `ErrorEnvelope` | `sarmg-contracts::{AdministratorLoginRequest,AdministratorSession,ErrorEnvelope}`、`src/server/session.rs`、`clients/web/login.js` | 保障 | 中 | 调用方会解析自然语言或形成各项目独有 wire shape | 多余/缺失/错类型字段、非规范 session、错误 code/retryable/detail、Content-Type 与正文上限；不接受任何额外或旧身份字段 |
| A-08 | 英文登录页由原生 HTML/JS 提交 JSON Fetch；客户端先校验 username、密码字节数和返回 session，不采用 PRG 或服务端一次性错误 token | `clients/web/login.html`、`clients/web/login.js`、`send_login_page_for_get` | 核心 | 中 | 没有首方登录入口；改回表单协议会偏离 Foundation | 键盘提交、无效字段聚焦、400/401/429 文案、非法 session、成功 replace `/` |
| A-09 | 登录正文 admission：16 KiB、10 秒、全局 32/每 IP 4 个并发读取许可 | `src/server/session.rs::{LOGIN_BODY_LIMIT,LOGIN_BODY_TIMEOUT,LoginBodyAdmission}` | 保障 | 中 | 慢/大登录正文可占用连接、内存和任务 | 正文恰好边界、超限 413、超时 408、每 IP/全局耗尽 429、permit drop 后恢复 |
| A-10 | 登录 token bucket 与账号退避：全局/IP 请求桶、来源 IP+canonical username 摘要失败状态、最多两个 Argon2 blocking 槽、1–60 秒退避 | `src/server/login_rate_limit.rs`、`AdmissionControl::login_slots`、`run_with_login_slot` | 保障 | 高 | 密码哈希可被 CPU DoS；简单全账号锁会被用于定向拒绝服务 | burst/refill、第五次失败、成功仅清对应键、不同 IP 隔离、取消后槽不提前释放、429 Retry-After |
| A-11 | 256-bit 随机、规范 43 字符 base64url 无 padding 的 session 与 CSRF token；服务端仅用 SHA-256 digest 定位/比较 token | `sarmg-admin-auth::{random_token,is_token_shape,token_hash,token_matches_hash}`、`src/auth.rs` | 保障 | 高 | 可预测或明文存储 token 会扩大盗用与内存泄露后果 | 长度/最后字符 canonical bits、重复生成拒绝、任一畸形 token 不查询成功、常量时间比较 |
| A-12 | 内存 session 生命周期：空闲 30 分钟、绝对 12 小时，`CLOCK_BOOTTIME` 包含休眠；重启全部失效 | `src/auth.rs::{SessionClock,SessionRecord,SESSION_*_TIMEOUT}` | 保障 | 中 | 无过期会长期授权；持久化 session 会增加磁盘 Secret/撤销迁移负担 | idle/absolute 边界、休眠时钟、访问续 idle 不续 absolute、重启重新登录 |
| A-13 | session 容量公平：每管理员 32、全局 1024；达到局部或全局上限时优先淘汰该管理员最久未活动记录，否则全局 LRU | `src/auth.rs::SessionStore::insert` | 保障 | 中 | 无上限导致内存增长；纯全局淘汰允许单一管理员挤掉其他人 | 第 33/1025 个 session、同时间 tie、过期先清理、多管理员公平性 |
| A-14 | 登录成功轮换已有 Cookie 对应 session；失败登录不撤销原会话 | `handle_login`、`AccessControl::create_session` | 保障 | 中 | 不轮换增加 session fixation 风险；失败即注销可被跨请求诱导下线 | 有/无旧 Cookie、失败保持、成功撤销旧 token、新旧并发结果 |
| A-15 | `__Host-dufs-session` Cookie：Secure、HttpOnly、SameSite=Strict、Path=/、无 Domain；注销用 Max-Age=0 清理 | `src/auth.rs::{COOKIE_ATTRIBUTES,clear_session_cookie}` | 保障 | 低 | 弱化属性会扩大脚本读取、跨站发送或子域注入风险 | Set-Cookie exact 属性、HTTPS 浏览器、注销清除；同主机名其他应用 Cookie 冲突属于部署边界 |
| A-16 | Cookie 解析失败关闭；收集全部 Cookie field lines，重复同名 session、逗号/控制字符/非规范 token 均不接受 | `sarmg-admin-auth::parse_cookie_value`、`session_token_from_headers` | 保障 | 中 | 选择第一个/最后一个会产生代理与服务端解析歧义 | 多 field line、重复 name、空值、无关 cookie、非法字节、43 字符 token |
| A-17 | Foundation 严格同源：安全相关 header 必须唯一、规范且不可逗号合并；effective Host 合并全部 Host field line 与 URI authority；生产仅 HTTPS，HTTP 仅 loopback development；`Sec-Fetch-Site` 必须 same-origin | `sarmg-admin-auth::require_administrator_same_origin`、`request_source_is_same_origin`、`effective_host_values` | 保障 | 高 | 登录或写请求可能被跨站触发，或 Host/authority 歧义绕过 | Origin/Host/authority/Sec-Fetch-Site 缺失、重复、大小写/端口规范、userinfo/path/query、cross-site、可信代理 XFP |
| A-18 | 每会话 CSRF；所有已认证 POST/PUT/PATCH/DELETE（包括 logout）要求唯一 `X-CSRF-Token` 并与 session digest 常量时间比较 | `AccessControl::verify_csrf_header_values`、`Server::csrf_is_valid`、dispatcher | 保障 | 高 | 浏览器 Cookie 会被自动携带，恶意同浏览器站点可借会话写文件 | 缺失/重复/逗号/畸形/其他 session token；GET/HEAD 不要求；登录由同源保护 |
| A-19 | 认证响应 `private, no-store` 与 CSP/nosniff/frame/referrer/Permissions-Policy 安全头 | `add_private_security_headers`、assets/login response tests | 保障 | 中 | 私有文件或认证错误可能进入共享缓存，页面注入面扩大 | 登录/session/error/file/Range/API/HEAD 响应头；CSP hash 与内联登录脚本字节一致 |
| A-20 | 只有验证成功的 canonical 管理员 username 进入 `$remote_user`；Cookie、CSRF、Authorization 和 Proxy-Authorization 日志变量始终脱敏 | `RequestContext`、`http_logger.rs`、router dispatch | 保障 | 中 | 认证失败身份会污染审计，或 Secret 进入日志 | 成功/失败/匿名日志、混合大小写 header、控制字符、重复变量与 16 KiB 日志上限 |

仍应由网关负责的认证外围能力：

- 在应用内限流之外，独立按可信的真实客户端 IP 限制登录频率；
- 把 HTTP 强制跳转到 HTTPS；
- 在确认域名只提供 HTTPS 后配置 HSTS；
- 用防火墙或私网阻止客户端绕过网关直连后端。

管理员 username 与认证 wire 类型的唯一规则来自 Foundation；`sarmg-admin-auth`、`sarmg-contracts`、
`sarmg-schema-identity`、`sarmg-server-target` 均精确固定为 `=0.3.1` 与 Git rev
`7c6a210cd5fc8bf987e0f50fccee69b7c58cbdf0`，不接受 workspace sibling、Cargo path dependency、可变 branch
或本地副本。Dufs 只负责把受保护 YAML 映射到该规则及把认证后的稳定管理员 ID 用于本地
operation/upload/purge owner domain。项目没有网页端新增管理员、删除管理员、改密码或改角色功能；配置变化
必须重启。未知但形状合法的 canonical username 仍执行当前成本的 Argon2 校验，以缩小按响应时间枚举已配置
username 的差异。这里的管理员 username 是管理面身份；`SessionInfo.user`、operation owner 等 data-plane 名称
保留其“文件操作归属键”语义，不重新引入普通用户或角色。

## 5. 浏览器目录界面

| ID | 当前特性 | 详细行为 | 删除后的用户影响 | 级别 | 复杂度 |
| --- | --- | --- | --- | --- | --- |
| B-01 | 英文目录页面 | 页面骨架由 Rust 返回，数据通过受认证 list API 加载 | 删除后没有浏览器管理界面 | 核心 | 高 |
| B-02 | 面包屑导航 | 从共享根逐级进入当前目录 | 深层目录只能依靠地址栏或返回操作 | 建议保留 | 低 |
| B-03 | 当前管理员与注销 | 右上角显示已认证管理员 username 并提供 Foundation 注销按钮 | 不影响文件 CRUD，但会降低会话可见性 | 建议保留 | 低 |
| B-04 | 文件/目录表格 | 显示名称、修改时间、大小和操作；目录不统计子目录数量 | 删除表格即失去核心浏览能力 | 核心 | 高 |
| B-05 | 类型区分 | 区分目录、根内符号链接目录、文件和根内符号链接文件 | 若统一显示，用户难以判断导航和下载行为 | 建议保留 | 低 |
| B-06 | 排序 | 按名称、修改时间、大小升序或降序；使用确定性名称次序和稳定归并排序。合并、索引构造和最终置换的每个有界步骤都会检查停机标志与 deadline。内部 list API 的未知 `sort`/`order` 分别回退到 `name`/`asc`，空 `q` 回退为普通目录列表 | 删除可减少少量前后端代码，但大目录查找更困难；改回不可中断标准排序会让最坏规模请求越过取消或时限 | 建议保留 | 中 |
| B-07 | 分页与加载更多 | 默认 200 项；API 可请求 1–500 项；第一页有界扫描并一次性排序完整结果集，后续页只从它切片，不会每页重扫和重排。直接列表、递归搜索都最多检查 100000 项；递归工作集受 1024 层/32 MiB 限制，结果向量另受 32 MiB 限制 | 删除分页会重新引入大响应体；删除结果缓存会恢复大目录每页重复工作的放大效应 | 保障 | 高 |
| B-08 | 有界、账号隔离的分页结果 | cursor 带随机结果 ID、offset 和抗篡改 tag，并绑定认证账号摘要、路径、设备号、inode、纳秒 mtime/ctime、排序、查询和 limit；进程内结果绝对存活 120 秒，总计最多 32 个/64 MiB、每账号最多 8 个/32 MiB。游标编码/版本无效、跨账号复用或其他请求绑定不匹配返回 `400`；tag 不匹配、结果未知/过期/淘汰及目录变化返回 `409`。直接列表构造前后复核当前目录；递归搜索在访问前和整轮完成后复核所有访问目录。构造成功后的各页来自同一不可变内存结果，但这些复核不是文件系统原子快照 | 删除账号绑定会形成越权读取缓存结果的风险；删除公平额度会让一个账号驱逐其他账号全部游标；取消容量/TTL 会造成内存无界增长；真正强一致需只读存储快照或等价版本化源 | 保障 | 中 |
| B-09 | 递归搜索 | 从当前目录递归、按文件或目录的名称进行不区分大小写的包含匹配；服务端最多接受 128 个 Unicode scalar values，首方输入框的原生 `maxlength=128` 按 UTF-16 code units 计数，因此 astral 字符的可输入数量可能更少 | 删除后只能逐级浏览；可移除递归搜索配置和部分遍历代码 | 建议保留 | 中 |
| B-10 | 空目录与不存在目录状态 | 空目录显示空态；访问以 `/` 结尾的不存在路径时显示 `Uploading files will create this folder automatically` | 删除不存在目录页面后，深层上传必须先逐级建目录 | 建议保留 | 中 |
| B-11 | 加载错误与重试 | 不带 cursor 的首屏请求遇到完整的 `409 directory_changed/refresh_target` 会自动重放同一 GET 一次；连续冲突、后续页及其他目录加载失败会显示英文原因和 `Retry`，认证失效会回到登录流程 | 删除会让单次目录变化和其他临时失败都只能刷新整页 | 建议保留 | 低 |
| B-12 | 安全 DOM 构建 | 动态名称和错误只经 `textContent`、属性 API、`URL`、`DocumentFragment` 进入页面 | 改回动态 HTML 会重新引入 DOM 注入风险 | 保障 | 中 |
| B-13 | 键盘、缩放与辅助功能 | 原生按钮、可见焦点、英文 `aria-label`、`aria-live` 状态和焦点转移；新建后的命名与 Rename 使用名称列中的单一行内编辑器，输入具有可访问名称、`aria-invalid` 和 alert 错误，Enter 提交、Escape 取消且不会删除已创建的默认项。Move、覆盖、删除及操作错误使用有可访问名称的原生 `<dialog>`。页面在不超过 537 CSS 像素时把每个文件列表行回流成两行网格，名称和操作位于首行，修改时间和大小移到第二行且仍可见；工具栏和长文本同时回流。Playwright 在 320 CSS 像素（相当于 1280 px 桌面 400% 缩放）验证无页面级横向滚动，在 `forced-colors: active` 下检查控件、焦点、行内编辑器和对话框边界，并以固定 `@axe-core/playwright` 扫描登录页、文件页、编辑器和打开的操作对话框 | 删除不影响鼠标基本操作，但降低可访问性、桌面高倍缩放及高对比模式可用性和自动化稳定性 | 建议保留 | 低 |
| B-14 | 深色外观适配 | 登录页和目录页响应系统浅色/深色偏好 | 只影响外观，可删除 | 可选 | 低 |
| B-15 | 拖放导航保护 | 拖入文件时阻止浏览器直接打开本地文件，但不会开始上传 | 删除后误拖文件可能离开当前页面；它不是拖放上传 | 建议保留 | 低 |
| B-16 | 页面内操作反馈 | 新建不显示命名对话框，Rename 也不再弹窗；两者共用列表行内编辑状态。单个原生 `<dialog>` 继续复用于 Move、覆盖和删除确认及操作错误，内容只经 `textContent` 更新。模态打开后按场景聚焦输入或主操作，原生 Tab 范围留在对话框内；Enter 提交表单，Cancel 或 Escape 关闭，关闭后显式恢复对应触发控件焦点。并发到达的错误提示会排队，不覆盖当前对话框 | 改回 `prompt/confirm/alert` 可删除少量 HTML/CSS/状态代码，但会失去一致外观、可测试语义和显式焦点恢复 | 建议保留 | 中 |

### 5.1 列表和文件名边界

- 浏览器协议的 URI 解码要求 UTF-8，因此整体无法寻址非 UTF-8 Linux 文件名；列表或搜索遇到此类名称时整个操作失败，不返回不完整成功结果。
- 直接列表在扫描前后复核当前目录；搜索在访问每个目录前复核捕获快照，并在完成后再次复核所有访问过的目录。目录项消失、类型/身份变化或可观察目录元数据变化会返回可重试 `409`。
- 上述复核只检测遍历期间可观察到的变化，不是文件系统原子快照：检查间发生又恢复的变化、未改变目录元数据的子文件原地内容/权限变化及最终复核后的变化仍可能不可见。需要强一致时必须从只读存储快照或等价版本化源遍历。
- 内部上传暂存、状态和删除回收名称不会显示，也不能通过普通浏览器路径访问。
- 普通浏览、上传和文件操作的大小写语义服从底层 Linux 文件系统；项目不探测 ext4 casefold 或宿主侧大小写不敏感目录。

## 6. 文件下载

| ID | 当前特性 | 详细行为 | 删除或简化后的影响 | 级别 | 复杂度 |
| --- | --- | --- | --- | --- | --- |
| D-01 | 文件下载 | `GET /path` 从根 fd 打开文件并流式返回 | 删除后不能从浏览器取得文件 | 核心 | 高 |
| D-02 | 强制附件 | 所有普通文件都使用 `Content-Disposition: attachment`，不在线预览或编辑 | 改为 inline 会恢复浏览器预览面和相关安全判断 | 保障 | 低 |
| D-03 | 安全下载文件名 | 固定安全 ASCII 回退名加 RFC 6266/8187 UTF-8 `filename*`；过滤控制字符 | 删除会使特殊字符文件名在响应头中产生歧义 | 保障 | 低 |
| D-04 | `HEAD` | 返回与完整 GET 一致的 metadata 和下载头，但不发送正文；按 HTTP 语义忽略 `Range`，因此不会返回 `206` 或 `416` | 浏览器主流程很少直接使用；标准客户端和诊断能力会减少 | 可选 | 中 |
| D-05 | 单段 Range | GET 只接受恰好一个 `Range` 请求头中的一个字节范围，单位按 ASCII 大小写不敏感；合法返回 `206`，超出文件尾的 end 会截断，过大的 suffix 会把完整表示作为 `206` 返回，无效、溢出或不可满足的 `bytes` 范围返回 `416`，不支持的范围单位被忽略并返回完整 `200` | 删除后大文件断点下载和浏览器下载恢复能力下降 | 建议保留 | 中 |
| D-06 | 拒绝多段或重复 Range | 逗号分隔的多段范围和重复 `Range` 请求头固定返回 `416`，不会挑选其中一个成员执行 | 已是有意精简；恢复会增加歧义处理或 multipart 响应复杂度 | 保障 | 高 |
| D-07 | 条件请求 | 支持 `If-Match`、`If-None-Match`、`If-Modified-Since`、`If-Unmodified-Since` | 只使用点击下载时价值有限，可作为协议级精简候选 | 可选 | 中 |
| D-08 | 弱 ETag 与 Last-Modified | ETag 包含 dev、inode、长度、纳秒 mtime/ctime；`If-Range` 存在时返回完整文件 | 防止快速原子替换后拼接错误版本 | 保障 | 中 |
| D-09 | 扩展名 MIME | 使用 `mime_guess` 的扩展名映射；未知名称固定为 `application/octet-stream`，不读取文件内容或猜测字符集 | 统一 octet-stream 还能再简化少量逻辑，但会丢失下载管理器可用的类型提示 | 可选 | 低 |
| D-10 | 同一文件句柄、类型与长度一致性 | 数据句柄经根 fd 以 `O_NONBLOCK` 打开，并在同一 fd 上确认普通文件；metadata 和正文都来自该句柄。完整 GET 与 Range 只发送打开时的 metadata 长度，随后原地追加同一 inode 也不会越过已声明正文 | 防止路由分类后被外部写者换成 FIFO 而阻塞，或使头与正文来自不同对象，也防止响应体超过 `Content-Length` | 保障 | 中 |
| D-11 | 无正文总时限、有读写空闲时限 | 响应头发出后的普通文件和 Range 传输不受 `request-timeout` 或最低速率限制，但每个源文件分块等待/读取和套接字写入分别有 30 秒 idle deadline；公网总时长/速率策略仍由网关补充 | 避免永久卡死的源文件或写端，同时不对持续有进展的慢速大文件设硬总时限 | 建议保留 | 中 |

## 7. 已移除的目录归档下载

目录 ZIP 已从当前产品中删除。浏览器不再显示目录下载入口，服务端不再规划、压缩或临时保存目录归档；目录仍可浏览和搜索，文件仍可逐个下载，多文件选择与文件夹上传也不受影响。需要整目录导出的部署应使用共享根之外的受控备份或归档工具。

目录请求始终使用普通 HTML 列表或搜索语义；未识别的查询参数不选择其他输出格式，也没有退役能力的专用兼容路由。

本次删除同时移除了原 Z-01 至 Z-08 的递归归档规划、跨平台条目命名空间校验、文件身份复核、私有临时归档、压缩级别、源/输出/条目预算、生成期并发槽、任务所有权以及 ZIP 专属测试。`async-deflate-zip` 与 `unicode-normalization` 不再是直接依赖；`tempfile` 仍由状态、存储及测试代码使用。明确损失是一键下载整个目录以及通过归档保留空目录，单文件下载协议不变。

## 8. 文件与文件夹上传

| ID | 当前特性 | 详细行为 | 删除或简化后的影响 | 级别 | 复杂度 |
| --- | --- | --- | --- | --- | --- |
| U-01 | 多文件选择上传 | 浏览器一次可选择多个文件，前端逐个排队上传；每批最多 512 个最终绝对逻辑路径，解码后的路径 UTF-8 合计最多 256 KiB，预检 JSON wire body 最多 2 MiB；进入预检/确认的文件与 pending DOM 行合计最多 512，终态历史只保留最近 200 行 | 删除后失去核心写入能力；删除数量、路径和预准入/DOM 上限会重新引入浏览器内存与可访问树无界增长 | 核心 | 高 |
| U-02 | 文件夹选择上传 | 使用桌面浏览器 `webkitdirectory`，保留相对路径 | 若只上传单文件可删除页面入口；空目录本来就不会创建 | 可选 | 低 |
| U-03 | 前端顺序队列 | 页面固定同时传 1 个任务；服务端默认允许 4 个，供多个页面/设备并发 | 改为页面并行可提速但增加浏览器和网络竞争 | 建议保留 | 中 |
| U-04 | 上传状态 | 分开展示等待、传输、提交确认、成功、已知失败和结果未知；传输中显示速度、百分比和预计剩余时间 | 删除不影响协议，但会明显降低大文件体验，也会把“可能已经提交”误导成可安全重试的普通失败 | 建议保留 | 低 |
| U-05 | 分阶段取消 | 正文传输期可由浏览器中止，服务端按收到的有效数据决定保留检查点或清理；进入提交确认后按钮改为 `Stop waiting`，只停止客户端等待并显示结果未知，不取消后台持久化提交 | 删除后只能关闭页面或连接；把提交期取消误认为服务端回滚会导致危险重试 | 建议保留 | 中 |
| U-06 | 离页提醒 | 队列或上传进行中时触发浏览器原生离页确认 | 删除可能因误刷新中断上传 | 建议保留 | 低 |
| U-07 | UUID 上传身份 | 每次重新选择文件生成新 UUID；不会用文件名、大小、mtime 猜测文件身份 | 删除会重新引入不同文件错误拼接风险 | 保障 | 高 |
| U-08 | 当前页面内安全重试 | 同一个 `File` 对象在已知可重试失败后先 HEAD 查询原 upload ID，再按 owner-scoped state 决定 PATCH 或新 ID。响应 `not-started` 只证明本次尝试在任何上传 mutation 前停止，仍必须先查询旧 ID；服务端 deadline 在原子 mutation boundary 前胜出时可明确给出 `408 not-started + retry`，而已发请求的客户端断线、提交等待超时、边界后服务端错误或明确 `unknown` 不允许盲重放 | 网络波动时不必总是从头上传，同时避免把“本次未启动”误读为“旧 ID 必定不存在”，或在结果不确定时重复覆盖 | 建议保留 | 高 |
| U-09 | 不跨刷新恢复 | 不使用 `localStorage`；刷新后必须重新选择并新建 ID | 已是安全取舍；恢复跨刷新需要可靠内容身份方案 | 保障 | 高 |
| U-10 | 预检与条件 PUT | 浏览器先向 `POST /__dufs__/api/upload/preflight` 提交最终绝对逻辑路径，服务按原顺序返回存在性、可替换提示和 revision。根 fd 相对路径最多 4095 个 UTF-8 字节，每个组件最多 255 字节；超过 Linux `openat2` 可寻址边界的值在任何文件系统探测前以 `400 invalid_path` 拒绝。无冲突零确认；只有已存在且可替换的文件进入覆盖/跳过/取消对话框。PUT 的 upload ID 和总长度必须各自精确出现一次；缺少 `X-Dufs-Upload-Overwrite` 或值为 `false` 使用原子 no-replace，`true` 必须携带 64 位小写十六进制 target revision。revision 绑定账号摘要、规范根内路径和完整 replacement identity，并在 rename 前复核；预检本身不提供原子锁。Missing 发布使用 `RENAME_NOREPLACE` 并在成功后核对目的名称与已打开 stage；Existing 覆盖是 identity 复核后执行普通 rename，不是对外部 writer 的目录项 CAS。目录、FIFO、Unix socket、设备等目标只返回不可替换提示或稳定拒绝，不会自动覆盖。fresh PUT 仍在创建 stage 前检查 upload/purge 路径义务；stage 在目标父目录下当前 `.dufs-upload-stages` 私有目录中初建为 `0600`，目录固定 `0700`，新文件发布后固定为 `0600`，覆盖 stage 则可能在发布前重放旧目标权限 | 预检减少无意义确认，no-replace 与条件复核防止当前 Dufs 的陈旧请求盲目覆盖；共享根仍须排除外部 writer | 核心 | 高 |
| U-11 | HEAD 查询检查点与终态 | schema revision 1 `upload_sessions` 以 owner 摘要+UUID 为键，持久化根内相对目标/stage 路径、长度/offset、stage dev/inode、可选 target revision 与 `Running/CommitStarted/AwaitingConfirmation/Committed/Rejected/Unknown`；stage 位于目标父目录中的当前 `0700` 子目录，启动时分页验证当前路径、目录权限/owner/设备与活跃 inode；非当前路径失败关闭且不移动文件或改写 SQLite。对外映射为 `running/awaiting-confirmation/committed/rejected/unknown`，`not-seen/not-started` 是仅存在于响应中的词汇。SQLite 是唯一状态权威。首个 offset 按 stage fsync、父目录 fsync、DB 提交排序；活跃 stage 路径跨 owner 唯一，部分 `Running` 与 `AwaitingConfirmation` 只接受记录绑定的 stage 身份。rename 前同步 stage 再持久化 `CommitStarted`，重启恢复为 `Unknown`；revision 条件冲突会退回持久的 `AwaitingConfirmation` 而不是丢弃满 stage。discard 原位 CAS `AwaitingConfirmation→Rejected` 后按 identity 清理，已有 `Rejected` 重试不续 TTL。仅部分 `running` 可续传，满 offset `awaiting-confirmation` 可明确发布或丢弃，`rejected/not-seen` 才允许换 ID，`committed` 精确匹配才成功 | 即使删除断点续传，也需保留终态和 awaiting-confirmation 协议，才能避免网络断开后重复覆盖，并让晚到冲突不必无条件重传；删除 owner、路径、stage identity、私有目录验证或提交屏障会造成跨账号泄露、路径竞态或歧义 ID 被错误重启 | 保障 | 高 |
| U-12 | PATCH 续传与空正文确认 | 普通续传的总长度和 offset 必须与 owner-scoped 会话完全一致，只写剩余内容；`AwaitingConfirmation` 只接受同一 ID、原总长度、满 offset 的空正文 PATCH，并要求当前 revision。长度或 offset 不匹配时响应仍准确携带 `awaiting-confirmation` 和原绑定值，要求先查询而不会把满 stage 降格成 `running`；目标再次变化也仍返回 awaiting-confirmation。每次可信 target-change 都重新发出 `refresh-required`，不会因同一 uploader 早先已失效过列表就吞掉后续通知。若带旧目标 metadata 的 stage 遇到目标消失，服务返回 `upload_metadata_preservation_refused`，浏览器先 discard，再以新 ID 完整 create-only PUT | 删除普通续传后失败任务只能重新 PUT；删除空 PATCH 确认会让每次晚到冲突都重传完整文件，错误降格确认状态会诱导客户端向只读满 stage 继续续传；忽略 metadata 例外则会把旧对象属性错误带到新文件 | 建议保留 | 高 |
| U-13 | 精确正文边界 | 超出声明剩余量返回 `413`；正文不足返回 `409`。确认态的剩余量固定为零，无论 `Content-Length` 已知还是以 chunked/流式正文送来首个多余字节，都会保留 `awaiting-confirmation` 与原绑定值并要求查询；传输层不会把只读满 stage 当作普通续传文件截断，也不会伪装成 `running` | 防止请求走私式额外数据和错误成功；截断或降格确认态会破坏满 stage，并诱导客户端继续重放正文 | 保障 | 高 |
| U-14 | 分阶段上传超时 | 服务端正文空闲时限默认 60 秒，绝对总 deadline 默认 24 小时并从等待路径租约前开始覆盖整个请求；首次 mutation 与该 deadline 原子竞争，deadline 先赢为 `not-started`，mutation 先赢后的外层超时为 `unknown/query_upload`。前端正文空闲 2 分钟、传输阶段总时限 24 小时，正文发送后清除传输计时并进入最长 5 分钟的独立提交确认等待；客户端提交等待超时或断线仍显示结果未知 | 删除会让卡死连接长期占槽；混淆服务端确定未启动与客户端“已经发送但未确认”会导致不安全重放 | 保障 | 中 |
| U-15 | 自动创建父目录及失败回滚 | fresh PUT 先从最近存在的父目录完成空间准入；空间不足不会创建祖先/stage/上传控制记录。准入后上传到不存在的深层路径时创建缺失祖先并记录身份；后续正文前会话准备失败时，自底向上只删除仍为空、身份未变且由本请求创建的目录 | 删除后文件夹上传和缺失目录上传要先手工 mkdir；删除身份化回滚会在拒绝请求后遗留目录或误删并发使用的目录 | 建议保留 | 中 |
| U-16 | 单文件大小与并发限制 | 默认 100 GiB、服务端并发 4。合法头与路径通过后先等待路径租约，再立即尝试上传槽；槽满直接返回绑定的 `429 not-started`，不读取或改变旧 owner state。该状态只证明本次尝试未进入上传 mutation；前端 Retry 必须先 HEAD，届时才恢复原 ID 的真实 committed/rejected/running/not-seen 状态。取得槽后，受跟踪 route metadata 任务也持有 permit；后续受跟踪上传 task 可做只读准备，但首次 filesystem/state mutation 必须赢得与总 deadline 的原子边界，deadline 先赢会 abort 且不能稍后写入 | 删除会造成资源无界使用；在 admission 前做慢 state/metadata I/O 会让准备任务绕过并发边界。response-only 状态、原子边界和强制 HEAD 使直接拒绝仍不会丢失旧检查点语义 | 保障 | 中 |
| U-17 | 磁盘最低水位 | 上传把声明逻辑字节和约 1 MiB + 64 KiB 的 xattr/checkpoint/目录项等元数据余量分别按实际 stage 文件系统的 `f_frsize` 向上取整后预留，随后大约每写入 8 MiB 异步复核。`fstat`/`fstatvfs` 在共享 mutex 外执行，返回后仅在同设备 revision 未变化时提交，最多重试 8 次，持续竞争以 `WouldBlock` 失败关闭；其他设备的变化不会迫使重试，block/fragment 乘法、取整或预算相加溢出也失败关闭 | 删除可能让并发上传写满文件系统；按目标路径而非实际 stage device 记账、允许整数折返、在全局 mutex 内调用慢文件系统或无限重试都可能破坏水位或公平性 | 保障 | 高 |
| U-18 | 崩溃持久化提交 | `flush → sync_all → 同文件系统原子 rename → fsync stage 与目标父目录 → SQLite 终态` 后才返回成功 | 删除会违背“成功后断电数据仍在”的目标 | 保障 | 高 |
| U-19 | 安全条件覆盖与 metadata 保留 | 覆盖会发布新 inode，但在提交前从单链接普通目标读取 numeric uid/gid、mode 和 xattr。setuid/setgid 位或任何 `security.*`、`trusted.*` 都会使覆盖拒绝；仅精确重放 `user.*`、`system.posix_acl_access` 等非特权属性。xattr 名称列表/条目数/单值上限为 64 KiB/1024/64 KiB，总分配最多 1 MiB。目标以 no-follow fd 分类并复核完整 identity；revision 同时绑定 owner、路径和该 identity。正文完成后 flush、确认长度、重放 metadata、`sync_all`，再于 rename 紧前复核 target revision 和 stage。Missing 用 `RENAME_NOREPLACE` 并在成功后核对 destination 与 stage fd；Existing 在复核后使用普通 rename，外部 writer 的相邻系统调用竞争仍属部署边界。确定的 pre-publication 失败清理并写 `Rejected`；完整 stage 的 revision 冲突持久化为 `AwaitingConfirmation`，可用最新 revision 空 PATCH 发布或显式 discard。目标消失但 stage 带旧 metadata 时必须 discard+新 ID 重传。发布后 identity 或父目录同步无法确认时是 `PublishedDurabilityUnknown` | 删除 metadata 保留或失败关闭会静默改变非特权属性；删除 revision 会让 Dufs 内部陈旧请求退化为无条件覆盖；尝试把特权 metadata 复制到攻击者可控新 inode 会扩大提权面 | 保障 | 高 |
| U-20 | 上传检查点分支 | fresh PUT 跨过 mutation boundary 后发生空闲/总超时或普通 I/O 错误，只有当前部分达到 20 MiB 才建立新检查点，较小部分清理；resumed PATCH 已拥有持久行，合法新增部分不受 fresh 阈值限制，失败时不会删除请求开始前的 checkpoint/stage。边界前 deadline/只读 I/O 失败不修改旧检查点。正文正常结束但短于声明长度时即使不足 20 MiB 也保存实际 offset 并返回 `409`；正文超量清除 fresh 会话、resume 则回退到原 offset；空间不足也回退到本次请求开始前的持久 offset | 删除 fresh 阈值会增加小暂存文件；把同一阈值错误用于 resume 会丢失合法检查点；删除续传可移除整个机制 | 建议保留 | 高 |
| U-21 | 有预算的过期上传清理 | `upload_sessions` 每次实际更新后按 7 天 TTL 计时；启动立即按不可变的 owner+UUID 键集游标分批清理过期 DB 行，此后每小时重试；当前页即使全被活跃 marker 或瞬时错误挡住，也会继续扫描后续页而不会形成队头饥饿。清理快照携带精确 `expires_at` 令牌；过期 `Running/AwaitingConfirmation` 只有在取得活跃 marker 后仍同时匹配完整业务行和该令牌，并复核根内路径及 stage dev/inode，才会删除 stage，TTL 刷新会使旧扫描立即失效。运行期 `CommitStarted` 明确排除在过期查询外，重启先转 `Unknown`。`Unknown/Committed` 及没有 stage identity 的 `Rejected` 可在后续写事务中顺带释放容量；带 identity 的过期 `Rejected` 不会被无关上传先删除控制行，而是由维护任务执行 discard 同款条件清理，再以原 snapshot+精确令牌+仍过期谓词删除行，身份不符时保留 replacement。根内扫描仍以 1024 项/100 ms 分片，只兜底无 DB 行的 orphan stage 和 orphan trash | 删除续传后可简化部分会话清理，但 awaiting-confirmation/discard 仍需有界回收；必须保留带 identity 的拒绝记录，并用含过期版本的 DB 快照+路径/inode 复核，防止 TTL 清理丢失安全清理能力、删除已刷新会话或同名替换物 | 保障 | 中 |
| U-22 | 认证失效暂停队列 | 首次明确的会话/CSRF 失败会暂停整队列，避免旧页面继续写请求 | 删除会产生大量确定失败请求 | 保障 | 中 |
| U-23 | 同页去重、二次冲突确认与未知暂停 | 页面生命周期内同一逻辑目标只入队一次。预检无冲突时零确认；已知可替换冲突只确认一次。上传期间目标出现或 revision 改变时，仅该文件进入 `awaiting-confirmation` 对话框，可用最新 revision 发布、跳过并调用 discard，或取消剩余队列；再次变化会再次确认并再次使列表 snapshot 失效，包括用户在两次冲突间完成 Refresh 的情况。未知/非法协议状态绝不自动覆盖，并暂停剩余队列；普通 Retry 仍先 HEAD 原 ID，`running/awaiting-confirmation/committed` 复用该 ID，只有明确 `rejected/not-seen` 才换新 ID，metadata 安全例外会先 discard 再新 ID 完整 PUT | 删除去重会让同一选择批次反复写同一路径；删除二次确认或 revision 检查会覆盖 Dufs 先前观察后变化的对象；删除未知暂停会在前一提交结果未确认时继续扩大不确定性 | 保障 | 中 |

成功上传的“真正落盘”保证仍取决于 Linux 文件系统、网络存储、virtiofs、设备和固件正确兑现同步请求。覆盖发布不保留原 inode、硬链接关系或原文件时间戳；有多个硬链接的目标会直接拒绝覆盖。Missing 目标通过 `RENAME_NOREPLACE` 防止晚到 occupant 被覆盖，并在成功后核对 destination 与已打开 stage；Existing 覆盖仍是身份复核后普通 rename，不是原子目录项 CAS。进程内路径租约和提交前身份复核不隔离拥有共享根写权限的外部进程，因此本地 shell、其他服务与 virtiofs 宿主机写者必须属于受信任的部署边界。介质损坏、后续位腐败和外部写入覆盖必须依靠权限隔离、运维纪律与备份处理。

最终提交明确区分发布边界。rename 前先同步满偏移 stage，再持久化 `CommitStarted`；该记录是歧义屏障，重启恢复为 `Unknown`，不因 stage 已只读、已 rename、缺失或异常而降格为 `not-seen`。确定发生在发布前的文件同步或条件复核失败会清理会话并尽力写入 `Rejected`；目标 revision 冲突则保留完整 stage 并转为 `AwaitingConfirmation`。Missing-target rename 成功后若 destination 无法再证明对应已打开 stage，或 rename 已经可见但父目录 fsync 未确认，返回 upload ID 和 `unknown` 并尽力写显式 `Unknown`；发布已持久化但 `Committed` 终态写入失败时也同样处理。只有同步、条件发布、发布后 identity 核对、父目录同步和 `Committed` 终态都成功且长度/offset 精确匹配时，前端才报告成功。

## 9. 新建、移动、重命名与删除

| ID | 当前特性 | 详细行为 | 删除后的影响 | 级别 | 复杂度 |
| --- | --- | --- | --- | --- | --- |
| M-01 | Windows 式新建空文件 | 点击后立即用零字节严格上传协议以 `newfile` 创建，确定冲突才依次尝试 `newfile (2)` 等候选；成功后在名称列原位编辑。每个候选使用新 upload ID，可信 awaiting stage 必须先明确 discard，unknown 不继续尝试 | 不影响上传已有文件，但缺少常用占位操作 | 可选 | 中 |
| M-02 | Windows 式新建目录 | 点击后立即以 `newfolder` 创建，服务端原子 `path_exists` 才递增为 `newfolder (2)` 等名称；成功后原位编辑。JSON API 仍支持一次创建深层目录及缺失祖先并同步目录项 | 删除后不能从页面组织目录 | 核心 | 中 |
| M-03 | 独立移动和行内重命名 | 页面和后端分别提供 Move 与 Rename。Rename 按钮把原名称位置切换为单一编辑器；文件默认选中最后一个扩展名前的主体，目录选中全名。`POST /move` 只接受源和已经存在的目标目录并保留 basename；`POST /rename` 只接受源和单段新名称并保留父目录。两者共享经过审查的路径租约、目标冲突、原子 rename、目录同步及 operation-id 提交基础；需要新目录时先使用 New folder。跨文件系统不自动复制后删除 | 删除后只能在系统侧整理文件 | 核心 | 高 |
| M-04 | 原子不覆盖 | 默认使用 Linux `renameat2(RENAME_NOREPLACE)`；目标竞争出现时返回 `409`，内核不支持该原子能力时失败关闭而不退化成“先检查再 rename” | 改成“先检查再 rename”会重新引入竞态覆盖 | 保障 | 中 |
| M-05 | 显式文件覆盖 | 用户确认后先复核 source/destination revision，再以普通 rename 原子替换；它不是能排除共享根外部 writer 的 compare-and-replace。目录不能被覆盖。若不同源/目标名称实际是同一 dev/inode 的硬链接，预检和 commit 内 fd-relative 复核都返回 `409 source_equals_destination`，不会把 POSIX rename no-op 报为 `204` | 删除后同名移动或重命名只能失败；放宽目录覆盖会增加递归语义；删除同 inode 复核会产生虚假成功 | 建议保留 | 中 |
| M-06 | 禁止目录移入自身 | 源目录不能移动到自己的后代 | 删除会产生无效或危险操作 | 保障 | 低 |
| M-07 | 删除文件或目录 | 页面确认后调用 `DELETE`；共享根本身始终返回 `403` | 删除后不再是完整文件管理器 | 核心 | 高 |
| M-08 | 持久化可见删除 | 先在同一父目录原子改名为隐藏 trash 并 `fsync`，再返回 `204` | 直接递归删除无法在中断时明确保证名称是否已消失 | 保障 | 高 |
| M-09 | 持久、有界、公平的后台回收 | DELETE 在 checked rename 前写无 revision 的 `Prepared`，父目录 fsync 后把完整 32 字节 trash revision 与 `Ready` 原子写入；outbox 全局 4096、每账号 1024，满载在可见 mutation 前拒绝。worker 原子 claim 为 `Claimed`，以 revision+持续 fd 锚点复核后按 256 项/25 ms 分片；普通 I/O 失败持久化回 `Ready`，100 ms 指数退避到最长 30 秒。defer/complete 瞬时失败时有界保留本地 claim，重启将 `Claimed`→`Ready`。`Prepared` 恢复永远保留 target、quarantine 任意 trash occupant 并释放 intent；Ready/Claimed 缺失 revision 或 `InvalidData` 也 quarantine/release。最终候选先移入随机 disposal 名并以 fd 复核，`ENOTEMPTY/EXIST` 不从 cursor 0 重扫；未记账 orphan 只有在通道满、取消或普通 I/O 失败时留待以后 maintenance，`InvalidData` 则使整根永久 quarantine。递归打开用 `RESOLVE_NO_XDEV`；同 UID 恶意 inotify 竞争随机名仍在威胁边界外 | 去掉 outbox/revision/fd 锚点、mount 边界复核、有界容量、公平分片、quarantine 或持久退避会引入同名替换物误删、跨存储删除、无界积压、健康 job 饥饿或永久丢失回收 | 保障 | 中 |
| M-10 | 无回收站/撤销 | 内部 trash 不对用户开放，逻辑删除后不可从页面恢复 | 若需要恢复功能，必须新增正式回收站模型 | 可选 | 高 |
| M-11 | 新项目权限策略 | 新建及零字节普通文件都从私有上传 stage 发布，并由显式 `fchmod` 使最终 permission bits 固定为 `0600`；新建和自动补建目录以 mode `0777` 请求创建，实际权限受进程 umask 与父目录 default ACL 影响；覆盖普通文件使用 U-19 的非特权 metadata 保留/特权 metadata 拒绝语义，替换符号链接后的普通文件仍为 `0600` | 新文件由服务账号拥有；目录权限继续依赖部署的 umask/default ACL。若底层策略自动赋予安全标签，应在部署环境验证；若需共享给其他本地账号必须明确设计权限策略 | 保障 | 低 |

原子 move 依赖同一文件系统的 `rename`。共享根中若包含不同挂载点，跨文件系统移动不会自动退化为“复制后删除”。

## 10. 根目录、符号链接与并发正确性

以下项目通常没有单独按钮，但共同决定文件管理结果是否可信。

| ID | 当前特性 | 详细行为 | 删除后的风险 | 级别 |
| --- | --- | --- | --- | --- |
| S-01 | 启动时根 fd 锚定 | 长期持有根目录 fd；外部重命名根并在原路径放新目录后，进程仍操作原对象 | 路径字符串重新解析可能切换到攻击者替换的目录 | 保障 |
| S-02 | fd 相对系统调用 | 使用 `openat2/openat/mkdirat/renameat2/renameat/unlinkat` | 退回普通路径会增加 TOCTOU 和符号链接逃逸 | 保障 |
| S-03 | 根内解析规则 | `RESOLVE_BENEATH \| RESOLVE_NO_MAGICLINKS` | 删除会允许路径越出共享根或进入 magic link | 保障 |
| S-04 | 根内相对符号链接 | 解析后仍在根内的相对链接可浏览和管理 | 若个人完全不用符号链接，可设计为全部拒绝，但会改变现有路径能力 | 建议保留 |
| S-05 | 根外链接拒绝 | 绝对链接和指向根外的链接隐藏且不能访问 | 删除会突破共享根边界 | 保障 |
| S-06 | 悬空/循环链接管理 | 根内悬空或自身成环的无效链接可列出、覆盖和删除，GET 不跟随无效目标；可解析但指回遍历祖先的目录链接可在单层列表显示，递归搜索会检测循环并以可重试 `409` 整次失败，不返回部分结果 | 删除后此类条目只能在 shell 中处理；删除递归循环检测会造成无界遍历 | 建议保留 |
| S-07 | 路径租约 | PUT、PATCH、DELETE、mkdir、move、rename 对同路径和祖先/后代冲突串行 | 多设备并发时可能互相删除或覆盖中间状态 | 保障 |
| S-08 | 符号链接语义键 | 除词法路径外比较沿途目录 dev/inode；真实目录路径和目录符号链接别名指向同一对象时冲突。最终文件符号链接仍按其目录项管理，并不与目标文件 inode 合并成同一键。语义解析错误使用以共享根 inode 为锚、与所有路径冲突的保守 wildcard 租约，随后由实际根边界/I/O 检查返回错误；不退化成纯词法租约，也不无限重试 | 目录别名可绕过并发协调；解析失败时继续放行会在最需要保护时失去别名冲突判断，永久重试则会泄漏 mutation task | 保障 |
| S-09 | 公平等待与重新解析 | 较早 waiter 仍在解析语义键时，只按词法祖先/后代关系阻塞后续 waiter，无关路径可超车；协调 epoch 变化时重新求语义键，插入前再次核对 epoch、现有租约和更早冲突 waiter。后来发现的符号链接别名会等待已取得的冲突租约；协调器不直接观察外部文件系统变化 | 全局阻塞解析会形成无关路径队头阻塞；不做最终语义/epoch 复验又可能沿用过期身份或放行别名并发 | 保障 |
| S-10 | 多路径原子登记 | move 和 rename 的源与派生目标先排序、去重，再在同一个 mutex 临界区整体检查并登记 | 删除整体登记会破坏确定性和冲突判断；当前算法并非逐把锁获取 | 保障 |
| S-11 | 提交任务独立收尾 | 浏览器或网关断开不会取消已经开始的最终 rename/fsync | 客户端取消可能让文件停在不明确阶段 | 保障 |
| S-12 | 单实例锁与外部写边界 | 根目录 fd 的独占 advisory `flock` 会阻止遵循同一协议的第二个 Dufs 实例；路径协调器仍只覆盖当前进程，shell、virtiofs 宿主和不理会 advisory lock 的其他程序不受约束。本文一致性保证要求 Dufs 独占写入共享根，人工修改只能停服执行 | 外部并发修改必须由部署侧排除；若要多节点共享同一存储，需要分布式协调而不是删除本地锁 | 保障 |
| S-13 | 常量 fd 的根内递归清理 | delete trash、过期上传和内部目录的递归清理保存根内相对目录路径与 cursor；每个工作片从已有父目录 fd 逐级使用 `openat(..., O_NOFOLLOW)`、`statat` 和 `unlinkat`，片结束前关闭工作 fd，不拼回绝对路径，也不依赖 `/proc/self/fd`。purge 错误把 job 持久化回 `Ready`；分片 cursor 不持久，重启从已记账 trash 根重新遍历 | 退回 procfs/普通绝对路径会重新引入部署依赖和路径替换竞态；跨片保存各层打开 fd 会使资源随深度增长；消费错误时丢 job 会破坏持久回收 | 保障 |
| S-14 | 普通写操作幂等协议 | 浏览器为 mkdir、move、rename、DELETE 生成 UUID operation ID；registry 在路径等待/业务校验前先按账号摘要、ID 和指纹建立 `Reserved`，同请求运行中返回 `202`，完成后可重放，不同指纹复用 ID 返回冲突。已知提交前错误记录为 `failed`；pre-commit guard 丢弃会移除预留，只有 `mark_commit_started` 后的异常才为 `unknown`。当前 revision 1 的文件型 SQLite 同时持久化管理 operation/upload/purge，使用 rollback journal `DELETE`、`synchronous=EXTRA`，启动删除 operation `Reserved`、把 operation `CommitStarted` 转为 `Completed/unknown`，并在 15 分钟 TTL 内重放 `Completed`；非当前 schema 不由服务迁移。Operation 容量全局 4096、每账号 1024；`/__dufs__/api/jobs/<uuid>` 是当前唯一公开查询入口且只包含这类 mutation | 删除后，断线或 `504` 会再次变成只能猜测并可能重复执行的结果不确定性；把所有 pre-commit 取消记成 unknown 会泄漏虚假运行记录；误把 SQLite 当成文件系统事务会产生错误恢复决策 | 保障 |

## 11. 资源、性能和错误边界

| ID | 当前特性 | 默认行为 | 价值 | 级别 |
| --- | --- | --- | --- | --- |
| R-01 | 全局连接上限 | 每个 listener backlog 为 1024；各 listener 先等待可读，再竞争所有监听器共享的 256 个连接槽，取得许可后才 `try_accept`，所以用户态已接受 socket 严格受上限约束，空闲监听地址也不会预占许可 | 防止连接耗尽进程，同时避免监听地址数量超过许可数时出现确定性地址饥饿 | 保障 |
| R-02 | HTTP/1 请求头预算 | 10 秒读取时限、64 KiB 最大缓冲 | 防止慢头和异常大头占用连接 | 保障 |
| R-03 | 普通请求与响应时限 | 300 秒内完成处理和响应头；已经交给提交任务的 mkdir/move/rename/delete 即使外层超时返回带 operation ID/state 的 `504`，仍会在后台完成并更新 registry，首方客户端随后只查询一次最终状态。响应正文无总时长，但每个源文件分块等待/读取和底层套接字写入分别有 30 秒 idle deadline | 防止处理、提交等待、故障源文件或完全停滞的客户端无限占用资源，同时不截断持续有进展的大文件传输 | 保障 |
| R-04 | Blocking 工作隔离 | RootedFs、列表/搜索、磁盘空间查询、上传维护及持久化 `fsync` 共用 64 个全局 blocking-I/O 准入槽；等待准入的磁盘探针保留在调用方 future 中，取消后不会留下脱离请求的排队任务。许可被移动到实际 blocking closure，内核调用开始后即使外层 future 超时或取消，也不会在调用退出前提前释放。搜索逐条累计结果容量，并在稳定归并排序的索引构造、合并和置换步骤检查取消/deadline | 防止故障 FUSE/NFS 让不可取消 syscall 或已取消请求的等待者无界占满 Tokio blocking pool，也避免先无界物化再事后检查 | 保障 |
| R-05 | 有界、一致性复核遍历 | 递归搜索使用显式 DFS，深度最多 1024、工作集最多 32 MiB，条目最多 100000；active-ancestor `HashSet` 按最大深度一次性预留并保守预检，结果 `Vec` 和路径字符串扩容前同时扣算旧、新缓冲区峰值。访问前和完成后复核所有访问目录，变化时以 `409` 失败；这仍不是原子文件系统快照 | 不会在结果之外再完整收集中间目录项向量，也不会因结果扩容瞬时越过预算；目录变化不会静默产生明显混合结果，强一致读取仍需存储快照 | 保障 |
| R-06 | 有界、公平分页快照 | 普通列表和递归搜索都最多物化 100000 项；结果以共享不可变切片保存并只排序一次，页读取只复制一个 `Arc` 后借用当前范围。默认进程级共享缓存绝对 TTL 为 120 秒，总计最多 32 个/64 MiB、每账号最多 8 个/32 MiB；library builder 可显式选择同上限的实例隔离缓存 | 同时控制大目录内存，阻止单账号驱逐其他账号全部快照，消除每页重新扫描/排序及字符串重复克隆；显式隔离适用于多租户 embedder | 保障 |
| R-07 | 异步上传磁盘记账 | 上传按实际 stage 文件的 Linux `st_dev` 分桶；同盘联合、异盘隔离。`fstat`/`fstatvfs` 在共享 mutex 外执行，按同设备 revision 验证后提交，最多重试 8 次并在持续竞争时失败关闭；block/fragment 乘法、分配单元取整和预算相加溢出也失败关闭。逻辑长度与约 1 MiB + 64 KiB 元数据余量分别按分配单元取整，并每写约 8 MiB 复核 | 封闭进程内部检查后的并发上传竞态，覆盖真实 stage 文件系统的元数据和块取整开销，并避免整数折返、无限重试、跨设备串行或小块 syscall 放大 | 保障 |
| R-08 | 明确过载状态码 | 并发满 `429`、超量 `413`、超时 `408/504`、空间不足 `507` | 网关、日志和前端可以区分失败原因 | 建议保留 |
| R-09 | 外部竞争边界 | 外部进程、virtiofs 宿主和存储侧变化不受 Dufs 的 advisory 根锁、路径租约或空间预留强制控制 | 必须保留额外磁盘余量并监控 | 保障 |
| R-10 | 小型协议正文上限 | browser API JSON 最多 16 KiB；Foundation 登录 JSON 在应用层最多 16 KiB，生产 nginx 的 exact `/api/v2/auth/login` 进一步限制为 4 KiB；分页 cursor 编码最多 4096 字节；Fetch 错误响应最多 16 KiB、成功响应最多 16 MiB，上传 XHR 响应最多 16 KiB | 防止辅助协议与客户端错误处理形成不必要的大分配 | 保障 |
| R-11 | 普通写提交 admission | mkdir、move、rename、DELETE 的受跟踪 mutation task 共用 64 个全局许可；额外请求等待许可并继续受普通请求 deadline 约束，上传使用独立并发预算 | 防止大量普通写请求无界创建提交任务；删除会恢复后台 mutation 数量失控 | 保障 |

常见 HTTP 状态语义：

| 状态 | 当前用途 |
| --- | --- |
| `200` | 登录/目录页面、列表与状态 API、完整文件，以及同长度已提交上传的安全重放 |
| `201` | 新建目录，或 fresh PUT 新建/覆盖文件成功 |
| `202` | 相同 operation ID 和请求仍在执行，客户端应查询状态而不是重发 |
| `204` | 注销、move、rename、DELETE 或 PATCH 成功且不返回正文 |
| `206` | 满足单段 Range 的部分文件响应 |
| `303` | 仅用于未认证 GET/HEAD 的 `Accept` 含精确 `text/html` 且合法 `q > 0` 时跳转登录；Foundation 登录 API 不重定向 |
| `304` | 下载条件请求确认表示未修改 |
| `400` | 路径、参数、cursor 编码/版本/请求绑定、JSON 或上传头无效 |
| `401` | 缺少有效认证，且请求不满足上述精确 HTML 导航条件 |
| `403` | CSRF/同源失败、删除共享根等禁止操作 |
| `404` | 文件、内部资源、上传检查点或已过期/不存在的 operation 记录不存在 |
| `405` | HTTP 方法不支持 |
| `408` | 登录正文读取超时，或上传空闲/总 deadline 超时；上传在 mutation boundary 前超时为 `not-started + retry`，边界后不能自动视为未写入 |
| `409` | 目标冲突、目录遍历/分页快照变化或不可用、cursor 认证标签不匹配、上传长度或 offset 冲突、operation ID 指纹冲突，或 MOVE/DELETE/fresh PUT 会切断仍有物理路径义务的控制状态 |
| `412` | 下载条件请求前置条件失败 |
| `413` | 小型协议请求体、上传、直接目录快照或搜索资源上限 |
| `415` | 登录或 browser API 的 Content-Type 不正确 |
| `416` | Range 无效、重复请求头、多段或不可满足 |
| `429` | 上传、目录分页或搜索并发已满；登录 admission、计算槽或管理员 username 组合退避满载时 Foundation API 直接返回 JSON `429 + Retry-After` |
| `500` | 未预期内部错误；上传确定未发布的 I/O 失败可返回 `rejected`，上传发布/终态持久性或带 Operation ID 的普通写结果无法确认时返回 `unknown`；公开响应不包含底层诊断 |
| `503` | readiness 未通过、服务停止期间取消目录遍历、operation/upload/purge 控制状态容量或命令 admission 暂不可用，或 namespace mutation 的持久路径安全检查暂不可用；上传只读准备阶段的未处理 I/O 为绑定的 `upload_precommit_failed + not-started + retry`，但仍须先 HEAD 原 ID，不能由 HTTP 状态单独决定重放 |
| `504` | 普通请求或目录遍历超时 |
| `507` | 上传受保护磁盘水位不足，或暂存文件创建/写入遇到实际空间或 quota 耗尽 |

## 12. 日志、健康检查和停机

| ID | 当前特性 | 详细行为 | 删除或简化后的影响 | 级别 | 复杂度 |
| --- | --- | --- | --- | --- | --- |
| O-01 | HTTP 访问日志 | 默认记录时间、级别、TCP peer、请求、状态，以及 mutation operation ID/state（无则为 `-`）；可加入认证管理员 username 或请求头 | 完全删除会降低故障、访问和结果不确定性定位能力 | 开发运维 | 中 |
| O-02 | 自定义日志变量 | 支持时间、毫秒、请求、方法、URI、状态、remote addr/user、operation ID/state、`$http_...` | 可硬编码固定格式以删除解析器 | 可选 | 中 |
| O-03 | 敏感头脱敏 | Authorization、Proxy-Authorization、Cookie、CSRF 统一记录为 `[REDACTED]` | 删除可能把凭据写入日志 | 保障 | 低 |
| O-04 | 单行转义和长度上限 | 控制字符转义；格式最多 4096 字节/128 元素；单条最多 16 KiB | 防止日志注入和巨型分配 | 保障 | 中 |
| O-05 | 有界异步日志 | 容量 4096；请求线程不阻塞；满时丢最新并聚合告警；250 ms flush；刷新失败保留 dirty 状态供后续重试 | 改同步写会让慢磁盘阻塞请求 | 保障 | 中 |
| O-06 | 安全文件或控制台输出 | `--log-file` 以 `O_NOFOLLOW|O_APPEND|O_NONBLOCK|O_CLOEXEC` 打开，要求当前服务用户拥有、单硬链接的普通文件；新文件固定 `0600`，已有文件必须预先精确为 `0600`，不安全权限不会被就地修改；未配置文件时全部日志使用 stderr，stdout 仅输出监听地址 | 防止高权限服务跟随符号链接、在特殊对象打开阶段阻塞或把 fd 泄漏给子进程，也避免 chmod 伪装修复已泄露或被预开 fd 持有的日志；单一控制台 sink 还避免 stdout 阻塞或刷新失败拖住 WARN/ERROR；只用 journald 时文件输出可删除 | 可选 | 低 |
| O-07 | 连接错误分类 | 记录 peer、协议、超时、断开、I/O 类型和系统错误码 | 对定位网关 `502`、超时和协议错误有价值 | 开发运维 | 低 |
| O-08 | accept 错误退避 | 从 50 ms 指数退避到 1 秒，下一次成功后重置 | 删除可能在 fd/内存耗尽时形成错误忙循环 | 保障 | 低 |
| O-09 | 公开 liveness | `GET/HEAD /__dufs__/health` 不要求会话并返回 `{"status":"OK"}`；它不访问文件内容、账号或共享根，只证明进程仍能处理 HTTP | 可供网关无凭据探活；删除收益很小 | 建议保留 | 低 |
| O-10 | 有硬截止的两阶段停机 | 首个 SIGINT/SIGTERM 停止接收并给普通任务/提交 30 秒；到期后取消普通工作、让停滞上传保存检查点或清理，再给受跟踪工作最多 10 秒。约 40 秒仍未完成则跳过日志 flush、立即以状态 1 强制退出；正常完成 tracked cleanup 后显式 `exit(0)`，避免 runtime drop 等待已取消但卡住的 blocking 工作突破时限 | 删除宽限会增加不明确结果；删除硬截止或显式退出会让故障内核/FUSE 工作无限拖住进程 | 保障 | 高 |
| O-11 | 第二信号立即退出 | 第二次 SIGINT/SIGTERM 不再等待或 flush 日志；SIGKILL 同样无法保证收尾，尾部日志可能丢失 | 是管理员明确的强制退出边界 | 开发运维 | 低 |
| O-12 | 正常退出日志 flush | 仅正常停止在 tracked cleanup 后由专用命名 OS thread 执行一次最多 5 秒的日志刷新，不依赖 Tokio blocking pool；主任务以 biased select 继续监听第二信号。硬截止和第二信号路径跳过并立即退出 | 删除可能丢失最后一批诊断记录；把 flush 放进强退路径或可能耗尽的共享阻塞池会突破硬截止，停止监听第二信号又会破坏管理员强退语义 | 开发运维 | 低 |
| O-13 | 内置资源日志降噪 | 只有成功返回的版本化资源 `GET` 跳过普通访问日志；资源错误、HEAD、登录、健康检查和其他请求仍记录 | 删除过滤会增加静态资源噪声；扩大过滤会漏掉诊断 | 建议保留 | 低 |
| O-14 | 认证 readiness | `GET/HEAD /__dufs__/ready` 要求有效会话；通过锚定根 fd 真实创建隐藏文件、写入、同步文件、删除并同步根目录，同时在现有 SQLite actor 连接执行 `BEGIN IMMEDIATE`、写入探针行并 `ROLLBACK`；还检查扣除进程预留后的 `min-free-space` 和停机状态，失败返回 `503 {"status":"not_ready"}` | 比 liveness 更适合受控冒烟检查；它证明当前根目录和状态库基本可写，但不执行 rename/介质读回，也不预测目标冲突、purge/上传容量等全部业务准入 | 建议保留 | 低 |

## 13. 内置资源、缓存和部署

| ID | 当前特性 | 详细行为 | 取舍建议 | 级别 |
| --- | --- | --- | --- | --- |
| E-01 | 编译时嵌入资源 | 生产运行不读取 `clients/web/` 外部目录，不支持运行时覆盖 | 保留可保证代码和页面版本一致 | 建议保留 |
| E-02 | 内容摘要资源 URL | 目录页的 `index.js`、18 个 ES module、`index.css`、登录页的 `login.css` 和 favicon 由 `server/assets.rs` 的单一注册表按名称、MIME 类型和内容共同生成完整 256 位 SHA-256，即 64 个十六进制字符的资源前缀；HTML 和内联登录脚本不参与该前缀，后者由独立 CSP SHA-256 授权。静态门双向核对 `clients/web/modules/` 与 `EMBEDDED_ASSETS` | 删除后要改用短缓存或手工版本号；混淆两套摘要或漏嵌模块会造成缓存、404 或 CSP 文档漂移 | 建议保留 |
| E-03 | 静态资源长期缓存 | 只有精确命中的成功摘要资源使用一年 `immutable`；其他响应 no-store | 删除会增加重复资源传输，但不影响功能 | 可选 |
| E-04 | 唯一非 React/Vite 例外 | Dufs 客户端是项目组唯一明确保留无 bundler 原生 ES modules 的前端；二进制及页面只由 Cargo 构建。Node 24.8 只用于检查、测试和发布辅助，不形成生产前端服务 | 改为 React/Vite 会扩大 Dufs 的构建和供应链；删除这项例外则必须重新设计嵌入、摘要 URL、CSP 与发布验证 | 建议保留 |
| E-05 | 分层前端脚本 | 目录页由 `index.js` 入口和 18 个 ES module 组成：`app.js`，`shared/{dom,index_data,mutation_effect,path}.js`，`http/{client,headers,response_buffer}.js`，`listing/controller.js`，`operations/{dialogs,file_operations}.js`，以及 `upload/{manager,preflight,protocol,queue,selection,transport,view}.js`。`index_data.js` 将 JSON.parse 的 unknown 严格验证为外层 `href/dir_exists/session` 三字段和 Foundation session 五字段的 frozen 对象；`mutation_effect.js` 定义 committed/outcome-unknown/refresh-required/not-committed 四值失效；`http/headers.js` 提供严格无符号头解析，`http/response_buffer.js` 负责 Fetch 正文上限、取消和重放流，`upload/protocol.js` 集中维护上传头名、允许状态码及按当前文件总长度绑定的单一状态解析。独立 `login.js` 编译进可执行文件，并在渲染每次登录响应时内联，由精确 CSP hash 授权；登录 CSS 是同一摘要前缀下的外部资源，CSP 不再允许 inline style | 合并成单文件会减少文件数但降低可维护性和测试定位；复制响应或上传协议规则会重新引入调用方漂移；修改登录脚本必须同步通过 CSP 摘要测试 | 建议保留 |
| E-06 | 多地址 TCP | 可同时监听多个明确 IP，所有监听器共享资源上限 | 若实际永远只监听一个地址，可简化参数和启动循环 | 可选 |
| E-07 | IPv6 | 显式 `--bind ::` 或其他 IPv6 地址；IPv6 listener 强制 `IPV6_V6ONLY`，因此 `::` 不同时承接 IPv4，双栈必须分别配置 IPv4 与 IPv6 地址 | 仅使用 IPv4 网关时可删除，但代码收益有限 | 可选 |
| E-08 | 严格分离的前端协议与响应边界 | 目录页 Fetch 统一经 `http/client.js` 编排并使用 30 秒 deadline；登录页单独向 Foundation `/api/v2/auth/login` 发送 JSON Fetch，原生导航和文件下载不在这两个边界内，上传正文另用专用 XHR。`http/response_buffer.js` 先按严格 `Content-Length` 拒绝再逐块读取；错误响应最多 16 KiB、成功响应最多 16 MiB，超限取消 reader/body。Problem Details 只接受 current `application/problem+json` 平铺 snake_case 结构；Foundation auth 则只接受 `AdministratorSession/ErrorEnvelope`。上传 XHR 在响应头、download progress 和最终 UTF-8 字节数三个阶段拒绝超过 16 KiB。operation 与 upload 解析都绑定规范 ID、状态、长度和 offset；异常 2xx、网络或协议结果只查询原 ID，不盲目重放 mutation | 删除会恢复无限等待/缓冲、两种错误合同混淆、协议词汇漂移和无法安全判断 mutation 是否可重试的问题；降低成功上限可能误拒合法大列表 | 保障 |
| E-09 | 明文 HTTP/1 回源 | 使用 Hyper HTTP/1 连接处理器，接受 HTTP/1.0 与 HTTP/1.1；拒绝明文 HTTP/2 prior knowledge，不实现 `Upgrade: h2c`。全部后端连接统一受 10 秒请求头时限、64 KiB 接收缓冲和连接预算约束 | 消除 HTTP/2 单连接并发 stream 绕过连接预算的边界；生产网关仍固定用 HTTP/1.1 回源 | 保障 |
| E-10 | nginx 生产网关基线 | 样例要求 nginx ≥1.25.1、HTTP SSL/HTTP2 模块和仍获上游或发行商安全更新的 OpenSSL，只以独立 `http2 on;` 启用 HTTP/2，明确拒绝已弃用的 `listen ... http2` 旧语法；固定规范域名并将 HTTP `308` 到该域名，拒绝未知 HTTP Host 与 HTTPS SNI/Host，启用 TLS 1.2/1.3 和 HSTS。它以 HTTP/1.1 回源并覆盖单值 Host/XFF/XFP，关闭请求/响应缓冲、缓存、错误拦截和重试；唯一 current exact `location = /api/v2/auth/login` 按来源 IP 限制 5 请求/分钟、burst 5、4 个连接、4 KiB 正文和 10 秒正文时限，不保留 `/__dufs__/login` POST location。隔离的真实 nginx 行为测试验证新语法、拒绝与恢复 | 模板必须替换域名和证书；后端不是默认 `127.0.0.1:5000` 时还要替换 upstream，并配合防火墙。删除这些边界会让认证、来源 IP、结果不重放和超时假设失效；退回 nginx 1.24 则配置会失败关闭 | 开发运维 |
| E-11 | systemd 最小权限基线 | 样例使用专用 `dufs` 用户/组、`UMask=0077`、`ProtectSystem=strict` 且只允许 `/srv/dufs` 写入，清空 capability 并启用 `NoNewPrivileges`、设备/临时目录/主目录/内核与 namespace 等沙箱；另设重启、65536 fd 和 120 秒停止超时。门禁对 unit 做 `systemd-analyze verify`，不宣称实际启动了沙箱服务 | 用户、路径和平台能力必须按部署同步调整；语法验证不能代替生产主机上的启动、权限和写入冒烟 | 开发运维 |
| E-12 | favicon | 目录页通过摘要资源前缀加载编译内置的 `favicon.ico`，浏览器标签页显示项目图标 | 纯外观能力，可与 X-18 一并删除 | 可选 |

浏览器入口使用 HTTPS 不仅是因为会话 Cookie 带 `Secure`；前端生成上传 UUID 所用的 `crypto.randomUUID()` 也要求安全上下文。

## 14. 构建、测试和代码结构

这些能力不会出现在浏览器中，但决定项目能否安全修改。

| ID | 当前特性 | 作用 | 删除后的影响 | 级别 |
| --- | --- | --- | --- | --- |
| T-01 | 固定 Rust 工具链 | Rust 1.98.0、edition 2024、Rustfmt、Clippy | 开发机结果可能漂移 | 开发运维 |
| T-02 | `Cargo.lock` 与不可变 Foundation 来源 | 固定完整依赖图；四个 Foundation crate 均为 `=0.3.1` + Git rev `7c6a210cd5fc8bf987e0f50fccee69b7c58cbdf0`，无 workspace/path/branch/local-copy fallback | 构建不可重复、认证/Schema/target 合同漂移且审计结果不可复核 | 开发运维 |
| T-03 | Linux 构建守卫 | 编译阶段明确拒绝错误目标 | 错误平台可能到运行时才失败 | 保障 |
| T-04 | Rust 模块分层 | `server.rs` 保留共享状态与模块协调；`router.rs`、`assets.rs`、`delete.rs`、`purge.rs` 分别负责请求路由、内置资源注册/摘要、删除提交事务和回收调度。`listing/{snapshot,walk}.rs` 隔离进程级快照/游标缓存与有界递归遍历；`rooted_fs/purge.rs` 隔离 fd-relative 删除执行器；`internal_names.rs` 与 `maintenance.rs` 提供服务端中性的内部名称和清理边界；`upload/{prepare,target,transfer,commit,failure,protocol,record}.rs` 隔离路径/会话准备、目标 identity/revision、传输、提交、失败、协议与检查点持久化。`server`、`listing`、`rooted_fs` 与 `upload` 的大段内联单元测试均位于各自 `tests.rs`，仍保留模块私有访问 | 拆分只移动内部职责，不改变 HTTP/上传协议，也不新增第三方依赖；重新合并不会减少能力，只降低边界清晰度、维护性和测试定位 | 开发运维 |
| T-05 | 可复用 `lib.rs` | 测试可在进程内构造服务层 | 删除会增加只能启动外部进程的测试成本 | 开发运维 |
| T-06 | `RequestContext` | HTTP 边界集中 peer 和访问日志身份 | 删除会让认证身份和日志再次分散 | 开发运维 |
| T-07 | `AppError` | 区分公开状态/说明与内部诊断来源；JSON API 错误稳定提供机器可读 `code` 和面向用户的 RFC `detail`，底层诊断只进入日志 | 删除可能把文件系统细节暴露给客户端，或迫使客户端解析自然语言判断错误类型 | 保障 |
| T-08 | 可注入持久化边界 | `StorageDurability` 先独立注入文件 sync；替换边界返回 `Published`、`Rejected`、`NotPublished` 或 `PublishedDurabilityUnknown`，测试可区分发布前失败、Missing 发布后 identity 无法确认以及 rename 后父目录 fsync 失败 | 删除会使真正落盘的故障路径难以自动测试，也容易把确定未发布与发布后未知混淆 | 开发运维 |
| T-09 | Rust 自动化 | 单元、集成、故障注入、不可变分页/搜索快照、退役目录归档路由、Range、认证、限流、协议、符号链接、纯 fd 清理和真实停机测试；范围与 URI 编解码另有大样本性质测试 | 删除后修改核心文件语义的风险显著上升 | 开发运维 |
| T-10 | 隔离 Playwright | Chromium 和 Firefox 必需、正式 Edge 可选；通过只呈现一个客户端地址的本地 HTTPS 网关运行，因此固定单 worker 串行执行，避免无关用例争抢生产登录令牌桶；失败重试 1 次且 `failOnFlakyTests: true`，所以重试通过仍会让门禁失败；每项测试使用随机目录。Rust HTTP 集成测试精确断言安全响应头；Playwright 验证 Secure Cookie、CSP violation、可访问性语义和真实浏览器交互 | 删除后无法验证真实浏览器行为及测试间状态污染；删除 Rust 断言会让响应头策略漂移失去精确回归保护 | 开发运维 |
| T-11 | 依赖安全审计 | `cargo audit` 固定为 0.22.2，并与 `npm audit --audit-level=high` 一起由 lockfile/manifest push、PR、每周计划及人工任务触发；Rust 审计显式 `--deny yanked`。发布只复用通过 canonical origin、HEAD/FETCH_HEAD、新鲜度、物理/Git/内容完整性检查的宿主 RustSec DB；alternates、不安全条目、untracked 或 tracked 内容/mode 漂移均拒绝。数据库以无硬链接私有 clone 封存 revision/fetch epoch/index/config；否则在任何项目/依赖代码前用 dummy lockfile 联网刷新。先执行 sealed `--no-fetch --no-yanked` advisory pre-audit，再用私有 Cargo home `fetch --locked` 填充覆盖完整锁图的 crates.io 索引项并执行 `--deny yanked`，随后以必填 `DUFS_QUALITY_AUDIT_DB` 交给 `scripts/check.sh`，在任何构建、测试或依赖安装前复审；预审计和 yanked 检查后重验封存，完整门后随质量树销毁数据库。制品清单只记录 revision/fetch epoch | 无法及时发现已知漏洞或已撤回依赖；空 crates.io 索引会让 cargo-audit 只打印无法检查而仍返回成功，因此覆盖完整锁图的私有索引项也是绿色结论的必要输入；直接复用可变、过期、内容漂移或来源不明的数据库会破坏时间、来源和完整性证据 | 开发运维 |
| T-12 | 统一质量与部署门禁 | `scripts/check.sh` 运行 Rustfmt、Clippy `-D warnings`、全 targets/features 测试、固定 `cargo-llvm-cov 0.8.6` 且行覆盖率不低于 70%、Cargo/npm 审计、固定 Acorn 8.17.0 AST 与有界词法常量 JS 分析及正负对抗样例、TypeScript 5.9.3 strict `checkJs` 全生产源码类型检查、支持围栏代码与 symlink fail-closed 的 Markdown 链接/锚点检查、含固定 `@axe-core/playwright 4.12.1` WCAG A/AA 扫描的双浏览器测试、生产解析器 YAML 校验、systemd/nginx 语法及隔离的真实 nginx 行为测试，并执行发布 no-clobber、Git 来源替换、归档树、SPDX notice、签名算法矩阵/失败传播和 lockfile npm cache 播种自测。六个 Bash 源总是经过 `bash -n`，安装 ShellCheck 时再执行 warning 门；CI 固定安装并强制使用 0.11.0。动态 computed 解构的属性名无法静态求值时失败关闭；原生 `alert/confirm/prompt` 的直接、别名、计算属性和反射访问同样由 AST 负例门拒绝。外部/解析输入保持 `unknown` 并由类型守卫收窄，生产源码不保留显式或隐式 `any`。部署 fixture 的真实 checkout 路径包含空格、`&`、`#` 和反斜杠，运行副本再使用安全名称 | 仍可手工执行，但容易漏项或让文档/部署示例与代码漂移；Acorn 门是防御纵深静态分析，strict `checkJs` 无需迁移 `.ts`，二者仍不等价于完整跨过程污点证明、ESLint 或通用 CommonMark parser。本地缺少 ShellCheck 时会明确跳过以保持离线可用，强制性由 CI 提供 | 开发运维 |
| T-13 | 100000 项手工基准 | 默认忽略，按需创建真实超大目录检查第一页性能 | 删除不影响正确性，但失去大目录回归基线 | 开发运维 |
| T-14 | 可验证本地发布 | release profile 使用 `opt-level=3`、LTO、单 codegen unit、`panic=abort` 和 strip；脚本要求干净 worktree、Cargo 版本与精确指向 HEAD 的 tag。完整 `scripts/check.sh` 在已验证 commit archive 的无 Git 私有副本中以清空环境、独立 Cargo/npm/target/tmp 执行；Cargo vendor 后离线，npm cache 只按 lockfile HTTPS+SHA-512 播种并 prefer-offline。门禁后用 snapshot index 复验 tracked 内容/mode 并拒绝非忽略新增路径，丢弃质量树，再 fresh extract 构建；签名/发布前继续复核 exact source。所有源码树拒绝 symlink、submodule 和特殊文件，只从摘要锁定 bare façade 归档，前后构建/打包 archive 的 commit、树、mode、额外路径和 SHA-256 均复核。固定 `cargo-cyclonedx 0.5.9` 离线生成规范化 SBOM，source revision 只接受恰为 40 或 64 位的小写十六进制对象 ID；第三方 notice 要求每个 vendored 可达非开发依赖有非空、经审核的 SPDX `license` 表达式，再解析审核清单内 SPDX AST 并要求完整 permissive 分支。`license_file` 仅收集依赖自身 no-follow UTF-8 许可证文本，不能替代缺失表达式或作为分类 fallback，项目许可证也不作正文 fallback。Rust 1.98.0 标准库 notice 还须匹配审核摘要。`BUILD-ENVIRONMENT.txt` 记录完整 SHA/版本/epoch/target 和实际 Bash、Rust/Cargo、Node/npm、Git、OpenSSL、归档/coreutils 版本。该清单、SBOM、项目许可证、两类 notice 和包内文件均进 checksum；签名密钥最后才短暂打开，并只允许 Ed25519、Ed448、RSA ≥3072 bit 或 `prime256v1`/`secp384r1`/`secp521r1` ECDSA，其他算法/强度失败关闭。输出目录须为当前 UID 所有且 group/other 不可写，经目录 fd 独占锁和 `/proc/self/fd` 锚定；私有 stage 与目标必须同文件系统，并依赖支持 `--update=none --no-copy` 的 GNU `mv` 做原子 no-clobber 发布，且以 source 必须消失的后置条件把静默碰撞变为失败 | 删除会失去源码到制品的可追踪性、依赖/许可清单、密钥强度底线和隔离验收流程。npm 缺失包/审计仍可能联网，环境清单只记录事实而不钉扎宿主工具，SBOM 规范化不等于完整 CycloneDX schema 验证；晚打开只缩短同 UID 暴露面，正式签名仍需独立账号、主机或 HSM | 开发运维 |
| T-15 | Node、浏览器与宿主工具边界 | `package.json` 与根 lockfile 只接受当前 Node 24.8.0；`package-lock.json` 精确锁定 Playwright 1.61.1、`@axe-core/playwright` 4.12.1、Acorn 8.17.0 和 TypeScript 5.9.3；远程工作流固定 Rust 1.98.0、ShellCheck 0.11.0、cargo-audit 0.22.2 及 ShellCheck 归档 SHA-256。静态、Rust、浏览器、审计、性能和 release binary job 使用 `ubuntu-24.04`；需要 nginx 1.25.1+ 的质量与正式包 E2E 使用 x64 `ubuntu-26.04` preview，所有关键 job 记录实际 runner image 和工具版本。正式包另以 `BUILD-ENVIRONMENT.txt` v2 记录实际发布工具和 RustSec DB 身份。本地 ShellCheck、npm、nginx、systemd、OpenSSL、Bash、Git、curl、GNU tar/gzip/coreutils、util-linux `flock` 和可选 Edge 的版本未由仓库统一钉死 | 删除 lockfile 会让前端门禁漂移；环境清单只支持追溯，不会把“固定 CI 关键工具”或一次记录变成整条宿主链逐包可重复；26.04 preview 的调度或镜像回归会阻断质量/正式包 E2E，但不得因此回退旧 nginx 语法；仍须复验 GitHub runner 镜像和本地宿主工具 | 开发运维 |
| T-16 | 支持版本与私密报告策略 | 安全修复在当前源码树开发，但 dirty worktree 或仓库 HEAD 不自动成为受支持二进制；仅按 exact tag、checksum 和签名流程生成的最新正式制品受支持，正式发布前不声明任何受支持二进制。漏洞应通过供应方的私密安全/事件通道报告，提供受影响版本和 `dufs --version` 的完整 Git SHA，并对配置、路径和凭据材料脱敏；发行方必须随二进制公布实际受监控的私密联系地址，公开上游 issue 不视为保密渠道 | 删除明确策略会混淆源码审查、正式制品和下游修改版的支持责任，也可能把敏感报告泄露到公开渠道 | 开发运维 |
| T-17 | 只读分层远程 CI | `.github/workflows/read-only-ci.yml` 仅使用 `pull_request`、`push` 和人工触发，权限为 `contents: read`，checkout 不持久化凭据，Action 固定完整 commit SHA。静态层以唯一当前 Node 24.8.0 运行 Shell/JS/type/docs，Rust 层运行 fmt/Clippy/test，质量层独立报告覆盖率、部署、发布脚本自测和 release binary smoke，浏览器层独立矩阵运行 Chromium 与 Firefox；静态、Rust 和浏览器用稳定 `ubuntu-24.04`，含现代 nginx 部署验证的质量层用 x64 `ubuntu-26.04` preview；不接收发布密钥、不创建 tag/release、不上传制品 | 删除后仍可运行权威本地门，但会失去每次远程变更的分层反馈；把该门当正式发布会绕过审计、exact-tag、签名和原子发布链；preview runner 不可用时质量结论不可用，不能跳过质量 job 合并 | 开发运维 |
| T-18 | 正式发布包真实 E2E | `.github/workflows/formal-release-e2e.yml` 在 `v*` tag、每周和人工触发时，以只读权限在 x64 `ubuntu-26.04` preview 的含 shell 元字符隔离 clone 中建立精确本地版本 tag、生成临时 Ed25519 key，并不带跳过开关调用真实 `package-release.sh`。它经过完整检查、vendor/build/SBOM/checksum/sign/sync/no-clobber 链，随后独立核验外层四项制品、签名/公钥、包内 `SHA256SUMS` 与二进制完整版本/SHA；不引用生产或自定义 secrets，只使用只读 GitHub token checkout，也不上传输出。便捷 Release 必须等待同 tag/SHA 的该任务成功 | 删除后 helper 自测仍可通过，但 exact-tag、隔离总门、真实构建和最终签名发布组合路径可能长期无人执行；临时测试密钥只证明流程，不构成生产信任根；preview runner 调度或镜像失败会推迟发布，不能用跳过 E2E 或旧 nginx 兼容替代 | 开发运维 |

T-12 的覆盖率门同时要求仓库总行覆盖率至少 70%，以及每个被插桩源码文件至少 1%；逐文件底线用于拒绝整个模块零覆盖，不代表 1% 已达到充分测试。

T-14 的原子 no-clobber 保证要求发布文件系统支持 Linux `RENAME_NOREPLACE`。脚本只在 source 消失、destination 是实体目录且设备号/inode 与移动前 source 相同时确认发布；`--update=none` 静默跳过、身份不符或不完整移动都会失败。

T-14 的发布顺序还包含 RustSec 输入封存：宿主 DB 通过完整验证或在任何项目/依赖代码前用 dummy lockfile 私有刷新，随后执行 sealed `--no-fetch --no-yanked` advisory pre-audit；私有 Cargo home 再用 `fetch --locked` 取得完整锁图所需的 crates.io 索引项，执行 `--deny yanked` 后才进入 vendor；该 Cargo home 每次全新创建，因此正式打包当前要求 registry 网络可达。`scripts/check.sh` 要求同一 `DUFS_QUALITY_AUDIT_DB` 并把审计放在其他项目/依赖步骤之前。封存时校验 seal 与新鲜度，pre-audit 和 yanked 检查后复核相同 revision/index/config；完整质量门后同时复核 seal 与新鲜度，随后销毁质量树和该 RustSec 数据库。`BUILD-ENVIRONMENT.txt` 当前使用 `dufs-build-environment-v2`，除既有工具字段外记录 cargo-audit 版本、RustSec advisory DB revision 和最近 fetch epoch，但不记录内部 index/config seal 摘要。包内文档检查先完成，`SHA256SUMS` 才作为最后一次内容变更生成；之后只读复核递归覆盖。`--self-test` 另验证深层 sentinel、篡改失败、两次归档一致和解包往返，T-18 再覆盖未缩短的真实正式入口。部署门中的 systemd 只使用占位 `ExecStart` 做静态验证，真实 nginx 连接的是 mock upstream，不等价于生产 systemd+Dufs+nginx 联合启动。

## 15. HTTP 入口总表

这些入口是浏览器内部协议，不代表项目承诺兼容 WebDAV 或通用第三方 API。Dufs 固定部署在独立主机名的根路径 `/`，下列路径均相对于该域名根路径。

| 方法与路径 | 用途 | 认证 | CSRF/同源 |
| --- | --- | --- | --- |
| `GET /__dufs__/login` | 只返回登录页面；其他方法 `405` | 否 | 只读；无 POST alias |
| `POST /api/v2/auth/login` | 严格 Foundation `username/password` JSON；规范化 candidate、创建内存会话并返回 `AdministratorSession` | 否 | Foundation 严格同源；不使用 CSRF |
| `GET /api/v2/auth/session` | 返回当前五字段管理员 session | 是 | 只读 |
| `POST /api/v2/auth/logout` | 撤销当前会话并清 Cookie | 是 | `X-CSRF-Token` + Foundation 严格同源 |
| `GET/HEAD /__dufs_assets_<digest>/*` | 登录页所需内置 JS、CSS、图标；HEAD 与 GET 使用相同 metadata，不发送正文 | 否 | 只读且只允许精确摘要资源 |
| `GET/HEAD /__dufs__/health` | 不访问共享根的公开 liveness | 否 | 只读 |
| `GET/HEAD /__dufs__/ready` | 以创建/写入/文件同步/删除/目录同步探针检查共享根，并以回滚写事务检查统一 state store，同时检查空间和停机状态 | 是 | 探针写入后清理/回滚 |
| `GET /__dufs__/api/list` | 分页列表或递归搜索结果 | 是 | 只读 |
| `POST /__dufs__/api/mkdir` | 新建目录 | 是 | CSRF + 同源 + JSON |
| `POST /__dufs__/api/move` | 移动到目标目录并保留原名称 | 是 | CSRF + 同源 + JSON |
| `POST /__dufs__/api/rename` | 在原父目录内修改名称 | 是 | CSRF + 同源 + JSON |
| `GET /__dufs__/api/jobs/<uuid>` | 统一查询当前账号的 mutation job；当前复用 operation registry | 是 | 只读 |
| `GET/HEAD /目录/` | 目录页面骨架 | 是 | 只读 |
| `GET/HEAD /目录/?q=文本` | 搜索页面骨架，结果由 list API 加载 | 是 | 只读 |
| `GET/HEAD /文件` | 附件下载与 metadata；只有 GET 处理单段 Range，HEAD 忽略 Range | 是 | 只读 |
| `HEAD /目标` + upload ID | 查询上传检查点 | 是 | 只读 |
| `PUT /目标` | 新上传、覆盖或零字节新文件 | 是 | CSRF + 同源 + 上传协议头 |
| `PATCH /目标` | 从持久化 offset 继续上传 | 是 | CSRF + 同源 + 上传协议头 |
| `DELETE /目标` | 持久化移除文件、目录或可管理的根内链接 | 是 | CSRF + 同源 |

首方浏览器必须在 mkdir、move、rename 和 DELETE 上携带规范 UUID 的 `X-Dufs-Operation-Id`，缺失或非 canonical UUID 会在 mutation 前返回 `400 invalid_operation_id`。第三方客户端若使用这些内部协议，也必须生成新 ID，并在结果未知时通过 job 端点查询状态而不是重发。同一 POST ID 的安全重试必须保持原始 JSON 正文字节完全一致，键序或空白变化也会产生不同指纹；DELETE 的指纹绑定已解码相对路径。

browser API JSON 中的 `path`、`source`、`directory` 与 `name` 已经是逻辑路径或名称字符串，后端不会再次进行 percent-decode；例如 `%2F` 表示名称中的三个字面字符而不是路径分隔符。查询字符串则遵循各路由自己的解析规则，不能把 URI 编码和 JSON 路径规则混用。

以 `__dufs__` 或当前摘要资源前缀开头的内部入口只接受唯一规范原始 URI：尾斜杠、重复斜杠、编码后的分隔符和对 unreserved 字符的多余百分号编码都会在执行路由前被拒绝。普通共享文件与目录仍保留各自合法的尾斜杠语义。

## 16. 当前明确不存在的功能

以下能力已经删除或有意不支持，不应在取舍时误认为仍有遗留实现：

- 匿名共享文件、目录、目录管理页或管理 API 访问；无需会话的入口只有登录页 GET、Foundation 登录 POST、精确内容摘要静态资源和固定 JSON liveness；
- 账号角色、只读账号、目录级权限或能力开关；
- Basic、Digest、URL token 或下载令牌认证；
- CORS 和跨站浏览器 API；
- WebDAV、Microsoft MiniRedir 兼容及相关方法；
- 在线预览和在线编辑；
- 静态网站托管、SPA fallback、自定义 `404.html`；
- 单文件共享模式；
- 无 JavaScript 页面或 `?noscript`；
- `?json`、`?hash`、`?simple` 等旧查询输出；
- 目录 ZIP 或其他目录归档输出；
- 多段 Range；
- 拖放上传和拖放目录递归；
- 手机 Web 支持承诺；
- 运行时 `assets` 覆盖；
- 内置 TLS 和证书参数；
- 明文 HTTP/2 prior knowledge、h2c Upgrade 或其他 HTTP/2 后端协议；
- Unix socket；
- 远程 Git 发布；
- Shell completion 生成；
- 环境变量配置；
- URL 路径前缀和子路径部署；
- 用户自定义隐藏规则；列表和搜索包含所有普通项目，内部保留项仍不可见；
- Windows、macOS、32 位 Linux 或其他系统适配；
- 多节点/分布式协调、让多个 Dufs 实例共享同一根目录，以及旧版本兼容分支；本机同根第二实例会被 advisory `flock` 拒绝；
- 用户可见回收站、删除撤销和版本历史；
- 内置备份、恢复、存储快照或数据回滚；制品回退也不会撤销版本切换后发生的用户写入。

## 17. 已知边界与当前不足

这些内容不是隐藏功能，判断取舍时应同时了解：

1. Foundation 当前密码合同是 12～1024 个 UTF-8 字节且不含 ASCII 控制字符，并固定当前 Argon2id 参数；它没有字符类别或强制熵规则，管理员仍应使用高熵密码管理流程。
2. Dufs 已有正文读取全局/每 IP 并发限制、全局/每 IP token bucket、Argon2 并发上限和按“客户端 IP + canonical 管理员 username 摘要”组合键的失败退避，但状态只在当前进程内、成功会清除对应组合记录，也不是分布式防护；公网网关仍应独立按可信真实 IP 限速。
3. 会话空闲 30 分钟和绝对 12 小时目前是固定常量，不能通过命令行或 YAML 调整；两者按 Linux `CLOCK_BOOTTIME` 计时，系统休眠时间同样消耗期限。
4. 公开 `/__dufs__/health` 只证明进程和路由能响应；认证 `/__dufs__/ready` 会真实创建隐藏文件、写入、同步文件、删除并同步根目录，还会在当前 SQLite actor 连接中执行回滚写事务。它仍不执行 rename 或介质读回，也不预测目标冲突、上传/purge 容量等全部业务准入，因此不能替代完整 CRUD 冒烟和备份恢复演练。
5. `$remote_addr` 始终是与 Dufs 建立 TCP 连接的 peer；登录限流仅在该 peer 匹配显式 `trusted-proxies` IP/CIDR 时接受恰好一个、无逗号且能解析为 IP 的 `X-Forwarded-For`。默认列表为空；未匹配、重复、多值或非法头会退回 TCP peer。受信列表只是来源地址声明，不能区分 nginx 与能直连同一回环端口的其他本机进程，仍需 OS/网络隔离。
6. CLI/YAML 的最终 bind 列表必须非空；`bind: []` 在创建 listener 或其他运行时资源前就以明确配置错误失败。多个 listener 各自先等待可读，再取得共享连接许可后 `try_accept`，因此空闲地址不会预占许可，用户态已接受 socket 不会越过上限；达到上限时内核 backlog 仍可能暂存已经完成握手的连接。
7. 成功上传任务会保留在上传队列表格中，但不会立即插入已经加载的普通目录列表，刷新页面后才会出现在常规列表。
8. 应用自定义文案为英文；浏览器原生 `Error.message` 仍可能按浏览器语言显示英文或其他语言。
9. 产品仍只承诺现代桌面浏览器，不把手机 Web 纳入支持范围；但主页面已覆盖 320 CSS 像素回流测试，确保 1280 px 桌面在 400% 缩放时不出现页面级横向滚动。窄视口把每个文件列表行回流为两行：名称和操作位于首行，修改时间和大小移到第二行并保持可见，核心浏览与文件操作仍可达。
10. 浏览器 `webkitdirectory` 不会提供空目录项，因此文件夹上传无法创建完全为空的目录；删除目录归档不改变这一上传边界。
11. move 只使用原子 rename；跨文件系统不会自动复制后删除。不同名称若是同一 dev/inode 的硬链接，覆盖预检和 commit 内复核返回 `409 source_equals_destination`，避免把 rename no-op 误报为成功。
12. 上传覆盖发布新 inode，并在单链接普通目标上保留 numeric uid/gid、除 setuid/setgid 外的权限位和允许的非特权 xattr；`security.*`、`trusted.*` 或 setuid/setgid 目标拒绝覆盖。xattr 名称列表/条目数/单值分别限制为 64 KiB/1024/64 KiB，索引、名称和按精确长度分配的全部值合计限制为 1 MiB。预检只给出存在性、可替换提示和 owner/path/完整 identity 绑定的 revision；Missing 发布用 no-replace 与发布后 identity 核对，Existing 覆盖用 revision 复核后普通 rename，后者仍受外部 writer 微窗约束。多硬链接、目录等不可替换目标以及 metadata 读取/重放失败都会失败关闭。确定的 pre-publication 失败会清理并尽力持久化 `rejected`；最终 revision 冲突则保留满 stage 为 `awaiting-confirmation`，可空 PATCH+最新 revision 发布或显式 discard。若目标消失但 stage 带旧 metadata，必须 discard 并以新 ID 完整 create-only PUT。发布后 identity、持久性或终态持久化无法确认时报告 `unknown`，不允许盲目重试。原 inode、硬链接关系和原文件时间戳不会保留。
13. 目录/搜索结果只存在当前进程内，绝对 TTL 为 120 秒并受总计 32 个/64 MiB、每账号 8 个/32 MiB 容量约束；cursor 绑定账号摘要。进程重启、跨账号复用、过期或容量淘汰都不会泄漏旧结果：跨账号复用属于无效绑定并返回 `400`，其他旧 cursor 返回 `409`，客户端必须从第一页重新开始。
14. 遍历会在访问前及结束后复核所有访问目录并在可观察变化时返回 `409`，但这不是原子文件系统快照，也不覆盖检查间发生又恢复的变化、子文件原地内容变化或最终检查后的变化。进程内路径协调同样不覆盖 shell、virtiofs 宿主或其他进程的外部写入；需要强一致读取时应使用存储快照。
15. 日志没有内置轮转、SIGHUP 重新打开或逐条 `fsync`；异常崩溃可能丢失最后一小批缓冲日志，轮转应交给 systemd/journald 或外部工具。
16. 首次停机信号提供 30 秒宽限，随后取消普通工作并仅再等待 10 秒；约 40 秒硬截止仍未完成会跳过日志 flush 并立即以状态 1 退出，即使 systemd 配置了更长超时也不会延长应用内截止。正常完成受跟踪清理后只做一次、最多 5 秒的日志 flush，再显式 `exit(0)`，不让 runtime drop 等待卡住 blocking 工作；第二信号也跳过 flush 并立即强制退出，SIGKILL 则更早终止。
17. Dufs 后端使用明文 Hyper HTTP/1 handler，接受 HTTP/1.0 和 HTTP/1.1；标准 HTTP/2 prior-knowledge connection preface 会被拒绝，也不实现 HTTP/1.1 `Upgrade: h2c`。浏览器到网关仍可使用 HTTP/2 或 HTTP/3，但生产网关固定用 HTTP/1.1 回源。
18. `upload/manager.js` 返回对象中的 `isBusy()` 当前没有生产或测试调用，可作为不改变功能的微型代码清理。
19. 固定 localhost Playwright 私钥和证书只供自动化测试，不能用于生产网关。
20. 本地发布脚本提供强制完整门禁、反复 exact-source 检查、来源隔离、源码 SHA、实际构建环境清单、固定工具生成的规范化 SBOM、第三方许可证 notice、校验和和签名制品，并拒绝 symlink、submodule 与特殊源/归档条目；环境清单记录本次工具事实但不钉扎宿主链，SBOM 规范化也不代表完整 schema 验证。项目已经提供由版本 tag 触发的远程 GitHub 便捷二进制发布，但该制品没有独立发布者签名，项目仍没有自动升级、包管理器或密钥托管；晚打开私钥不能隔离同 UID 恶意进程，管理员仍须使用独立账号、主机或 HSM。
21. Foundation 同源检查要求 `Origin`、effective Host（包括全部 Host field line 与 URI authority）和 `Sec-Fetch-Site: same-origin` 全部存在、唯一、规范且互相一致；任何缺失、重复、逗号拼接、cross-site 或歧义都失败关闭。生产网关必须列入显式受信代理、只接受固定规范主机名，以固定值覆盖上游 `Host`，并写入唯一、无逗号的 `X-Forwarded-Proto: https`；生产只接受 HTTPS，环回开发才允许 HTTP。
22. 会话 Cookie 固定为 `__Host-dufs-session; Path=/`，Cookie 不按端口隔离：同一主机名下的应用共享 host/path 作用域；若还共用 scheme 和端口，则浏览器也会把它们视为同源。同主机再部署另一份 Dufs 还会发生 Cookie 名冲突。因此 Dufs 必须独占一个主机名，并固定部署在该域名的根路径 `/`。
23. 同一共享根上的第二个 Dufs 实例会因根 fd 的非阻塞独占 `flock` 启动失败；该 advisory lock 没有 PID 文件，也不能阻止 shell、宿主机、网络文件系统另一节点或忽略 flock 的程序修改目录。多个根目录仍由多个进程分别管理。
24. 上传总 deadline 从每次 PUT/PATCH 等待路径租约前开始，覆盖准备、正文、写入、flush、metadata 重放和等待提交结果；每次 PATCH 重试重新计时。合法头解析后依次等待路径租约、尝试上传槽、受跟踪地读取 route metadata；fresh PUT 随后在同一 deadline 内分页检查目标及后代的 durable upload/purge obligations，才进入 owner checkpoint/上传准备，PATCH 不重复扫描自身会话。路径、route 或状态检查超时返回绑定的 `408 not-started`，持久状态冲突/不可用分别返回 `409/503 not-started`，槽满直接返回 `429 not-started` 且不读旧 state；这些分支都不创建 stage/SQLite 行。后续 tracked upload task 的只读准备也不等于 unknown：首次 filesystem/upload-state mutation 与总 deadline 原子竞争，deadline 先赢会关闭边界、abort 并返回 `408 not-started + retry`，边界前未处理只读 I/O 为 `408/503 not-started + retry`；task 先越界后的外层 deadline/未处理错误才保守返回 `unknown + query_upload`，后台继续持有租约和槽安全收尾。两类可重试响应都必须先 HEAD；最终 rename/fsync 进入提交后不可取消，前端不提供盲目重试。
25. mkdir、move、rename 或 DELETE 的 operation ID 在路径等待/校验前预留。已知 pre-commit 失败会记录为 `failed`，pre-commit guard 异常丢弃会移除预留并允许重试；只有越过明确 commit 边界后的异常退出才记录 `unknown`。提交可能在外层 `request-timeout` 返回 `504` 后继续；客户端用原 ID 查询一次，严格核对同一 ID 和 `running/succeeded/failed/unknown`，不自动重发。`/__dufs__/api/jobs/<uuid>` 是唯一查询路径且当前只公开 mutation operation。当前 revision 1 文件型 store 同时存储 operation/upload/purge；必填 `state-dir` 固定使用 `state.sqlite3`，只初始化空库并严格拒绝非当前格式。operation 终态 TTL 15 分钟，容量全局 4096/每账号 1024；upload TTL 7 天，容量 16384/4096；purge 容量 4096/1024，普通 I/O 故障不用 TTL 丢弃。SQLite 与文件系统没有共同事务：operation/upload 依靠 `unknown/AwaitingConfirmation` 保守恢复；purge 只有 live Ready 提交的完整 revision 才授权清理，Prepared 恢复 quarantine/release 而不猜测 rename。
26. 递归清理不再依赖 `/proc/self/fd`：工作状态只保存根内相对路径和 cursor，每片从父目录 fd 逐级执行 `openat/statat/unlinkat` 且不跟随符号链接。purge 每片最多 256 项/25 ms；普通 I/O 错误持久化回 `Ready` 并从 100 ms 指数退避到 30 秒，状态命令瞬时失败时有界保留本地 claim，重启把 `Claimed`→`Ready`。完整 trash revision 与持续 fd 锚点授权清理；每个最终候选先移入随机 disposal 名并以 fd 复核。缺失 revision、身份异常或最终 `ENOTEMPTY/EXIST` 使整棵根 quarantine/release，不从 cursor 0 重扫。未记账 orphan 只有在通道满、取消或普通 I/O 失败时保留到后续 maintenance；`InvalidData` 会把整根永久 quarantine。`.dufs-quarantine-<uuid>.hold` 排除在自动扫描之外，必须停服人工调查；恶意同 UID inotify 竞争随机名仍在威胁边界外。
27. `http/client.js` 调用 `http/response_buffer.js` 实施的 16 KiB/16 MiB 上限，是目录页 Fetch 的硬流式读取边界，不适用于浏览器原生导航或文件流式下载。分块先保留在有界 replay stream；类型化 `requestJson`/`requestNoContent` 随后直接消费该 stream，不用 `Response.clone()` 产生第二个未读 tee 分支。上传 XHR 会在响应头、下载 progress 和最终文本三个阶段拒绝超过 16 KiB 的响应，但浏览器可能在事件回调前已经内部缓冲一个网络块；因此这是客户端接受/中止边界，不是对浏览器瞬时内存分配的严格证明。
28. move 只接受已经存在的目标目录，目标不存在返回 `404 destination_directory_not_found`，目标不是目录返回 `409 destination_not_directory`；需要新的目标目录时先使用 New folder，Move 本身不会隐式创建目录。
29. 当前可访问性自动化覆盖原生控件、行内名称编辑器、页面内 `<dialog>` 的名称/标签/键盘关闭与焦点恢复、ARIA、至少 24 px 操作目标、320 CSS 像素回流及 `forced-colors: active` 下的关键边界；固定 `@axe-core/playwright 4.12.1` 还按 WCAG 2.0/2.1/2.2 A/AA 标签扫描登录页、文件页、打开的行内编辑器和操作对话框。但自动扫描仍不构成 WCAG 合规声明，项目尚未完成真实读屏和系统化人工对比度验收。
30. 管理员 username 配置必须为 3～64 个小写 ASCII 字节，首尾为字母数字、字符仅 `[a-z0-9._-]`，明确禁止 `@` 且允许相邻分隔符；登录 candidate 可为 1～64 bytes 且每字节 `0x20`～`0x7e`，服务端执行 ASCII trim/lowercase 后再验证，自动规范化绝不能用于放宽持久配置。密码字段由内联脚本 `TextEncoder` 提示，服务端权威执行 12～1024 UTF-8 字节和无 ASCII 控制字符边界。
31. 备份和恢复完全属于外部运维职责。可靠文件级备份或存储快照需要按实际语义保留 uid/gid、mode、ACL、xattr、稀疏布局、符号链接、硬链接和 Dufs 内部暂存项，并定期做恢复演练；仅复制二进制、配置或可见普通文件不能证明可恢复。
32. 发布脚本会把最终目录 rename 与输出父目录 sync 放在短暂忽略 HUP/INT/TERM 的提交窗中；普通信号不会在两步之间中断，但 SIGKILL、主机掉电或实际 sync 失败仍可能留下“完整目录已经可见、持久性尚未确认”的结果，必须重新校验制品和文件系统状态。若公开输出路径在提交期间被换绑，脚本会在事后复核中报错，但不会撤销已经提交到 fd 锁定原目录中的制品。
33. 与制品一起下载的公钥只能证明包内材料彼此自洽，不能独立建立发布者身份。生产验收必须使用通过另一可信渠道预先固定的公钥，并从独立发布记录取得预期完整 Git SHA；同源下载的归档、签名、公钥和 SHA 不能共同构成信任根。

## 18. 最值得优先判断的可选项

下表按“可能减少的代码或依赖”排序，不代表建议直接删除。

| 决策 ID | 可选能力 | 删除收益 | 明确损失 | 建议 |
| --- | --- | --- | --- | --- |
| X-02 | 当前页面断点续传 | 可删除 PATCH、上传状态 HEAD、SQLite 上传会话、7 天 TTL 和大量故障分支 | 网络失败后大文件必须从头上传 | 只传小文件或网络极稳定时评估 |
| X-03 | 递归搜索 | 可删除搜索 UI、递归匹配和搜索条目上限；直接分页列表仍保留 | 只能逐级寻找文件 | 目录结构固定且不大时评估 |
| X-04 | 多管理员 username | 可把配置收敛为单个管理员 username，但唯一 `admin` 角色、会话和 CSRF 仍必须保留 | 多位操作者不能使用独立身份，日志无法区分人，也无法只撤销某一管理员的会话 | 确认永远只有一位操作者时评估 |
| X-05 | 条件请求 | 可减少 ETag、日期和四类前置条件分支 | 标准客户端不能再做版本前置条件；附件 MIME 内容抽样已经移除 | 当前先保留 |
| X-06 | 自定义日志格式和日志文件 | 可删除格式解析、动态请求头变量和文件 sink；保留固定安全访问日志 | 不能定制字段或直接写文件 | 全部使用 journald 固定格式时评估 |
| X-07 | YAML 配置 | 可删除 YAML 解析和一个直接依赖 | systemd 命令行变长，密码哈希更容易出现在进程启动配置中 | 通常建议保留 |
| X-09 | 多监听地址和 IPv6 | 可把 bind 收敛为单个 IPv4 地址 | 失去 IPv6 和多网卡同时监听 | 仅固定单一回环或内网 IPv4 地址时可评估 |
| X-11 | 单段 Range | 可减少下载协议分支 | 大文件暂停、恢复和部分读取能力下降 | 对大文件管理通常建议保留 |
| X-12 | 文件夹选择器 | 只删除一个前端入口和对应测试，后端深层上传仍在 | 不能一次选择整个目录树；空目录本来就不保留 | 只上传单文件时可评估 |
| X-13 | 新建空文件 | 删除一个按钮和零字节调用入口 | 不能直接建立占位文件 | 可按使用习惯决定 |
| X-15 | liveness/readiness | 删除两个小路由和测试 | 网关失去无凭据 liveness，受控冒烟也不能检查根目录、空间和 state store 线程健康 | 删除收益很低 |
| X-16 | 深色样式 | 删除少量 CSS | 深色系统下体验下降 | 纯外观选择 |
| X-18 | favicon | 删除一个图标资源和少量嵌入/摘要代码 | 浏览器标签页没有项目图标 | 纯外观，收益极低 |

## 19. 不建议作为“精简功能”删除的项目

以下项目看起来代码较多，但它们不是附加功能，而是现有文件能力成立的条件：

- 账号认证、会话、CSRF、同源检查和安全 Cookie；
- 根 fd、`openat2`、根外符号链接拒绝和内部保留路径；
- 上传 `sync_all + rename + fsync` 提交顺序；
- move 的原子不覆盖和父目录同步；
- 删除的原子隐藏、父目录同步和遗留清理；
- 路径协调器及符号链接语义键；
- 连接、请求、上传、遍历和磁盘预算；
- 有界分页与 blocking 文件系统任务；
- 私有 `no-store` 和动态 DOM 安全边界；
- 优雅停机时等待已经开始的持久化提交；
- 公开错误与内部诊断分离。

删除这些内容可能减少代码行数，却会让同一个按钮在并发、断电、网关取消、符号链接或磁盘不足时产生不可信结果。

## 20. 功能与直接依赖关系

下表只列生产直接依赖。一个 crate 可能同时服务多个功能，删除单项后不一定能立刻从 `Cargo.toml` 移除。

| 功能组 | 主要源码 | 主要直接依赖 | 取舍说明 |
| --- | --- | --- | --- |
| CLI 与参数 | `src/args.rs`、`src/main.rs` | `clap` | 所有运行模式都需要；只可减少具体参数 |
| YAML | `src/args.rs` | `serde_yaml_ng`、`serde` | 删除 X-07 后可移除 YAML 运行依赖 |
| 登录与密码 | `src/auth.rs`、`src/server/session.rs`、`src/server/login_rate_limit.rs` | `argon2`、`rpassword`、`getrandom`、`subtle` | `rpassword` 主要用于 hash-password；其他部分属于认证、会话或登录限流核心 |
| 会话、摘要和编码 | `src/auth.rs`、`src/server/assets.rs`、`src/server/listing.rs`、`src/server/listing/snapshot.rs`、`src/server/operation_registry.rs`、`src/server/upload.rs`、`src/server/upload/record.rs` | `sha2`、`base64` | 摘要与 Base64 同时用于会话/账号、资源、页面上下文、抗篡改 cursor、operation 指纹或上传内部名称；固定小写十六进制编解码由 `utils.rs` 的有测试小函数完成，不再引入 `hex` |
| 统一控制状态 | `src/server/operation_registry.rs`、`src/server/state_store.rs`、`src/server/upload/record.rs`、`src/server/purge.rs` | `rusqlite`（bundled SQLite） | 当前 revision 1 文件数据库同时持久化管理 operations/upload_sessions/purge_jobs；SQLite 是唯一状态权威，`state-dir` 必填，不存在内存模式，服务不迁移旧格式 |
| HTTP 服务 | `src/main.rs`、`src/server.rs`、`src/server/router.rs`、`src/server/assets.rs` | `tokio`、`tokio-util`、`hyper`、`hyper-util`、`http-body-util`、`headers`、`bytes`、`futures-util` | 核心运行栈；下载以 `StreamBody` 驱动受全局门控的 fd-relative 分块读取，每次门控等待与读取共用 30 秒源 idle deadline；`tokio-util` 只启用任务生命周期所需的 `rt`，`hyper-util` 只提供 Tokio I/O 和计时适配，生产依赖图不包含 `h2` |
| TCP 监听 | `src/main.rs` | `socket2` | 用于 Linux listener 配置和 backlog |
| Linux 系统边界与会话计时 | `src/args.rs`、`src/auth.rs`、`src/server/rooted_fs.rs`、`src/server/rooted_fs/purge.rs`、`storage.rs`、`disk_space.rs` | `rustix` | 配置 ACL 探测使用 fd-relative xattr；`CLOCK_BOOTTIME` 让会话期限包含系统休眠；`openat2`、`*at`、`fsync`、fd-relative 递归删除、`fstatvfs` 等构成文件系统边界 |
| 路由和表单编码 | `src/server.rs`、`src/server/router.rs`、`session.rs`、前端 URL | `percent-encoding`、`form_urlencoded` | 登录、查询和路径编码共同使用 |
| 浏览器 JSON 协议 | `browser_api.rs`、`listing.rs`、`listing/snapshot.rs`、`operation_registry.rs`、`upload.rs`、`upload/record.rs` | `serde`、`serde_json` | mkdir/move/rename、分页结果、operation 状态和上传状态都使用 |
| 文件类型判断 | `src/server/download.rs` | `mime_guess` | 附件只按扩展名给出 MIME，未知名称使用 octet-stream；不再抽样或猜测 charset |
| 目录排序 | `src/server/listing.rs` | `alphanumeric-sort` | 删除排序或改用普通字符串比较后可移除 |
| 上传、operation 和内部名称 | `src/server/upload.rs`、`src/server/upload/{prepare,target,transfer,commit,failure,protocol,record}.rs`、`src/server/{internal_names,maintenance,operation_registry,rooted_fs}.rs` | `uuid` | 上传/operation ID 来自浏览器，服务端 UUID 还用于暂存状态和删除 trash |
| 日志 | `src/logger.rs`、`src/http_logger.rs` | `log`、`chrono` | 删除自定义格式不等于能删除基本日志依赖 |
| 错误传递 | 全部 Rust 模块 | `anyhow` | 公开错误由 `AppError` 隔离，内部诊断广泛使用 |

## 21. 可直接回复的决策模板

可以按下面的 ID 回复，例如：“删除 X-15；保留其余；X-02 先不动。”之后再针对选中的组合检查依赖关系并实施。目录 ZIP 已经删除，不再列为待决选项。

- [ ] X-02 删除当前页面断点续传，只保留完整 PUT
- [ ] X-03 删除递归搜索
- [ ] X-04 收敛为单账号
- [ ] X-05 删除条件请求（MIME/字符集内容探测已移除）
- [ ] X-06 固定日志格式并删除日志文件输出
- [ ] X-07 删除 YAML，只保留命令行
- [ ] X-09 删除多地址和 IPv6，只保留单 IPv4 bind
- [ ] X-11 删除单段 Range
- [ ] X-12 删除文件夹选择上传
- [ ] X-13 删除新建空文件
- [ ] X-15 删除 liveness 与 readiness
- [ ] X-16 删除深色样式
- [ ] X-18 删除 favicon

在确定其余取舍前，建议先说明实际文件大小、是否经常上传目录、账号数量，以及日志是否全部交给 systemd/journald。这些信息会直接改变 X-02、X-04、X-06 和 X-12 的最优选择。
