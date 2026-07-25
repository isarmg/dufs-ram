# Dufs 浏览器文件管理器

Dufs 是一个使用 Rust 编写的轻量级浏览器文件管理器。启动单个 Linux 可执行文件后，即可通过 Edge、Firefox 等现代桌面浏览器浏览和管理指定目录，不需要单独部署前端服务。

本项目面向个人或受控局域网环境。一台系统只运行一个 Dufs 实例，并由该实例管理一个共享根目录。账号只决定能否登录；创建账号后即拥有整个共享根的完整文件管理权限。

## 支持范围

- 浏览目录并按名称、修改时间或大小排序；
- 下载文件、单段 Range 断点下载和目录 ZIP；
- 通过文件选择器上传文件，通过文件夹选择器上传目录；
- 显示上传速度、进度和预计剩余时间；
- 当前页面内的失败任务可校验服务端持久化检查点并继续上传；
- 新建文件和目录，移动、重命名及删除文件或目录；
- 从当前目录开始按文件名递归搜索；
- Argon2id 密码、中文登录页、服务端会话和 CSRF 防护；
- 隐藏指定名称、资源预算和异步访问日志；
- 编译内置的原生 HTML、CSS、JavaScript 管理页面。

本项目仅支持 Linux 服务端和现代桌面浏览器，不提供匿名访问、账号分级权限、手机 Web、WebDAV、无 JavaScript 客户端、拖放上传、在线预览或编辑、静态网站托管、运行时页面资源覆盖、Unix socket、CORS 或环境变量配置。

## 文档分工

- [项目工作流程与流程树](docs/project-workflow.md)：说明当前代码的启动、认证、浏览、上传、下载、持久化和停机流程；
- [十项优化 TODO 与完成记录](docs/browser-only-optimization-review.md)：本轮十项质量优化的实现与验证依据；
- [本地变更记录](CHANGELOG.md)：记录从 0.46.0 起已经完成的改动及更早版本历史。

本项目只使用本地 Git，不发布到 GitHub。

## 环境要求

- 64 位 Linux，且内核必须提供 `openat2`；不支持时程序会拒绝启动；
- Rust、rustc 和 Cargo 1.97.1，源码使用 Rust 2024 edition；
- 建议使用 rustup；`rust-toolchain.toml` 已固定工具链并包含 Clippy、Rustfmt；
- Node.js 18 或更高版本仅用于运行前端自动化测试。

`build.rs` 会在编译期拒绝非 Linux 目标。`Cargo.lock` 中出现上游依赖的其他平台条件包属于 Cargo 的完整依赖图，不表示本项目支持这些平台。

## 编译

```sh
cargo build --release
```

生成的可执行文件位于：

```text
target/release/dufs
```

也可以直接从当前本地源码安装：

```sh
cargo install --path .
```

## 快速开始

先生成密码哈希：

```sh
./target/release/dufs hash-password
```

再把下面的 `$argon2id$…` 替换为命令输出的完整 PHC 字符串：

```sh
./target/release/dufs \
  -p 5000 \
  -a 'admin:$argon2id$…' \
  /需要管理的目录
```

未指定 `--bind` 时，Dufs 默认监听 `0.0.0.0:5000`，即本机全部 IPv4 网络接口。需要 IPv6 时可显式使用 `--bind ::`；网关与 Dufs 位于同一台主机时，建议显式使用 `--bind 127.0.0.1`。

浏览器会话 Cookie 带有 `Secure` 属性，因此浏览器入口必须使用 HTTPS。通常应在浏览器中打开网关提供的地址：

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
  /需要管理的目录
```

使用要求：

- 密码必须是 `dufs hash-password` 生成的完整 Argon2id PHC 字符串；
- 哈希包含 `$`，在 Shell 中应使用单引号；
- 重复用户名和非 Argon2id 格式会阻止启动；
- 每个账号均可浏览、上传、覆盖、移动、删除及搜索整个共享根；
- 登录成功后使用服务端内存会话；空闲 30 分钟或创建满 12 小时后失效；
- 程序重启会清空会话，浏览器需要重新登录；
- 登录、注销和文件写入均受同源及 CSRF 检查保护。

## 浏览器文件管理

### 浏览、下载与搜索

- 点击目录名进入下一级目录；
- 点击文件名或下载按钮下载文件；
- 点击目录下载按钮生成并下载 ZIP；
- 点击表头按名称、修改时间或大小排序；
- 搜索框从当前目录开始递归匹配文件名。

普通文件始终作为附件下载，不提供在线预览或编辑。服务只支持单段 Range，多段 Range 返回 `416`。目录列表、搜索和 ZIP 只接受 UTF-8 文件名；遇到非 UTF-8 Linux 文件名时整个操作会失败，需要先在系统侧重命名。

目录页只返回骨架，文件项通过受认证的分页 API 每次最多加载 500 项，默认 200 项；翻页期间由 Dufs 引起的目录项新增、删除或替换会要求从第一页重新加载。目录游标不监控 shell、virtiofs 宿主机等外部进程对现有子文件进行的原地内容或 metadata 修改。目录 ZIP 会先在服务端私有临时文件中完整生成，成功后才开始响应，因此生成失败不会向浏览器返回截断的成功归档。ZIP 的源文件总量按实际读取字节计算，临时归档另有独立输出上限，并在每次写入前保护最低剩余磁盘空间。

### 上传与续传

- “上传”可选择一个或多个文件；
- “文件夹上传”会保留所选目录中的相对路径，但不会创建空目录；
- 页面显示每个任务的速度、进度、预计剩余时间和最终结果；
- 当前页面内上传失败后，“重试”会先核对同一上传任务的服务端检查点，再从已持久化偏移继续；
- 页面刷新后不会自动恢复旧任务，也不会读取 `localStorage` 续传记录；重新选择文件会创建全新上传 ID，避免仅凭文件名、大小和时间戳把不同内容拼接在一起；
- 拖入文件只会被阻止触发浏览器导航，不会开始上传。

服务端不会直接改写最终文件。只有完成文件同步、同文件系统原子发布和目标父目录同步后才返回成功；因此在受支持的 Linux 本地文件系统及存储正确兑现同步请求的前提下，成功响应表示文件已经按崩溃持久化语义提交。强制断电、介质损坏、错误实现同步语义的存储以及后续位腐败仍需要可靠存储和备份处理。

上传协议、检查点、内部暂存、任务取消和目录同步的完整步骤见[项目工作流程](docs/project-workflow.md#9-持久化上传与断点续传)。

### 新建、移动与删除

- 可以新建空文件和目录；
- 移动与重命名共用一个操作；
- 不允许覆盖时使用原子不替换语义，并发出现同名目标会返回冲突；
- 共享根目录本身不能删除；
- 删除会先持久化移除原名称，再异步回收磁盘空间；
- 内部删除暂存项不是回收站，不提供恢复或撤销功能。

解析后仍位于共享根内的相对符号链接可以使用。绝对符号链接和指向根外的符号链接会被隐藏并拒绝访问。需要管理其他磁盘或 virtiofs 导出目录时，应先将其挂载到共享根内。

### 文件系统大小写说明

Dufs 按 Linux 大小写敏感语义处理路径、路径租约和内部暂存名称。共享根建议使用未启用目录 casefold（`+F`）的 ext4；使用 virtiofs 时，宿主导出目录也应能区分仅大小写不同的名称。若底层目录不区分大小写，`Foo` 与 `foo` 可能指向同一对象；本项目不会检测、拒绝或兼容这种挂载，也不会为此增加特殊代码。

## 命令行参数

```text
用法：dufs [选项] [共享目录]
```

| 参数 | 说明 |
| --- | --- |
| `[serve-path]` | 要管理的现有目录，默认当前目录；非目录会拒绝启动 |
| `-c, --config <file>` | YAML 配置文件 |
| `-b, --bind <ips>` | 监听一个或多个 IPv4/IPv6 地址，默认 `0.0.0.0` |
| `-p, --port <port>` | 监听端口，默认 `5000` |
| `--path-prefix <path>` | URL 路径前缀 |
| `--hidden <value>` | 隐藏匹配的文件或目录名 |
| `-a, --auth <account>` | 添加一个拥有完整共享目录权限的账号 |
| `--log-format <format>` | 自定义 HTTP 访问日志格式 |
| `--log-file <file>` | 将日志写入文件 |
| `--compress <level>` | ZIP 压缩级别：`none/low/medium/high` |
| `--max-upload-size <bytes>` | 单文件最大声明长度，默认 100 GiB |
| `--upload-idle-timeout <seconds>` | 上传正文最大空闲时间，默认 60 秒 |
| `--upload-total-timeout <seconds>` | 单次上传总时限，默认 24 小时 |
| `--max-concurrent-uploads <count>` | 同时上传数，默认 4 |
| `--min-free-space <bytes>` | 上传和 ZIP 临时文件写入期间必须保留的可用空间，默认 1 GiB |
| `--max-connections <count>` | 活跃 TCP 连接上限，默认 256 |
| `--max-search-entries <count>` | 单次搜索最多检查的目录项，默认 10000 |
| `--max-zip-entries <count>` | 单次 ZIP 遍历最多检查的目录项，默认 10000 |
| `--max-zip-uncompressed-size <bytes>` | ZIP 源文件未压缩总量上限，默认 10 GiB |
| `--max-zip-output-size <bytes>` | 单个 ZIP 临时归档的实际输出上限，默认 10 GiB |
| `--max-concurrent-searches <count>` | 同时执行的列表或搜索数，默认 2 |
| `--max-concurrent-zips <count>` | 同时生成或向客户端发送的目录 ZIP 数，默认 1 |
| `--request-timeout <seconds>` | 普通请求处理并生成响应头的时限，默认 300 秒；不包含响应头发出后的文件流传输 |
| `-h, --help` | 显示帮助 |
| `-V, --version` | 显示版本 |

`--bind` 只接受 IP 地址，可以重复使用或用逗号分隔；主机名、文件路径和 Unix socket 路径会被拒绝。命令行参数覆盖 YAML 中的值。

`--request-timeout` 在响应头生成完成时结束计时。普通文件、单段 Range 和已经生成完成的 ZIP 正文传输不受该计时器限制；它们仍受活跃连接上限约束，目录 ZIP 还会一直占用 ZIP 并发槽位，直到正文发送完成、失败或被客户端取消。

上传和 ZIP 通过按 Linux `st_dev` 分桶的共享记账保护 `--min-free-space`：同一文件系统上的两类操作联合计算预留，不同文件系统互不影响。该保证覆盖 Dufs 进程内部并发；外部进程、virtiofs 宿主机或存储侧变化仍可能竞争空间，生产配置应保留额外余量。

## YAML 配置

```yaml
serve-path: /需要管理的目录
bind:
  - 127.0.0.1
port: 5000
path-prefix: files
hidden:
  - .git
  - .DS_Store
  - '*.tmp'
  - '*.lock'
auth:
  - 'admin:$argon2id$…'
log-format: '$time_iso8601 $remote_addr "$request" $status'
log-file: ./dufs.log
compress: low
max-upload-size: 107374182400
upload-idle-timeout: 60
upload-total-timeout: 86400
max-concurrent-uploads: 4
min-free-space: 1073741824
max-connections: 256
max-search-entries: 10000
max-zip-entries: 10000
max-zip-uncompressed-size: 10737418240
max-zip-output-size: 10737418240
max-concurrent-searches: 2
max-concurrent-zips: 1
request-timeout: 300
```

启动：

```sh
./target/release/dufs --config ./dufs.yaml
```

设置 `path-prefix: files` 后，浏览器地址为：

```text
https://files.example.com/files/
```

YAML 会拒绝未知字段。生产配置只来自命令行和 YAML，Dufs 不读取 `DUFS_*` 环境变量。

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

默认的 `0.0.0.0` 适合需要通过服务器 IPv4 地址接入的部署，但会让所有本机 IPv4 接口接受连接。应使用主机防火墙只允许网关访问该端口。若网关与 Dufs 同机，可把暴露面收窄到回环地址：

```sh
./target/release/dufs \
  -b 127.0.0.1 \
  -p 5000 \
  -a 'admin:$argon2id$…' \
  /需要管理的目录
```

网关配置要求：

- 浏览器只访问网关提供的 HTTPS 地址，不能绕过网关直连后端；
- Dufs 后端只提供 HTTP；HTTPS 证书、TLS 协议和公网安全策略全部由网关负责；
- 保留原始 `Host`，并正确转发路径前缀，否则同源检查会失败；
- 不缓存登录、认证文件、Range、ZIP、上传、API 或错误响应；
- 保留上游的 `Cache-Control: private, no-store`；
- 按可信的真实客户端 IP 限制登录速率；
- 强制把 HTTP 入口重定向到 HTTPS，并在确认域名只提供 HTTPS 后启用 HSTS；
- 最好使用独立主机名，不与不可信应用共享同一主机名。

本项目不再提供内置 TLS。若网关不在同一主机，必须使用隔离私网、主机防火墙或等效 ACL，避免其他设备绕过 HTTPS 网关直接访问后端端口。

## 隐藏规则

```sh
./target/release/dufs \
  -a 'admin:$argon2id$…' \
  --hidden '.git,.DS_Store,*.log,*.lock' \
  /需要管理的目录
```

隐藏规则只匹配文件名或目录名，不匹配完整路径。隐藏仅影响目录列表和搜索，不是访问控制；需要隔离的内容不应放入共享根。

## 访问日志

常用变量：

| 变量 | 含义 |
| --- | --- |
| `$remote_addr` | 与 Dufs 建立 TCP 连接的客户端地址；经网关时通常是网关地址 |
| `$remote_user` | 已成功认证的用户名；未认证或认证失败时为 `-` |
| `$request` | 完整请求行 |
| `$status` | HTTP 状态码 |
| `$http_...` | 请求头，例如 `$http_user_agent` |

Authorization、Proxy-Authorization、Cookie 和 CSRF 请求头会在自定义日志变量中脱敏。连接处理错误会记录 TCP peer、错误类别和系统错误码，便于定位网关 `502`、超时和协议问题。

示例：

```sh
./target/release/dufs \
  -a 'admin:$argon2id$…' \
  --log-format '$time_iso8601 $remote_addr $remote_user "$request" $status' \
  --log-file ./dufs.log \
  /需要管理的目录
```

设置 `--log-format=''` 可以关闭 HTTP 访问日志。

## 停止服务与 systemd

首次收到 SIGINT 或 SIGTERM 时，Dufs 会停止接受新连接，并给予普通任务 30 秒宽限；随后取消可取消任务，同时继续等待上传及已经开始的文件系统提交安全收尾。第二次停止信号和 SIGKILL 会立即越过等待，不能保证正在处理的写入已经落盘。

systemd 的停止超时必须大于 30 秒，并为最慢的存储同步和大目录清理留出余量：

```ini
[Service]
TimeoutStopSec=45s
KillSignal=SIGTERM
```

`45s` 只是起点，应按实际目录规模和存储性能调大。

## 内置页面

`assets/` 中的 HTML、CSS、JavaScript 和图标会在编译期固定写入可执行文件。运行时不读取外部页面目录，也不支持自定义 `404.html`。资源内容变化会生成新的摘要 URL，只有已知版本化资源使用长期缓存。

生产构建不需要 Node.js 或前端打包步骤；Node.js 只用于 Playwright 测试。

## 本地检查

确认工具链：

```sh
rustc --version
cargo --version
```

Rust 检查：

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo audit
```

首次准备前端测试：

```sh
npm ci
npm run test:frontend:install
```

运行桌面浏览器自动化测试：

```sh
npm run test:frontend
npm audit --audit-level=high
```

当前 Playwright 必需矩阵覆盖 Chromium 和 Firefox；已安装 Microsoft Edge 时可执行 `npm run test:frontend:edge`。测试通过本地 HTTPS 网关转发到 Dufs 的 HTTP 动态端口，与生产部署边界一致。`tests/data/key_pkcs8.pem` 是公开、固定且仅供 localhost 自动化使用的测试私钥，绝不能作为生产网关密钥部署。

完整本地检查可使用：

```sh
./scripts/check.sh
```

提交前还应执行：

```sh
git diff --check
git status --short
```

本项目只提交到本地 Git。创建版本时应先确认工作树完整、检查全部通过，再创建与 Cargo 版本一致的本地 tag。

## 目录结构

```text
.
├── assets/                         # 编译内置的浏览器页面源码
├── docs/
│   ├── project-workflow.md         # 当前实现流程与 Mermaid 流程树
│   └── browser-only-optimization-review.md
├── src/
│   ├── main.rs                     # 启动、监听和连接生命周期
│   ├── args.rs                     # 命令行与 YAML 配置
│   ├── auth.rs                     # 账号、会话与 CSRF
│   ├── server.rs                   # 服务入口与公共路由
│   └── server/
│       ├── browser_api.rs          # 新建目录、移动和重命名
│       ├── disk_space.rs           # 按文件系统联合计算上传与 ZIP 空间
│       ├── download.rs             # 文件下载、MIME 与 Range
│       ├── listing.rs              # 目录、搜索与 ZIP
│       ├── path_coordinator.rs      # 进程内路径写租约
│       ├── rooted_fs.rs            # 共享根 fd 与 Linux 文件操作
│       ├── session.rs              # 登录、注销与写请求校验
│       ├── storage.rs              # 可注入的持久化提交边界
│       └── upload.rs               # 上传、检查点与维护
├── tests/
│   ├── frontend/                   # 按职责拆分的 Playwright 测试
│   └── *.rs                        # Rust 集成测试
├── Cargo.toml
├── Cargo.lock
├── package.json
├── playwright.config.js
└── rust-toolchain.toml
```
