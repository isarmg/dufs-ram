# 02. 环境准备与第一次运行

本章先跑通一个隔离的开发实例，再解释每个参数的作用。不要一开始就把真实照片、工作文档或生产目录交给正在学习的实例。

## 2.1 平台要求

构建源码需要：

- 唯一目标平台是 Linux AMD64 GNU，即 `x86_64-unknown-linux-gnu`；aarch64、ARM64、musl、Windows 和 macOS 均在编译期拒绝；
- Rust、rustc、Cargo 1.98.0；
- 源码使用 Rust 2024 edition；
- 与目标 ABI 匹配的 C 编译器、链接器和 binutils；项目通过 `rusqlite` 的 bundled feature 编译 SQLite C 源码。

运行已经构建好的二进制不需要 Rust 工具链，但制品必须匹配目标机器的 CPU 架构和 libc/动态加载器 ABI，内核还必须提供可用的 `openat2`。因此不能把某个 GNU libc 的 x86-64 制品笼统视为“所有 64 位 Linux 都能运行”。

[build.rs](../../build.rs) 会在编译期拒绝任何非 `x86_64-unknown-linux-gnu` 目标。启动时还会探测 `openat2`；精确 target 能编译仍不代表当前内核具备运行所需系统调用。

Node.js 精确版本 24.8.0 用于 JavaScript 安全/类型检查、前端单元测试、文档检查、Playwright/部署测试和发布辅助脚本；正式质量门会拒绝其他 Node 版本。生产服务器运行已经构建好的 Dufs 二进制时不需要 Node.js。

## 2.2 先检查工具

```sh
rustc --version
cargo --version
git --version
cc --version
ld --version
ar --version
node --version
npm --version
curl --version
rg --version
ss --version
```

`cc`、`ld` 和 `ar` 分别代表 C 编译器及链接/binutils 工具；SQLite bundled 源码构建需要它们。后续示例还假定存在 curl、ripgrep 的 `rg`、iproute2 的 `ss`，以及 GNU coreutils 提供的 `mktemp`、`install`、`stat` 和 `rm --one-file-system`。只编译后端时 Node/npm 可以暂缺，但浏览器测试和前端门禁需要它们。

仓库的 [rust-toolchain.toml](../../rust-toolchain.toml) 会让 rustup 自动选择固定工具链。若缺少工具链，rustup 可能需要联网下载。

首次 Cargo 构建可能要从 crates.io 下载未缓存依赖；`npm ci` 可能访问 npm registry，Playwright 安装命令还会下载浏览器。离线环境需要事先准备对应 toolchain、依赖缓存或 vendored 依赖和浏览器制品。依赖审计的联网条件见[第 8 章](08-testing-debugging-and-change-workflow.md#810-完整-checksh-与干净工作树)。

前端依赖按锁文件安装：

```sh
npm ci
```

如果你暂时只编译并运行后端，可以先不执行 `npm ci`；前端源码已经在仓库中，不需要打包步骤。

## 2.3 编译

开发构建：

```sh
cargo build --locked
```

输出位于：

```text
target/debug/dufs
```

发布构建：

```sh
cargo build --release --locked
```

输出位于：

```text
target/release/dufs
```

`--locked` 要求 Cargo 严格使用 [Cargo.lock](../../Cargo.lock)，避免一次普通构建意外改变依赖解析结果。
其中 Foundation 的 `sarmg-admin-auth`、`sarmg-contracts`、`sarmg-schema-identity`、`sarmg-server-target`
都必须是 `=0.3.0`，Git rev 都必须是 `1fe326081cfd896f05ff502e80f99504797c14c6`。即使只是开发联调，也不能
改成相邻工作区、Cargo path dependency、可变 branch 或本地复制代码；那会让同一 Dufs 提交产生不同的认证、
Schema 或正式 target 合同。

## 2.4 准备隔离目录

创建两个互不包含的目录：

```sh
tutorial_root="$(mktemp -d /tmp/dufs-learning.XXXXXXXX)" &&
chmod 0700 "$tutorial_root" &&
mkdir "$tutorial_root/shared" &&
install -d -m 0700 "$tutorial_root/state" &&
printf 'hello from dufs\n' > "$tutorial_root/shared/hello.txt" &&
mkdir "$tutorial_root/shared/examples" &&
printf 'Tutorial root: %s\n' "$tutorial_root"
```

后续命令沿用当前 shell 中的 `$tutorial_root`，不要关闭这个终端。`mktemp -d` 产生不可预测且仅当前用户可进入的目录，避免在多人机器的 `/tmp` 下沿用一个可能已被预置或换成符号链接的固定名称。

这里：

- `shared` 是浏览器能管理的共享根；
- `state` 是私有状态目录；
- `0700` 表示只有当前 Linux 用户可读、写和进入状态目录。

正式部署时，状态目录应由专用服务账号拥有，并放在共享根之外的稳定磁盘位置。`/tmp` 只适合本章实验。

## 2.5 生成登录密码哈希

执行：

```sh
target/debug/dufs hash-password
```

命令会要求输入两次密码，输出形如：

```text
$argon2id$v=19$m=...$...$...
```

这是 PHC 格式的 Argon2id 密码哈希，不是原始密码。原始密码必须为 12～1024 个 UTF-8 字节，且不能包含 ASCII 控制字符。

把完整账号值写入 YAML 时建议使用单引号：

```text
'admin:$argon2id$v=19$...'
```

冒号左侧是 canonical 管理员 username：3～64 个 lowercase ASCII 字节，首尾为字母数字，中间只允许字母、数字、`.`、`_`、`-`；不能包含 `@`。不要把真实密码、会话 Cookie、CSRF token 或完整生产 PHC 放进 Git、截图或公开日志。

## 2.6 启动最小实例

先复制受保护配置模板，把其中的共享根、状态目录和账号 PHC 替换为本章生成的真实值，再启动：

```sh
cp config/dufs.yaml.example "$tutorial_root/dufs.yaml"
chmod 0600 "$tutorial_root/dufs.yaml"
# 用编辑器替换路径、PHC，并为本章的小型临时卷设置 min-free-space: 0
target/debug/dufs --config "$tutorial_root/dufs.yaml"
```

`--min-free-space 0` 只为了避免很小的临时测试卷达不到默认 1 GiB 余量；不要把它不加思考地复制到生产配置。

成功后会输出类似：

```text
Listening on http://127.0.0.1:5000/
```

这表示后端已监听，并不表示生产浏览器入口应该使用明文 HTTP。会话 Cookie 带 `Secure`，前端上传 ID 使用安全上下文中的 `crypto.randomUUID()`，因此完整浏览器使用需要 HTTPS 网关。

## 2.7 先用健康接口验证

另开终端：

```sh
curl --noproxy '*' --connect-timeout 2 --max-time 10 \
  -i http://127.0.0.1:5000/__dufs__/health
```

预期状态是 `200 OK`。该接口是公开 liveness，只证明进程还能处理 HTTP。

readiness 需要认证：

```sh
curl --noproxy '*' --connect-timeout 2 --max-time 10 \
  -i http://127.0.0.1:5000/__dufs__/ready
```

没有会话时收到 `401` 是正常的；这条未认证命令只证明 ready 受到保护，**不会执行 readiness 探针**。带有效会话请求 ready 时，服务才会实际验证共享根的创建、写入、文件同步、删除、目录同步，以及 SQLite 的写事务能力和可用空间。下面的本地 HTTPS 示例给出完整验证命令。

这里显式使用 `--noproxy '*'`，避免机器上的 `HTTP_PROXY`/`HTTPS_PROXY` 和不完整的 `NO_PROXY` 把回环测试送到外部代理；连接和总时限则避免诊断命令无限等待。

## 2.8 浏览器为什么要经过 HTTPS

推荐拓扑是：

```text
浏览器 --HTTPS--> nginx --HTTP/1.1 回环连接--> Dufs
```

nginx 负责：

- TLS 证书和 HTTPS；
- 固定可信域名；
- 用固定 Host 和受控代理头把请求转发给回环后端；
- 登录路由的额外速率和连接限制；
- 大文件请求体与发送超时边界。

URI 和业务路径是否规范由 Dufs 自己校验；nginx 样例不会替后端证明文件路径安全。

仓库提供 [nginx 示例](../../deploy/nginx-dufs.conf) 和 [代理头片段](../../deploy/dufs-proxy.conf)。测试环境则由 [tests/frontend/server.mjs](../../tests/frontend/server.mjs) 建立本地 HTTPS 代理，并在启动后端时显式把 `127.0.0.1/32` 配为受信代理；生产部署必须按真实网关地址设置同一边界。

不要把测试证书直接用于生产，也不要为了省事删除 `Secure` Cookie 或来源校验。

### 最快的本地浏览器体验

如果只是学习界面，可以直接使用仓库的隔离测试服务器：

```sh
cargo build --locked
DUFS_FRONTEND_TEST_PORT=9443 node tests/frontend/server.mjs
```

这里的网关脚本只使用 Node.js 内置模块，所以仅手工浏览不需要先执行 `npm ci` 或下载 Playwright 浏览器。要运行自动化前端测试时，才按[第 8 章](08-testing-debugging-and-change-workflow.md#82-首次准备测试环境)安装锁定的 npm 依赖和 Playwright 浏览器。

然后打开：

```text
https://127.0.0.1:9443/
```

测试登录信息是：

```text
username: frontend-test-0
password: test-password
```

浏览器会因为公开的自签名测试证书显示警告，这是本地测试预期行为。该脚本会创建临时共享根、临时状态目录、多个测试账号和示例文件，在 `Ctrl+C` 后停止后端并清理临时数据。仓库中的测试证书、私钥和固定账号只能用于这个隔离场景，绝不能复制到生产。

在测试服务器仍运行时，可以另开终端登录并真正调用 readiness：

```sh
(
  set -eu
  cookie_dir="$(mktemp -d /tmp/dufs-local-ready.XXXXXXXX)"
  case "$cookie_dir" in
    /tmp/dufs-local-ready.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]) ;;
    *) printf 'Unexpected temporary path: %s\n' "$cookie_dir" >&2; exit 1 ;;
  esac
  trap 'rm -rf -- "$cookie_dir"' EXIT
  curl -k --noproxy '*' --connect-timeout 2 --max-time 30 \
    --silent --show-error \
    --cookie-jar "$cookie_dir/cookies" \
    --header 'Content-Type: application/json' \
    --header 'Origin: https://127.0.0.1:9443' \
    --header 'Sec-Fetch-Site: same-origin' \
    --data '{"username":"frontend-test-0","password":"test-password"}' \
    --output /dev/null \
    https://127.0.0.1:9443/api/v2/auth/login
  curl -k --noproxy '*' --connect-timeout 2 --max-time 30 \
    --fail --silent --show-error \
    --cookie "$cookie_dir/cookies" \
    https://127.0.0.1:9443/__dufs__/ready
)
```

预期正文是 `{"status":"ready"}`。这里的 `-k` 只为接受仓库自签名测试证书，不能作为生产 TLS 校验方式。

## 2.9 用 YAML 保存配置

第 2.6 节已经复制了配置模板；若需要重新开始，可再次复制并立即收紧权限：

```sh
cp config/dufs.yaml.example "$tutorial_root/dufs.yaml"
chmod 0600 "$tutorial_root/dufs.yaml"
```

核心结构如下：

```yaml
serve-path: /tmp/dufs-learning.ABCDEFGH/shared
state-dir: /tmp/dufs-learning.ABCDEFGH/state
bind:
  - 127.0.0.1
trusted-proxies:
  - 127.0.0.1/32
port: 5000
auth:
  - 'admin:$argon2id$v=19$...'
min-free-space: 0
```

上面的 `ABCDEFGH` 和 PHC 都是占位符。编辑复制出的 YAML 时必须完成三项替换：把两个路径改成第 2.4 节打印的**同一个真实 `$tutorial_root` 值**，把 `auth` 改成第 2.5 节生成的完整 Argon2id PHC，并为这个临时小卷确认 `min-free-space: 0`。Dufs 不会在 YAML 内展开 shell 变量；保留示例中的 `REPLACE_WITH_A_REAL_HASH` 会因无效认证配置启动失败。

若第 2.6 节的服务仍在运行，先在原终端按 `Ctrl+C` 正常停止，再回到保有 `$tutorial_root` 的**同一个 shell**启动；否则另一个 shell 没有这个变量，旧实例也仍占用共享根锁和端口。

启动：

```sh
target/debug/dufs --config "$tutorial_root/dufs.yaml"
```

YAML 采用严格字段校验。写错字段或保留已经删除的旧配置项会直接失败，不会静默忽略。这样在 current-only 版本切换时会明确暴露不匹配，并能防止操作者误以为某个安全限制仍然生效。

配置优先级是“内置默认值 → YAML → 命令行中明确给出的非认证参数”。因此命令行中的 `--bind` 和 `--trusted-proxy` 会分别整体替换 YAML 中的对应列表，而不是追加合并。账号只能来自受保护 YAML 的 `auth`；CLI 不定义账号参数，未声明选项由 clap 统一拒绝。YAML 中的相对路径按启动进程的当前工作目录解析，不按配置文件所在目录解析，生产中优先使用绝对路径。

配置文件本身必须是不超过 1 MiB 的普通 UTF-8 文件，不能是符号链接、FIFO 或设备。Linux 上只允许 root 或服务 euid 拥有，mode 必须精确为 `0400/0440/0600/0640`；组读模式要求 gid 匹配服务 egid，还要求单硬链接且没有扩展 POSIX access ACL。文件只打开一次，ACL 探测与正文读取使用同一 fd，并在读取前后复核身份、安全属性、大小和修改时间没有变化。生产配置只来自命令行和 YAML，Dufs 不读取 `DUFS_*` 环境变量；测试脚本中名字相似的环境变量只控制测试工具，不会成为服务配置。

## 2.10 常用配置及默认值

当前默认值由 [src/args.rs](../../src/args.rs) 定义：

| 配置 | 默认值 | 作用 |
| --- | ---: | --- |
| `serve-path` | 当前工作目录 `.` | 必须已经存在且是目录的共享根 |
| `state-dir` | 无，必须显式配置 | 位于共享根之外、权限和属主受校验的持久状态目录 |
| `auth` | 无账号，启动校验失败 | 至少一个 `user:<argon2id PHC>` 全权限账号 |
| `bind` | `127.0.0.1` | 监听地址，可重复指定 |
| `trusted-proxies` | 空 | 可声明代理头的直连网关 IP/CIDR；默认不信任任何代理 |
| `port` | `5000` | HTTP 后端端口 |
| `max-upload-size` | 100 GiB | 单文件声明长度上限 |
| `upload-idle-timeout` | 60 秒 | 上传没有进展的允许时间 |
| `upload-total-timeout` | 24 小时 | 单次上传流程总时限 |
| `max-concurrent-uploads` | 4 | 服务端并发上传槽 |
| `min-free-space` | 1 GiB | 计入预留后的最低可用空间 |
| `max-connections` | 256 | 所有 listener 共用的连接配额 |
| `max-search-entries` | 10000 | 搜索最多检查的目录项，硬上限 100000 |
| `max-concurrent-searches` | 2 | 并发目录列表和递归搜索数量 |
| `request-timeout` | 300 秒 | 非上传请求在产生响应头前的预算；不限制已经开始的流式下载正文总时长 |

`state-dir` 没有可随便接受的默认位置，必须显式配置。数据库文件固定为 `<state-dir>/state.sqlite3`。

## 2.11 启动时发生了什么

从外到内依次是：

1. [main.rs](../../src/main.rs) 构造 CLI；
2. 若是 `hash-password` 子命令，生成哈希后退出；
3. [args.rs](../../src/args.rs) 合并并验证 CLI/YAML；
4. 初始化异步日志；
5. `Server::builder(args).build()` 锚定共享根、打开状态库并组装服务；
6. 为每个地址创建 TCP listener；
7. 输出实际监听地址；
8. 接受连接，并在全局信号量许可下交给 Hyper HTTP/1.1；
9. 等待 SIGINT、SIGTERM 或所有 listener 异常退出。

任何启动校验失败都应在接收业务请求前退出，例如：账号为空、哈希格式错误、状态目录不安全、共享根不支持所需能力、监听地址为空。

## 2.12 正确停止

在前台终端按 `Ctrl+C` 会发送 SIGINT。正常排空时 Dufs 会：

1. 停止接受新连接；
2. 给现有连接和任务 30 秒正常完成；
3. 超时后取消普通工作，再给强制收尾 10 秒；
4. 关闭运行时 worker 和状态线程；
5. 最多等待 5 秒刷新日志；
6. 正常退出。

如果 30 秒正常宽限和后续 10 秒强制收尾都耗尽，程序会直接以失败状态退出；第二个停止信号也会要求立即退出。这两条快速退出路径都会跳过尚未完成的后续清理和日志 flush。`kill -9` 更会绕过全部应用停机代码，只应在真正失控时使用。

## 2.13 常见启动失败

### `Address already in use`

端口已经被占用。找出监听者：

```sh
ss -ltnp | rg ':5000\b'
```

换端口或正常停止旧进程，不要随意杀死不认识的系统服务。

### 账号参数解析失败

常见原因是 PHC 没有用单引号包裹、管理员 username 不是 canonical 形式或发生重复，或者哈希不符合 Foundation 当前 Argon2id 参数。配置 username 必须是 3～64 个 lowercase ASCII 字节，首尾 alnum、字符仅 `[a-z0-9._-]`；不能写 `@`、空白、Unicode 或首尾分隔符。

### 状态目录权限或位置错误

确认它存在、是普通目录、权限私有，并且与共享根没有包含关系：

```sh
stat "$tutorial_root/shared" "$tutorial_root/state"
```

### 同一共享根已有实例

Dufs 会对共享根目录 FD 取得非阻塞独占锁。指向同一根的第二个实例应启动失败；这是防止两个进程各自维护一套内存协调状态。

### `openat2` 不可用

当前内核或文件系统环境不满足安全路径解析要求。不要退回字符串拼接路径，应换到受支持的 Linux 环境。

## 2.14 清理学习环境

先正常停止 Dufs，确认没有仍在使用这些目录的进程，再删除实验目录：

```sh
case "${tutorial_root:-}" in
  /tmp/dufs-learning.[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9])
    if [ -d "$tutorial_root" ] &&
       [ ! -L "$tutorial_root" ] &&
       [ "$(stat -c %u -- "$tutorial_root")" = "$(id -u)" ]; then
      rm -r --one-file-system -- "$tutorial_root"
    else
      printf 'Refusing missing, linked, or foreign-owned tutorial path\n' >&2
    fi
    ;;
  *) printf 'Refusing unexpected tutorial path: %s\n' "${tutorial_root:-<unset>}" >&2 ;;
esac
```

不要把这条命令改成指向真实共享根。

## 2.15 本章检查点

你现在应该能解释：

- 为什么共享根和状态目录是两个不同的安全域；
- 为什么 Node.js 是开发依赖而不是生产运行依赖；
- 为什么后端打印 `http://127.0.0.1`，浏览器生产入口仍要求 HTTPS；
- liveness 和 readiness 分别证明什么；
- `--locked` 和 YAML 严格字段校验各自避免了什么意外。

下一章会补齐阅读源码所需的最小 Rust 与 Web 知识。
