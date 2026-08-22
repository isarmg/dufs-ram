# Dufs 全面代码审查与整改报告

> 文档说明：第 1～12 节以初次审查时的证据、风险解释和修复建议为主，便于追溯“为什么要改”。其中未明确标注“整改状态”“整改后”或“当前代码”的源码行号、测试数量和“当前”措辞属于当时快照；这些明确标注的整改补记、本页开头两张整改表以及第 13～15 节属于后续历史验收快照，第 16 节描述 2026-08-20 的当前上传终态并取代更早章节中与上传确认、覆盖条件或状态 schema 冲突的结论。

> 2026-08-20 补记：目录 ZIP 已从产品、配置、依赖和测试中整体移除；兼容窗内只保留对任意 `zip` 查询 key 的 `410 directory_archive_unsupported`。下文 ZIP 缺陷、修复和验收记录特意保留作为历史审计证据，不表示当前仍支持目录归档。该补记取代报告中所有与 ZIP 当前可用性有关的旧结论。

> 2026-08-20 上传补记：浏览器不再要求每个合法批次一律确认，而是先做有界目标预检；实际发布使用原子 no-replace 或绑定 owner/path/完整目标 identity 的 revision CAS。晚到冲突保留完整 stage 为 `AwaitingConfirmation`，可空 PATCH 条件发布或显式 discard；任何 `unknown` 都不会自动覆盖。第 16 节给出完整当前口径。

## 整改总览（2026-07-28）

初次审查列出的 2 个 High、13 个 Medium、16 个 Low 问题，以及随后在全仓二次复审中发现的 2 个 High、6 个 Medium、11 个 Low 问题，均已落实代码级修复或等效的风险消除措施。工程治理项也补齐了本地质量门、精确协议测试、许可证/第三方 notice、安全政策、主动部署行为测试、制品来源标识、SBOM、checksum、签名以及备份/升级/回滚流程。

| 编号 | 状态 | 已落实的整改与验证点 |
| --- | --- | --- |
| H-01 | 已修复 | ZIP 名称从真实相对组件逐段构造；拒绝 C0/DEL 控制字符、反斜杠、点组件、绝对/盘符/冒号、Windows 不兼容字符和 Windows 保留设备名（包括 `CONIN$`、`CONOUT$`、`COM0`、`LPT0`），并在 Unicode canonical normalization 后检测不区分大小写的跨平台命名碰撞；创建归档前完成全量预检，并以真实 ZIP 本地头和中央目录测试 `%5C` 攻击链。 |
| H-02 | 已修复 | 生产后端只启用 Hyper HTTP/1 handler（接受 HTTP/1.0 和 HTTP/1.1），删除 h2/server-auto 依赖；真实 HTTP/2 prior-knowledge preface 被拒绝。 |
| M-01 | 已修复 | 上传绝对 deadline 从路径排队开始，覆盖正文读取、写入、flush、metadata 恢复及进入不可取消提交点前的步骤；最终同步无法安全取消时，外层按时返回 `unknown`，后台继续持有租约收尾。 |
| M-02 | 已修复 | 浏览器区分 `transferring/submitting/unknown`；正文发送完成即停止传输计时，提交确认有独立上限，未知结果不显示重试。 |
| M-03 | 已修复 | 先等待路径租约，再占上传槽；热点路径排队受同一上传 deadline 约束。 |
| M-04 | 已修复 | mkdir、move、DELETE 的账号隔离 operation ID 在校验/路径等待前进入 `Reserved`；pre-commit 明确失败记 `failed`，pre-commit guard 异常丢弃会移除预留，只有 `mark_commit_started` 后异常才记 `unknown`。成功必须 durable 且前端严格核对回显 ID/`succeeded`；每账号配额防止占满全局 registry，实际非上传 mutation task 另受 64 个全局许可约束。 |
| M-05 | 已修复 | DELETE 在可见 rename 前写入 SQLite `Prepared` purge outbox；容量为全局 4096、每账号 1024，64 项内存 channel 只传递可合并 wake 和旧 orphan，不承担持久排队。单 worker 每片最多 256 项/25 ms；I/O 失败把 job 持久化回 `Ready` 并从 100 ms 退避到 30 秒，不因固定次数丢弃。状态转换瞬时失败时有界保留本地 claim，重启把遗留 `Claimed` 恢复为 `Ready`；独立 reconciler 处理 `Prepared`。维护与 purge 只跨片保存根内路径/cursor；EOF 后仍 `ENOTEMPTY` 会丢弃句柄并从 0 重扫并发新增项，fd 不随深度增长。 |
| M-06 | 已修复 | stage 固定 `0600`；上传会话只存于统一 SQLite `upload_sessions`，共享根内不写入、读取或导入 JSON state sidecar。部分 `Running` 用 PATCH 实际采用的同一个可写 no-follow stage fd 校验普通文件、`nlink == 1`、已记录 dev/inode 及 durable offset，并在该 fd 上截断未确认尾部；`CommitStarted` 是歧义屏障，重启恢复为 `Unknown`，不因 stage 只读、已 rename、缺失或异常降格。覆盖保留 numeric owner/group、非 setuid/setgid mode 与允许的非特权 xattr，`security.*`/`trusted.*` 及 setuid/setgid 目标一律拒绝。xattr 名称列表/条目数/单值上限为 64 KiB/1024/64 KiB；每值先查询精确长度，索引、名称和值总分配上限 1 MiB。上传终态绑定账号摘要，成功后记录 `Committed`，确定拒绝记录 `Rejected`；歧义状态不能盲重试。 |
| M-07 | 已修复 | 仅明确的不存在、非目录和安全隐藏的链接逃逸映射为 404；权限、容量和其他 I/O 故障继续上抛。 |
| M-08 | 已修复 | 服务边界统一通过 `AppError` 映射 I/O 状态并隔离公开消息与内部诊断；浏览器 JSON API 使用 RFC 9457 `application/problem+json` 的稳定 `code` 与 `detail`，不再靠纯文本或英文文案决定覆盖逻辑。move overwrite 对不同名称但同一 dev/inode 的硬链接在预检和 commit 内再次 fd-relative 复核，返回 `409 source_equals_destination`，不会把 POSIX rename no-op 误报为 `204`。 |
| M-09 | 已修复 | 首屏一次有界物化并排序，后续页按 offset 切片；直接列表前后复核目录，递归遍历在访问前和完成后复核所有访问目录。条目、DFS 深度/工作集和结果内存均有硬预算；HashSet 按最大深度预留，Vec/字符串扩容前计入旧/新缓冲峰值，ZIP BTreeMap 节点保守记账；变化返回 `409`。 |
| M-10 | 已修复 | 构造完成后的递归搜索页固定在同一内存结果内；遍历复核明确不冒充原子文件系统快照，检查间恢复、内容级变化及最终检查后的变化仍需存储快照才能强一致。 |
| M-11 | 已修复 | `fstat`/`fstatvfs` 在共享 mutex 外执行，返回后按同设备 revision 验证，最多重试 8 次并在持续竞争时失败关闭；block/fragment 乘法、分配单元取整或预算相加溢出也失败关闭。上传逻辑字节与约 1 MiB + 64 KiB 元数据余量分别按分配单元取整预留。ZIP 针对实际 temp device 先持有 metadata 预留并核对文件 device，再按逻辑 extent 取整且不消费 rounding slack。 |
| M-12 | 已修复 | ZIP 整个 handler、直接计划构造、名称空间校验、归档生成和异步磁盘预留 waiter 都有受跟踪 owner 并持有 permit；计划与预算化 Vec/BTreeMap 索引合计 64 MiB，节点/key/扩容峰值均计入，条目硬上限 100000。归档 finalize 后释放，慢客户端下载不占生成槽。 |
| M-13 | 已修复 | 应用在正文读取前增加全局/IP admission 和短正文 deadline，解析后再应用账号摘要失败退避及 `Retry-After`；可信回环网关的单值真实 IP 与覆盖整个登录路径族的 nginx 限流/并发共同生效。未认证 GET/HEAD 逐字段、逐逗号项精确解析 `Accept`，只在存在 `text/html` 且合法 `q > 0` 时重定向；`text/htmlx` 和 `q=0` 返回 `401`。 |
| L-01 | 已修复 | 同源校验严格比较外部 scheme 与 authority，并拒绝重复、逗号拼接或非法 `X-Forwarded-Proto`。 |
| L-02 | 已修复 | 递归删除改为 `openat/unlinkat`、`O_NOFOLLOW` 的根内相对算法，不再依赖 `/proc/self/fd`，也不跨工作片保存随目录深度增长的 fd 栈。 |
| L-03 | 已修复 | 启动时对共享根 FD 取得非阻塞独占锁；第二实例直接拒绝启动。 |
| L-04 | 已修复 | 三类 timeout 均限制为 1～365 天，并在监听前验证单调时钟可表示性。 |
| L-05 | 已修复 | 公开 liveness 与鉴权 readiness 分离；readiness 检查根目录、计入预留的磁盘水位、purge 背压和停机状态。 |
| L-06 | 已修复 | 日志以 `O_NOFOLLOW\|O_APPEND\|O_NONBLOCK\|O_CLOEXEC` 打开，只接受当前用户拥有、单硬链接的普通文件，并固定 `0600`。 |
| L-07 | 已修复 | Session 增加每账号 32 个上限，并在账号/全局容量压力下优先淘汰同账号旧记录。 |
| L-08 | 已修复 | ZIP 不再写 UID/GID，且将目录、普通文件和可执行文件 mode 规范化为安全可移植值。 |
| L-09 | 已修复 | ZIP 显式写入空目录条目。 |
| L-10 | 已修复 | 所有 405 分支返回准确 `Allow`；目录 ZIP HEAD 明确 `405 Allow: GET`，摘要静态资源 HEAD 则复用 GET metadata 并省略正文。 |
| L-11 | 已修复 | ZIP 管线、登录限流和操作注册表拆为专门模块；`listing.rs` 的归档职责已移入 `listing_zip.rs`。 |
| L-12 | 已修复风险基线 | JavaScript 门固定使用 Acorn 8.17.0 AST 与有界词法常量分析，覆盖字符串拼接、模板、`join`、别名、反射和动态全局属性访问，并以内置正负对抗样例验证；TypeScript 5.9.3 以 `allowJs + checkJs + strict + noEmit` 检查全部生产 JavaScript，外部/解析输入保持 `unknown` 并经守卫收窄，生产源码不保留显式或隐式 `any`。五个 Bash 源总过 `bash -n` 且在 CI 强制 ShellCheck 0.11.0。Markdown 链接、生产解析器 YAML/systemd/nginx 语法、隔离真实 nginx 的重定向/Host/SNI/回源/限流行为、发布来源/树/no-clobber 与覆盖率也进入统一脚本。部署 fixture 还从含空格、`&`、`#`、反斜杠的真实 checkout 读取文件后映射到安全运行名。Acorn 与 strict `checkJs` 分别是防御纵深和类型门禁，后者无需迁移 `.ts`，但二者仍不等价于完整跨过程污点证明或 ESLint。 |
| L-13 | 已修复 | 安全响应头测试改为精确值和关键 CSP 指令断言。 |
| L-14 | 已修复 | Playwright 每测试使用唯一目录和 worker 账号，启用双 worker、完全并行和一次诊断重试；`failOnFlakyTests: true` 保证首轮失败不会因重试通过而假绿。只有串行执行多次 Argon2 登录、注销和 Cookie 重放的复合认证场景使用 slow 测试预算，不改变产品请求 deadline。 |
| L-15 | 已修复 | 普通前端请求统一通过 AbortController/deadline 层和有界错误解析；operation 响应接受 `running/succeeded/failed/rejected/unknown` 并核对 ID（状态记录不持久化 rejected）。普通上传 XHR 要求状态绑定同一 ID，只有预期 PUT/PATCH 状态码、`committed` 和精确长度/满 offset 同时成立才成功；直接响应的 `running/rejected/not-started` 提供 Retry，但必须先 HEAD 原 ID，并在 HEAD 阶段严格核对长度/offset。直接 `not-seen`、显式 `unknown`、缺失/非法状态或 committed 不匹配保守归为 unknown。 |
| L-16 | 已修复 | 浏览器 mutation API 返回统一的 RFC 9457 Problem Details 与稳定错误码；前端只解析 canonical `application/problem+json`，不接受旧文本响应。 |
| P3 | 已完成 | 发布要求干净、版本 tag 精确指向 HEAD。完整门禁在已验证 commit archive 的无 Git 私有副本中以隔离 Cargo/npm/target/tmp 运行；Cargo vendor 后离线，npm cache 按 lockfile HTTPS+SHA-512 播种，可用 RustSec DB 无硬链接私有 clone。门禁后 snapshot index 复验内容/mode/新增路径，丢弃质量树，再 fresh extract 构建；签名前/发布前继续验证 exact source。全部源码树拒绝 symlink、submodule、特殊文件，双 archive 与 commit tree 完整核对。固定工具离线生成无本地路径泄漏的 SBOM；第三方 notice 要求每个包有非空、经审核的 SPDX `license` 表达式，再验证真实 SPDX AST、完整 permissive 分支与依赖自身许可证文本；`license_file` 不能替代表达式或作为分类 fallback。Rust 标准库 notice 绑定固定工具链审核摘要；`BUILD-ENVIRONMENT.txt` 记录实际源码/target/工具版本且不冒充全链钉扎。环境清单、SBOM、项目许可证和两类 notice 进入 checksum。签名 key 最后短暂打开，正式签名仍要求独立信任域；release 目录原子 no-clobber 发布。 |

### 二次复审追加问题

| 编号 | 严重度 | 状态 | 整改结果 |
| --- | --- | --- | --- |
| R2-H-01 | High | 已修复 | 发布入口拒绝 Git replace refs、legacy grafts、私有 attributes、tracked symlink/submodule/特殊条目；实际归档只经摘要锁定 bare façade，并在清空 Git 配置的环境运行。质量门本身也在验证过的无 Git commit archive 中以私有依赖/cache/target/tmp 执行，结束后由 snapshot index 复验并丢弃，再 fresh extract 构建。构建/打包两次 archive 的 commit、解包普通树、mode、额外路径与 SHA-256 均复核，后续 exact-source gate 为强制步骤。 |
| R2-H-02 | High | 已修复 | 私钥不再由构建 shell 预先打开；Cargo、Node、构建脚本、SBOM、第三方与 Rust 标准库 notice、文档、归档、checksum 和签名前 exact-source 检查完成后，短命子进程才打开/签名/验签。正式签名须使用独立账号、主机或 HSM。 |
| R2-M-01 | Medium | 已修复 | Nginx 对完整登录路径族统一设置真实 IP 限速、4 个并发连接、4 KiB body 和 10 秒正文时限；应用在读取正文前使用全局 burst 16/每秒 1 个及来源 IP burst 8/每秒 1 个的 token bucket，正文读取另受全局 32/每 IP 4 个并发许可和 10 秒 deadline 约束，解析后再执行来源 IP/账号失败退避。 |
| R2-M-02 | Medium | 已修复 | 维护扫描跳过跨设备条目时也提交新 cursor，不再每片重复停在同一项。 |
| R2-M-03 | Medium | 已修复 | 递归搜索在 tracked blocking worker 中直接转换结果、累计真实结构与字符串容量并排序；配置与运行时均有 100000 项硬上限，排序前后检查 deadline。 |
| R2-M-04 | Medium | 已修复 | 多地址 listener 改为先 accept 再为已接受 socket 获取全局连接许可，不再因 listener 数超过许可数导致固定地址饥饿。 |
| R2-M-05 | Medium | 已修复 | 深目录 purge 跨片只保存路径/cursor，fd 数不随深度增长；错误把未完成 job 持久化回 `Ready` 并从 100 ms 退避到 30 秒，健康 job 可越过。job 不再因固定失败次数丢弃；SQLite outbox、公平容量和持续 reconciliation 共同避免故障 job 饿死其他回收或静默遗失。 |
| R2-M-06 | Medium | 已修复 | 上传提交结果采用类型化发布阶段和 owner-scoped 终态：合法头后依次等待路径租约、尝试上传许可、以持有二者的受跟踪任务读取 route metadata，再查询 owner state/进入上传 mutation。路径/route 超时或槽满使用绑定的 response-only `not-started`；槽满直接返回 `429`，不读取或改变旧 state。前端任何可重试失败都先 HEAD，届时才恢复旧 ID 的真实状态。目标策略及 rename 前失败确定 `NotPublished/rejected`，rename 后持久性或 committed 终态落盘无法确认时保留满 offset running/返回 unknown。 |
| R2-L-01 | Low | 已修复 | fresh PUT 记录本次新建祖先目录身份；正文前失败时逆序删除仍为空且身份未变的目录。 |
| R2-L-02 | Low | 已修复 | PathCoordinator 的语义解析错误不再 `.ok()` 后降级为词法锁或无限重试，而是使用以共享根 inode 为锚、与所有路径冲突的保守 wildcard 租约；后续 handler 返回原根边界/I/O 错误。解析中的早期 waiter 只阻塞词法祖先/后代，无关路径可超车；最终语义键与 epoch 复验仍防止别名并发。 |
| R2-L-03 | Low | 已修复 | `fstat`/`fstatvfs` 在共享预留 mutex 外执行，再按同设备 revision 核对和记账；最多 8 次重试后失败关闭，block/fragment 乘法或预算溢出也失败关闭。上传按分配单元计入保守 metadata overhead，慢文件系统不再跨设备串行阻塞。 |
| R2-L-04 | Low | 已修复 | `bind: []` 在建立 listener 前作为明确配置错误拒绝。 |
| R2-L-05 | Low | 已修复 | 网关使用固定规范主机名，80/443 均有 default reject server，上游 `Host`/`X-Forwarded-Host` 不再信任任意 `$host`。 |
| R2-L-06 | Low | 已修复 | URI 只解析一次；内部 API/摘要资源要求唯一规范原始路径，外层 timeout/operation 分类和实际 handler 共享规范路径。 |
| R2-L-07 | Low | 已修复 | ZIP HEAD 返回 `405 Allow: GET`，不再在 GET 可能失败时伪造 `200`。 |
| R2-L-08 | Low | 已修复 | 已知摘要静态资源 HEAD 返回与 GET 一致的状态、类型、长度和缓存 metadata，但不发送正文。 |
| R2-L-09 | Low | 已修复 | 分页 cursor 与内存结果绑定账号摘要；缓存增加每账号 8 个/32 MiB 公平上限，同时保留全局 32 个/64 MiB 上限。构造期复核全部访问目录，但明确不是原子 FS snapshot。 |
| R2-L-10 | Low | 已修复 | operation ID 指纹冲突统一使用已记录的 `rejected` 状态，不再产生文档外 `conflict` 状态。 |
| R2-L-11 | Low | 已修复 | 递归搜索/ZIP 发现祖先符号链接循环时映射为可重试 `409`，不再作为内部 `500`。 |

没有改变的产品边界包括：所有账号共享同一权限域、仅支持 Linux 64 位和 `openat2`、每个共享根只允许一个实例、后端不终止 TLS、响应正文没有应用内总时长/最低速率（但已有 30 秒 socket write-idle），以及拥有共享根写权限的本地/virtiofs 外部参与者属于受信任域。这些是部署前提，不是通过本轮代码修复可以消除的多租户、恶意本地写者隔离或分布式能力差距。首次停机信号后应用最多等待约 30 + 10 秒；硬截止会跳过日志 flush 并立即强制退出，不保证卡住提交落盘或尾部日志写出。正常收尾的最多 5 秒 flush 由专用命名 OS thread 执行，不依赖可能被故障文件系统占满的 Tokio blocking pool；主 async 任务同时以 biased select 监听第二信号，收到后跳过 flush 等待并立即以 130/143 退出。

## 1. 审查信息

- 审查日期：2026-07-27
- 审查基线：`main` / `v0.47.0-2-g2c9aa58`
- 整改版本：`0.48.0`（未发布；历史审查基线仍为 `0.47.0`）
- 审查方式：全仓只读代码审查、静态检查、自动测试、依赖审计和关键安全问题动态复现
- 审查范围：
  - `src/` 全部 Rust 实现；
  - `assets/` 全部 HTML、CSS 和 JavaScript；
  - `tests/` 单元测试、集成测试、故障注入及 Playwright 测试；
  - Cargo、Node、构建脚本、配置、文档和部署约定。

`src/`、`assets/` 和 `tests/` 合计约 19,300 行，其中生产 Rust 实现约 10,700 行。

除新增本报告外，初次审查过程未修改业务代码。

## 2. 初始总体结论（历史基线）

这是一个明显经过系统性重构和安全加固的单机文件管理器，代码水平高于常见同类项目。文件系统边界、上传持久化、会话安全和测试覆盖尤其突出。

综合评价：**7.8/10，成熟度较高，但当时基线不建议直接发布。**

核心结论：

- 未发现 Critical 级漏洞。
- 发现 1 个已动态复现、在既定 HTTPS 网关模型下仍成立的 High 风险：ZIP 路径穿越条目。
- 发现 1 个条件性 High 风险：HTTP/2 可以绕过 TCP 连接级资源预算。
- Rust 静态检查、全部 Rust 自动测试、Chromium/Firefox 测试及依赖审计均通过。
- 项目适合作为个人或受控局域网内的单实例文件管理器。
- 项目不适合作为公网直接暴露服务，也不能用于隔离互不信任的多租户用户。
- 发布前应至少修复或禁用目录 ZIP。

## 3. 初始验证结果（历史基线）

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | 通过 |
| Rust 全部测试 | 244 通过，1 个 10 万目录项手工基准 ignored |
| Chromium Playwright | 29/29 通过 |
| Firefox Playwright | 29/29 通过 |
| Node 全部 JS/MJS 语法检查 | 通过 |
| `cargo audit` | 扫描约 250 个锁定依赖，无已知 advisory |
| `npm audit --audit-level=high` | 0 个已知漏洞 |
| `git diff --check` | 通过 |
| Microsoft Edge | 可选矩阵，本次未执行 |

依赖审计只能说明在本次 advisory 数据快照中没有已知漏洞，不能替代持续审计、代码审查和威胁建模。

## 4. 初始分项评分（历史基线）

| 维度 | 评分 | 评价 |
| --- | ---: | --- |
| 架构设计 | 8.5/10 | 子系统边界明确，关键安全约束具有专门抽象 |
| 代码质量与正确性 | 8.1/10 | 实现严谨，但个别错误语义和超时边界不完整 |
| 安全性 | 7.4/10 | 服务端路径安全很强，但 ZIP 缺陷必须先修复 |
| 数据完整性 | 8.4/10 | 暂存、原子替换、fsync 和路径协调设计优秀 |
| 可维护性 | 7.6/10 | 模块职责清楚，但若干核心文件和函数过大 |
| 性能与扩展性 | 7.2/10 | 有较多资源预算，但大目录、HTTP/2 和慢存储仍有缺口 |
| Rust 测试 | 9.1/10 | 单元、集成、并发、真实进程和故障注入覆盖优秀 |
| 前端与浏览器测试 | 7.8/10 | 场景丰富，但状态共享，缺少 lint 和类型检查 |
| 可观测性与运维 | 6.5/10 | 日志较好，健康检查、网关配置、制品和 CI 不足 |
| 文档 | 8.2/10 | 内容详尽并诚实记录边界，但存在重复和少量版本漂移 |
| 综合 | **7.8/10** | 高质量受控环境项目，修复 High 后可稳妥部署 |

## 5. 主要优点

### 5.1 服务端文件系统安全

`RootedFs` 启动时固定持有根目录 FD，并强制依赖 Linux `openat2`，使用 `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`。重要文件操作通过 FD-relative 的 `openat`、`renameat`、`unlinkat` 等完成。

证据：

- `src/server/rooted_fs.rs:77-105`
- `src/server/rooted_fs.rs:108-145`
- `src/server/rooted_fs.rs:338-369`
- `src/server/rooted_fs.rs:502-559`

这比先 canonicalize、随后持续使用字符串路径的传统实现可靠得多。审查未发现可通过普通 HTTP 路径或符号链接逃逸共享根的漏洞。

其他优点：

- 创建最终文件使用 `O_EXCL | O_NOFOLLOW`；
- 禁止删除共享根；
- 服务启动后即使根目录路径被重命名，服务仍锚定原始 inode；
- 写操作使用语义路径租约协调祖先、后代和符号链接别名冲突；
- 生产源码未发现 `unsafe` 块或外部命令执行入口。

### 5.2 上传与持久化设计

上传不会直接覆盖目标，而是：

1. 创建内部暂存文件；
2. 检查声明长度、实际长度和磁盘余量；
3. 持久化续传检查点；
4. 完整写入并 flush/sync；
5. 原子 rename；
6. 同步父目录。

证据：

- `src/server/upload.rs:420-509`
- `src/server/rooted_fs.rs:502-559`
- `src/server/storage.rs`

浏览器断开后，已经进入提交阶段的任务仍保持路径租约和持久化任务所有权，避免请求 future 被取消后留下半提交文件。这部分工程质量较高。

### 5.3 认证、会话与 CSRF

认证设计较为扎实：

- Argon2id 参数固定，并严格校验 PHC 策略：`src/auth.rs:24-30`、`src/auth.rs:380`；
- 未知用户名仍执行真实 Argon2 校验，降低用户名枚举：`src/auth.rs:287-303`；
- Session 和 CSRF 均使用 256 位随机值；
- 服务端只保存 Session Token 的 SHA-256 摘要；
- Session 具有 30 分钟空闲和 12 小时绝对寿命：`src/auth.rs:16-18`；
- Cookie 使用 `__Host-`、`Secure`、`HttpOnly`、`SameSite=Strict`：`src/auth.rs:16`、`src/auth.rs:30`；
- CSRF 使用常量时间比较：`src/auth.rs:260-285`；
- 登录正文、用户名和密码长度受到约束。

### 5.4 前端安全与结构

前端由七个原生 ES 模块组成，API、DOM、路径、列表、操作、上传和应用组装职责清晰，不需要生产前端打包器。

安全优点：

- 动态文本使用 `textContent`、`createElement` 和属性 API；
- 未发现 `innerHTML`、`eval`、`document.write` 等动态注入；
- 文件路径经过逻辑校验并逐段 URL 编码；
- 浏览器测试覆盖恶意文件名；
- 目录页使用严格 CSP、`X-Frame-Options: DENY`、`no-referrer` 和 `nosniff`；
- 内置资源使用完整 SHA-256 内容摘要 URL，只对准确命中的版本化资源长期缓存。

证据：

- `assets/modules/dom.js:62-70`
- `assets/modules/path.js:5-29`
- `src/server/listing.rs:774-798`
- `tests/frontend/browse.spec.js:3-15`
- `tests/frontend/accessibility.spec.js:68-79`

### 5.5 测试设计

测试覆盖的不只是 handler happy path，还包括：

- 真实服务进程和临时共享目录；
- 登录、Session、CSRF、Cookie 注销和重放；
- 符号链接逃逸和根目录替换；
- Range、缓存、分页、搜索和 ZIP；
- 上传中断、续传、磁盘空间及故障注入；
- 并发写路径租约和真实停机行为；
- HTTPS 测试网关后的 Chromium、Firefox 交互；
- XSS 文件名、键盘焦点和基础无障碍行为。

统一检查脚本包含 Rustfmt、Clippy `-D warnings`、全部 Rust targets/features、双浏览器测试、依赖审计和 Git 清洁检查：`scripts/check.sh:24-55`。

## 6. High 风险问题（初始证据）

### H-01：ZIP 文件名规范化产生路径穿越条目

严重度：**High**

既定 HTTPS 网关模型下：**成立**

这是本次最重要且已经动态复现的问题。

#### 证据链

1. Linux 将反斜杠视为普通文件名字节。HTTP 路径解析只要求组件为 `Component::Normal`，不会拒绝 `\`：
   - `src/server.rs:695-705`
2. ZIP 入口名由真实相对路径原样转换为 UTF-8 字符串：
   - `src/server/listing.rs:1220-1233`
3. 该字符串直接传给 `ZipWriter::append_file`：
   - `src/server/listing.rs:1308-1321`
4. `async-deflate-zip 0.2.0` 在 `append_file` 中调用 `sanitize_path`，将全部 `\` 替换为 `/`，但不拒绝绝对路径、`.` 或 `..`：
   - `async-deflate-zip-0.2.0/src/writer/zip_writer.rs:118`
   - `async-deflate-zip-0.2.0/src/writer/zip_writer.rs:398-407`

#### 动态复现

在隔离共享目录内创建合法 Linux 文件名：

```text
..\escape.txt
```

启动 Dufs、登录并下载根目录 ZIP，随后检查归档中央目录，实际输出：

```text
../escape.txt
```

同理：

```text
a\..\..\escape.txt  -> a/../../escape.txt
\etc\target         -> /etc/target
```

#### 影响

攻击路径：

1. 已认证用户通过 `%5C` 上传带反斜杠的文件名，或者本地进程将其放入共享目录；
2. 其他用户下载目录 ZIP；
3. 用户使用没有 Zip Slip 防护的解压器；
4. 归档可能越过解压目录覆盖客户端文件。

在特定解压路径和权限下，可能进一步导致配置文件、启动项或代码被覆盖。

服务端的 openat2、CSRF、认证和 TLS 都不能缓解下载产物在客户端解压时的风险。

#### 修复建议

- 修复前在网关拦截 `?zip`，或临时从代码中禁用目录 ZIP。
- ZIP 入口名必须从真实路径组件逐段构造。
- 每个组件拒绝：
  - `\`；
  - 空组件；
  - `.`；
  - `..`。
- 最终入口名不得以 `/` 或 `\` 开头。
- 拒绝 Windows 盘符和 UNC 形式。
- 不建议简单将 `\` 替换成 `_`，否则可能产生文件名碰撞。
- 增加解析真实 ZIP 中央目录的端到端回归测试，至少覆盖：
  - `..\escape.txt`
  - `\absolute.txt`
  - `a\..\..\escape.txt`
  - 编码后的 `%5C`
  - 正常嵌套目录
- 建议向 `async-deflate-zip` 上游报告该规范化行为。

### H-02：HTTP/2 绕过全局请求资源预算（已修复）

严重度：**High，条件性**

整改状态：后端现已强制使用 Hyper HTTP/1 连接处理器并接受 HTTP/1.0 和 HTTP/1.1，删除 `server-auto` 和 `h2` 生产依赖；标准 HTTP/2 prior-knowledge connection preface 有真实 TCP 拒绝回归测试。以下证据与建议保留为初次审查基线记录。

既定网关模型下：

- 后端只允许可信网关访问，且网关回源固定 HTTP/1.1 时，风险基本被隔离；
- 后端端口可被直接访问，或网关回源使用 h2c 时，风险成立。

#### 证据

- `max_connections` 是 TCP 连接级 semaphore：`src/main.rs:170-175`；
- 服务端使用 `hyper-util server-auto`，支持明文 HTTP/2 prior knowledge；
- 只为 HTTP/1 配置了 10 秒请求头时限和 64 KiB 缓冲：
  - `src/main.rs:380-389`
- 没有项目级 HTTP/2 stream 上限或全局 request semaphore；
- `request_timeout` 只覆盖响应头生成：
  - `src/server.rs:181-215`
- 普通文件和已经生成的 ZIP 正文不受该 timeout 约束：
  - `README.md:192`

一个 TCP 连接可以承载大量并发 stream，因此连接数上限不等于并发请求、任务或文件描述符上限。认证用户还可以建立大量慢速文件下载。

#### 修复建议

- 如果后端不需要 HTTP/2，直接强制 HTTP/1.1。
- 如果保留 h2c：
  - 显式设置较低的 `max_concurrent_streams`；
  - 设置 HTTP/2 keepalive interval/timeout；
  - 增加跨连接、跨 stream 的全局活跃请求 semaphore；
  - 给普通下载和 ZIP 正文增加空闲时限、总时限或最低传输速率；
  - 在网关限制客户端/IP 并发和慢速响应。
- 增加 HTTP/2 prior-knowledge 集成及压力测试。

## 7. Medium 风险问题（初始证据）

### M-01：上传总超时不覆盖磁盘写入和最终提交

`PUT/PATCH` 绕过普通 `request_timeout`：`src/server.rs:191-197`。

上传的 idle/total timeout 主要包围读取下一个 body frame：

- `src/server/upload.rs:538-577`

真正的磁盘写入、flush、sync 和 rename 不在同一 deadline 内：

- `src/server/upload.rs:600-646`
- `src/server/upload.rs:454-494`

慢速或挂死的 FUSE、NFS 或故障磁盘可能长期占用上传槽、路径租约和提交任务，并延长停机时间。

建议让 total deadline 贯穿 chunk 写入、flush 和提交；在进入不可取消提交点前重新检查 deadline，并增加 slow `AsyncWrite`、slow fsync 和停机测试。

### M-02：浏览器上传超时可能误报服务端提交失败

浏览器固定使用：

- 2 分钟 idle timeout；
- 24 小时 total timeout；
- 30 秒 checkpoint 查询 timeout。

证据：`assets/modules/upload.js:17-20`。

idle timer 只在上传 progress 事件中重置，但一直持续到收到完整 HTTP 响应：

- `assets/modules/upload.js:284-301`
- `assets/modules/upload.js:385-406`

正文已经发送完成、服务端仍在慢速 sync/rename/fsync 时，浏览器可能主动 abort 并显示失败，而服务端提交任务仍可能继续完成。

建议正文发送完成后停止传输 idle timer，增加独立的“正在提交”状态；提交等待超时应显示“结果未知，请查询状态”，而不是普通上传失败。

### M-03：上传槽位先于路径租约获取，存在头阻塞

代码先占用 upload permit，随后才等待冲突路径租约：

- `src/server.rs:366-388`

大量针对同一路径的排队请求可以占满所有上传槽，导致其他无冲突路径也返回 `429`。

建议先完成 header 校验和路径排队，真正可运行时再获取 upload permit；或者给路径等待设置独立 deadline 和公平队列。

### M-04：普通写操作存在结果不确定性

外层请求超时或客户端断开后，内部 mutation 会继续运行以保持路径租约和持久化保证。这是合理的安全设计，但会产生协议歧义：

- 客户端收到 `504` 或无响应；
- mkdir、move、delete 实际可能稍后成功；
- 客户端重试可能遇到冲突或重复操作。

此外，rename 已经改变目录可见状态后，父目录 fsync 失败也会返回 500：

- `src/server/rooted_fs.rs:502-523`
- `src/server/rooted_fs.rs:571-605`

建议引入 operation ID、幂等键或可查询的异步操作状态，并在日志中区分 `visible`、`committed` 和 `durable` 阶段。

修复状态：已完成。浏览器 mkdir、move 和 DELETE 均生成 `X-Dufs-Operation-Id`，服务端要求这三类请求携带唯一 canonical UUID；必需的文件型 SQLite state store 中按账号隔离、固定容量、完成态自动过期的 operation 记录使同 ID 同请求在运行中返回 `202`、完成后幂等重放，同 ID 不同请求返回 `409`。持有 guard 的运行项不会按 TTL 淘汰，容量全局 4096、每账号 1024，满容量时安全返回 `503`；已有 ID 的状态/重放优先于配额判断。唯一认证状态端点 `GET /__dufs__/api/jobs/<UUID>` 使用 `job_id` 返回 `running/succeeded/failed/unknown`；真正的 commit task 持有状态 guard，只有 durable success 才登记成功，业务拒绝登记失败，rename 后 fsync 失败等无法证明结果的路径登记 `unknown/outcome_uncertain`。前端在结果未知时仅查询一次且绝不盲目重试，响应与默认访问日志包含 operation ID 和 state。覆盖注册表并发、容量/过期、长期运行 guard、保守未知终态、账号隔离/公平配额、API 重放/冲突/鉴权及浏览器一次查询行为的回归测试已加入。

### M-05：DELETE purge 和全树维护缺少工作量预算

每个 DELETE 将条目原子移入内部 trash 后，立即启动一个独立后台 purge：

- `src/server.rs:566-584`

目录 purge 使用 blocking `remove_dir_all`，没有 purge semaphore：

- `src/server/rooted_fs.rs:732-754`

启动及定时维护还会递归扫描整个共享根：

- `src/server/upload.rs:167-205`
- `src/server/upload.rs:650-745`

扫描没有目录项数、深度或每轮时间预算。大量大目录删除会堆积 blocking 工作和存储 I/O。

建议使用低并发 purge 队列，限制队列长度；维护任务应分批扫描、保存游标并设置每轮条目及时间预算。

### M-06：覆盖上传丢失 ACL、xattr 和所有权元数据

覆盖目标前只读取普通 `permissions()`：

- `src/server/upload.rs:278-284`

提交前也只恢复 mode：

- `src/server/upload.rs:484-494`

由于最终通过新 inode 原子替换目标，owner/group、POSIX ACL、xattr、SELinux label、capability xattr、备份标记和硬链接身份都会丢失。

如果共享目录同时受操作系统 ACL 或其他本地服务管理，这可能改变实际权限边界。

建议明确产品策略：

- 复制经过白名单允许的 owner/group、ACL 和 xattr，并测试失败语义；或
- 检测到安全元数据时拒绝覆盖并提示管理员。

生产部署应使用权限最小化的专用服务账号。

### M-07：元数据错误被伪装为 404

主路由把未预期的 metadata 错误折叠为 `None`：

- `src/server.rs:395-404`

随后 `guard_root_contained` 对未预期错误同样返回“隐藏”：

- `src/server.rs:672-691`

因此 EIO、EMFILE、PermissionDenied 或存储故障可能表现为普通 404，既影响客户端语义，也会让监控遗漏真实存储故障。

建议只把明确的 ENOENT、悬空链接和 root escape 映射为 404；其他错误应通过 typed error 记录并返回合适的 403 或 500。

### M-08：错误类型仍未贯穿 handler

`AppError` 和 `ListingError` 已经建立了公开消息与内部诊断分离，但多个 handler 仍直接修改 `Response` 并返回 `anyhow::Result`。

这造成：

- 403、404、409、500 映射不一致；
- 方法不允许、权限不足及 I/O 错误处理散落；
- 某些文件下载 PermissionDenied 最终成为通用 500。

建议建立统一的 `DomainError/IoError -> AppError -> HTTP Response` 映射，让 handler 返回 `Result<Response, AppError>`。

### M-09：大目录分页总体成本过高

每一页目录列表都完整扫描目录，仅在内存中保留 `limit + 1` 个候选：

- `src/server/listing.rs:839-898`

单页内存有界，但翻完包含 N 项、页大小 K 的目录时，总体接近 `O(N²/K)` 次访问和比较。

10 万目录项测试目前是默认 ignored 的手工基准：

- `tests/pagination.rs:99-119`

建议引入服务端快照或索引；名称排序可考虑稳定 readdir token 快路径。至少应给直接列表增加独立 entry budget、耗时指标和自动性能基线。

### M-10：递归搜索 cursor 只绑定搜索根快照

搜索分页只检查根目录 snapshot。嵌套子目录发生增加、删除或替换时，不一定改变根目录时间，因此跨页搜索可能漏项或重复而不返回 409。

证据：

- `src/server/listing.rs:436-463`
- `src/server/listing.rs:518-535`
- `src/server/listing.rs:967-987`

建议为递归查询使用服务端 snapshot/version，或者在 cursor 中包含已遍历目录的聚合 generation。无法提供一致快照时，应明确 API 为 eventually consistent。

### M-11：磁盘余量检查会在异步路径中同步执行

磁盘空间追踪直接调用同步 `fstat/fstatvfs`：

- `src/server/disk_space.rs:27-39`
- `src/server/disk_space.rs:118-157`

上传每个 body chunk 都会检查一次：

- `src/server/upload.rs:637-646`

ZIP 的 `AsyncWrite::poll_write` 也会执行该同步检查。繁忙存储、网络文件系统或大量小 chunk 会阻塞 Tokio worker，并造成 syscall 放大。

建议按较大 extent 批量预留空间，按字节或时间阈值复查；把可能慢的 statvfs 放到 blocking 容量服务，并增加 runtime starvation 测试。

### M-12：慢 ZIP 客户端可长期占用唯一 ZIP permit

默认 ZIP 并发为 1。permit 从生成归档一直持有到响应 body 发送或被丢弃：

- `src/server/listing.rs:207-253`
- `src/server/listing.rs:711-727`

响应头发出后没有服务端 body idle/min-rate timeout，因此一个极慢的下载客户端可能阻塞所有后续 ZIP。

建议分离“压缩 CPU 并发”和“临时归档留存/发送配额”，并在网关限制响应空闲时间和最低传输速率。

### M-13：登录没有应用层速率限制

登录 Argon2 只有两个并发槽：

- `src/server.rs:111`
- `src/server/session.rs:167-177`

槽位满时会拒绝，但没有基于账号或 IP 的 token bucket、失败次数限制或退避。攻击者可以持续占满计算槽并进行在线口令猜测。

项目文档把登录限速交给网关。在严格网关模型下该风险得到缓解，但绕过网关或配置遗漏时仍然成立。

建议网关按真实客户端 IP 和账号双维度限速；应用侧增加全局失败速率、短期指数退避、`Retry-After`、指标和告警。


## 8. Low 风险和维护问题（初始证据）

### L-01：来源检查只比较 authority

`request_source_is_same_origin`：

- 会拒绝 `Sec-Fetch-Site: cross-site`；
- 缺少 Origin 时直接接受；
- 有 Origin 时只比较 authority，不比较 scheme。

证据：`src/server/session.rs:369-396`。

所有认证写操作仍要求独立 CSRF token，因此没有发现直接 CSRF 绕过。建议配置明确外部 origin，并在可信代理配置下比较规范化 scheme、host 和 port。

### L-02：递归删除依赖 `/proc/self/fd`，但启动不验证（历史证据）

目录递归删除通过 `/proc/self/fd/<fd>/<name>` 调用 `remove_dir_all`：

- `src/server/rooted_fs.rs:455-471`
- `src/server/rooted_fs.rs:732-754`

如果容器没有挂载 `/proc` 或权限受限，DELETE 仍可能先把目录隐藏并返回成功，后台 purge 随后失败，trash 长期占据空间。

历史建议是在启动时 fail-fast 验证 `/proc/self/fd`，或实现纯 `openat/unlinkat` 的 FD-relative 递归删除。整改选择了后者；当前代码和部署已不依赖 `/proc/self/fd`。

### L-03：单实例约束没有进程锁（历史证据）

项目文档明确要求每台系统只运行一个实例，但程序不会阻止误启第二实例：`README.md:5`。

第二实例会绕过进程内路径租约、活跃上传集合和磁盘空间预留。建议在共享根上获取 advisory lock，并在锁不可用时拒绝启动。

整改已在共享根 fd 上取得非阻塞独占锁，因此当前强制语义是“每个共享根一个实例”；同机不同根仍可运行独立实例。

### L-04：极端 timeout 配置启动时未拒绝

配置解析只检查 upload timeout 非零及相对大小：

- `src/args.rs:354-362`

运行时使用 `Instant::checked_add`：

- `src/server/upload.rs:538-550`

`u64::MAX` 等极端值可以通过启动校验，却让上传直接返回内部错误。建议在启动时验证 Duration 可表示性并设置合理上限。

### L-05：健康检查要求认证且过于浅

`GET /__dufs__/health` 位于认证之后，并只返回静态：

```json
{"status":"OK"}
```

证据：

- `src/server.rs:295-307`
- `src/server.rs:642-646`

建议拆分：

- 无敏感信息的最小 liveness；
- 受保护的 readiness，检查共享根可访问性、磁盘水位和维护队列状态。

### L-06：日志文件安全属性未固定

日志文件使用普通 `create + append`，没有固定 `0600`、`O_NOFOLLOW` 或 owner 校验：

- `src/logger.rs:173`

高权限服务向攻击者可控制目录写日志时存在本地符号链接追加风险。建议固定权限、禁止跟随符号链接、校验文件类型和 owner，并优先使用 journald。

### L-07：Session 全局容量缺少账号公平性

Session 总容量固定为 1024，满时淘汰全局最久未活动记录：

- `src/auth.rs:84`

同一有效账号可反复登录并驱逐其他账号会话。建议增加每账号会话上限或优先淘汰同账号旧 Session。

### L-08：ZIP 泄露服务器 UID/GID 和特殊 mode

ZIP 写入真实 UID/GID 和 `mode & 0o7777`：

- `src/server/listing.rs:1386-1392`

下载者可以看到服务账号数值 ID、权限布局和 setuid/setgid/sticky 位。建议移除 UID/GID，并按产品需求规范化权限。

### L-09：ZIP 不保留空目录

ZIP 遍历只收集普通文件：

- `src/server/listing.rs:1292-1305`

空目录不会写入归档。如果“下载目录”要求结构保真，应显式增加 directory entries。

### L-10：405 响应没有 `Allow`

未知方法返回 405，但没有 `Allow` header：

- `src/server.rs:288-290`
- `src/server.rs:559-561`

属于 HTTP 协议完整性改进。

### L-11：核心模块和函数过大

主要文件规模：

- `src/server/listing.rs`：2006 行；
- `src/server/upload.rs`：1309 行；
- `src/server/rooted_fs.rs`：969 行；
- `src/server.rs`：953 行；
- `src/args.rs`：800 行。

建议拆分：

- listing cursor、排序和快照；
- ZIP 遍历、生成和响应发送；
- upload protocol、checkpoint、commit 和 maintenance；
- 路由 dispatch 与错误映射。

### L-12：前端质量门禁不足（历史证据）

初始快照没有：

- ESLint 或同类 JS lint；
- `checkJs` 或 TypeScript 类型检查；
- Prettier、Stylelint、HTML validator；
- axe/pa11y；
- Rust 或 JavaScript 覆盖率门槛；
- ShellCheck、Markdown lint 和链接检查；
- fuzz、property、Miri、sanitizer 或 mutation test。

当时已有的禁止 `innerHTML` 正则测试很有价值，但不能替代完整静态分析。

整改先补齐了项目定制的 JS/Markdown 负例门、Rust 覆盖率、部署解析、浏览器 flaky 检测和统一检查入口；当前终态又加入全生产 JavaScript 的 TypeScript 5.9.3 strict `checkJs`（外部/解析输入保持 `unknown` 并收窄，拒绝显式或隐式 `any`）、本地存在时/远程 CI 强制的 ShellCheck 0.11.0，以及固定 `@axe-core/playwright` 的自动可访问性扫描。迁移为 `.ts` 源码、完整 ESLint、真实读屏/人工可访问性验收、fuzz、Miri 等仍是可继续扩展的高级工具，不把自动化门禁误报为已经执行这些工具。

### L-13：安全响应头测试只验证存在

测试只断言 CSP、XFO、Permissions-Policy 等 header 存在：

- `tests/http.rs:21-36`
- `tests/http.rs:313-334`

如果 CSP 被意外放宽，这些测试仍会通过。

建议精确或结构化断言：

- `default-src 'none'`；
- `frame-ancestors 'none'`；
- script/style/connect 白名单；
- `X-Frame-Options: DENY`；
- `Referrer-Policy: no-referrer`；
- `X-Content-Type-Options: nosniff`；
- 完整 Permissions-Policy。

### L-14：Playwright 测试状态共享

配置强制：

- `fullyParallel: false`
- `workers: 1`
- `retries: 0`

证据：`playwright.config.js:22-31`。

每个浏览器项目只创建一个共享目录，操作测试会创建、移动、覆盖和删除固定文件名。当时顺序运行可以通过，但无法安全开启并行、重试或 `--repeat-each`。

建议每个测试使用唯一子目录或独立后端实例，随后再启用并行与受控重试。

### L-15：普通前端请求缺少统一取消和超时

目录加载、新建、移动、删除和注销没有统一的 AbortController 和客户端超时。网关半断开时，界面可能长期停留在 loading/pending 状态。

建议建立统一请求层，区分：

- 网络失败；
- 客户端超时；
- 认证失效；
- CSRF 失效；
- 业务冲突；
- 结果未知。

### L-16：前端错误协议依赖文本

通用 `assertResponse` 丢弃响应正文，只保留 HTTP 状态：

- `assets/modules/api.js:20-30`

移动覆盖逻辑又依赖服务端正文精确等于固定英文字符串。建议返回带稳定 `code` 的 JSON 错误，由前端将 code 映射为用户文案。



## 9. 已知设计边界

以下属于明确的产品约束，不应误报为实现漏洞，但必须纳入部署验收：

1. 所有账号都拥有整个共享根的完整浏览、上传、覆盖、移动和删除权限。
2. 不能把不同账号理解为互不信任租户之间的权限隔离。
3. 后端不提供 TLS，必须使用独立主机名的 HTTPS 网关。
4. 后端端口必须通过回环地址、私网 ACL 或防火墙禁止客户端绕过网关。
5. 网关应负责登录限速、请求/响应空闲超时、最大正文、连接限制、HSTS 和缓存禁止。
6. 同一个共享根只能运行一个 Dufs 实例；代码不会阻止同机不同根的独立实例。
7. Linux 必须支持 `openat2`；当前纯 FD-relative 清理实现不再依赖 `/proc/self/fd`。
8. 测试目录中的 TLS 私钥已明确标记为 localhost 自动化用途，不是泄漏的生产密钥。
9. 硬链接无法像符号链接一样由 openat2 判断是否还有根外名称；远程 API 不能创建硬链接，因此这是本地共享目录信任边界。上传会拒绝检测到的多硬链接，并在提交前复核包含 nlink 在内的完整 stat 快照，但拥有共享根写权限的外部进程仍能在最后一次 `statat` 与 `renameat` 之间竞争。

## 10. 初始建议补充的测试

### 安全

- 危险 ZIP entry central-directory 测试；
- 已完成 HTTP/2 prior-knowledge connection preface 拒绝测试；
- 路径、Range、cursor、上传头的 property/fuzz 测试；
- 安全响应头精确值测试；
- 日志文件 symlink 和权限测试。

### 上传和持久化

- 可控 Pending/慢 `AsyncWrite`；
- 慢或失败 fsync；
- 上传 total deadline 覆盖磁盘写和提交；
- 客户端超时后最终文件状态；
- rename 已可见但父目录 fsync 失败；
- 幂等重试。

### 并发与性能

- 同路径上传队列占满 upload permit；
- 慢 ZIP body 占用 permit；
- 百万级目录维护分批和取消；
- 大目录多页完整遍历性能；
- PathCoordinator 热点公平性；
- purge 队列压力；
- statvfs 慢调用导致的 runtime starvation。

### 前端和无障碍

- 每测试独立共享目录；
- `--repeat-each` 和多 worker；
- 200%/400% 缩放；
- forced-colors 和深色模式；
- axe 扫描；
- 普通 Fetch 超时和取消；
- 上传正文完成后长时间等待提交。

## 11. 初始整改优先级

### P0：发布前必须完成

1. 修复 ZIP 路径验证，并增加真实归档解析回归测试。
2. 修复前通过网关拦截 `?zip`，或临时禁用目录 ZIP。
3. 检查已有共享目录中是否存在包含反斜杠的文件名。

### P1：下一轮稳定性与安全迭代

1. **已完成：**后端强制使用 Hyper HTTP/1 连接处理器（接受 HTTP/1.0 和 HTTP/1.1），并删除 HTTP/2 生产依赖。
2. 让上传 deadline 覆盖写入、flush 和提交。
3. 修正前端“正文已发送但提交未完成”的状态与超时语义。
4. 为 DELETE purge 和维护扫描增加队列、并发和工作量预算。
5. 默认绑定改为回环地址。
6. 提供经过测试的完整网关及 systemd 配置。
7. 明确覆盖上传时 ACL、xattr 和 owner/group 的保留策略。

### P2：正确性与可维护性

1. 统一 typed error 和 I/O 到 HTTP 状态的映射。
2. 给 mutation 增加幂等键、operation ID 或最终状态查询。
3. 拆分路由、上传状态机、ZIP 管线和列表游标。
4. 优化大目录分页、递归快照和 statvfs 调用频率。
5. 拆分 liveness 和 readiness。
6. 为共享根增加单实例进程锁。
7. 启动时验证 timeout 可表示性；`/proc/self/fd` 依赖则通过纯 FD-relative 递归删除从实现中消除。

### P3：工程治理

1. 建立自动质量门禁和覆盖率基线。
2. 引入 JS lint、`checkJs`、axe、ShellCheck 和 Markdown link check。
3. 隔离 Playwright 测试数据，使其可并行、重试和重复。
4. 发布产物嵌入 Git SHA，并生成 checksum、SBOM 和签名。
5. 补充许可证、安全报告渠道、备份、升级和回滚说明。

## 12. 上线判断

### 初始审查 HEAD（历史结论）

当时不建议作为新版本发布，阻断原因是已经确认的 ZIP 路径穿越归档。

### 整改后工作树

本报告列出的代码级发布阻断项已经关闭。在完整质量门通过、从干净提交生成并验证签名制品后，如果严格满足以下条件，项目具备受控环境生产可用性：

- HTTPS 网关；
- 后端端口 ACL；
- HTTP/1.1 回源；
- 单实例；
- 可信账号；
- 专用低权限服务用户；
- 网关登录限速、响应总时长/速率和附加空闲策略；
- 合理的备份和恢复方案。

公网入口仍必须经过限流和超时配置正确的 HTTPS 网关。Dufs 自身有 30 秒响应 write-idle，但没有正文总时长或最低速率。若要支持互不信任用户、多个写实例或跨节点共享状态，则属于新的产品架构范围；目录遍历的前后复核和进程内不可变结果也不能替代存储快照或分布式版本。

## 13. 最终验收结果（2026-07-28）

本节记录当时一次完整 `0.48.0` 验收快照，用于历史追溯；后续继续增加的终态、预算、停机、发布与部署检查会改变测试数量，不能把下表固定数字当作当前工作树通过证明。正式发布必须以该精确 tag 上重新运行 `scripts/check.sh` 和发布脚本的即时结果为准。

| 验收面 | 实测结果 |
| --- | --- |
| Rust 格式与静态检查 | `cargo fmt --all -- --check` 通过；`cargo clippy --locked --all-targets --all-features -- -D warnings` 通过；锁文件 metadata 解析通过 |
| Rust 自动测试 | 全 targets/features 共 358 项通过、0 项失败、1 项手工十万目录基准按设计忽略 |
| Rust 覆盖率 | `cargo-llvm-cov 0.8.6` 行覆盖率 77.40%（13,165 行中 2,975 行未覆盖），通过 70% 强制门槛 |
| 前端源码与文档 | 18 个 JavaScript 文件通过语法、格式和项目安全规则；8 个 Markdown 文件通过格式、本地链接、锚点、围栏代码及 symlink fail-closed 检查 |
| 浏览器端到端 | Chromium 33/33、Firefox 33/33；`failOnFlakyTests` 生效，无重试假绿 |
| 依赖安全 | RustSec 使用当日 1170 条 advisory 扫描 251 个 Cargo 依赖，未发现漏洞；`npm ci` 与 high 级 `npm audit` 通过，0 个漏洞 |
| 部署材料 | systemd、nginx 和 Dufs YAML 由生产解析器验证；当前门禁还启动隔离 nginx/mock upstream 验证重定向、Host/SNI、回源头、登录正文及连接/速率限制 |
| 发布脚本自测 | 当时覆盖输出 FD/no-clobber、信号、签名、SBOM 和 Git 来源树；当前脚本进一步拒绝 symlink/submodule/special file，在隔离 commit archive 内强制完整门禁并复验树，同时验证 npm cache 播种、真实 SPDX notice 和固定 Rust 标准库 notice，须由正式 tag 即时重跑 |
| 可重复发布 | 从同一个临时干净 `v0.48.0` 精确 tag 分别在短、长 checkout 路径执行锁定依赖 vendoring 和离线隔离构建；两边各生成且只生成 4 个公开文件，两个 release 目录逐字节一致 |
| 制品独立验证 | 当时两份外层 SHA-256 和 Ed25519 签名均验证成功；当前包内验证还必须覆盖 `THIRD_PARTY_LICENSES.txt` 的 SPDX/来源约束及 `RUST-STANDARD-LIBRARY-COPYRIGHT.html` 的审核摘要 |

该次验收时仓库故意保留尚未提交的整改工作，因此统一脚本最后的“真实工作树必须干净”发布前置条件不能在当时工作树上成立；这不是代码或测试失败。为验证该条件和实际发布链，当时验收使用从该文件集合生成的临时干净提交及精确 tag，分别从短、长 checkout 路径运行完整流程；这些仅用于验收的私钥、仓库、stage 和制品不属于项目交付物。正式发布仍须在真实 `v0.48.0` 精确 tag 上重跑同一流程。

在上述历史快照之后，随后一轮临时签名发布已经通过隔离前置条件、部署检查和 nginx 主动边界测试并运行到 `cargo test`，但共享 `/mnt/dufs` 使用率达到 99% 后链接器以 `No space left on device` 中止。因此该次运行属于环境容量阻断，不能被写成完整发布链已经通过；释放空间后仍须从精确 tag 重新执行全链。

### 提交前工作树增量复核（历史）

以下结果来自当时完成一轮上传、路径、磁盘、移动、停机和发布门禁修复后的提交前工作树；它们更新了该阶段代码与测试结论，但不替代干净精确 tag 上的正式发布验收：

| 复核面 | 当时结果 |
| --- | --- |
| Rust 格式与静态检查 | `cargo fmt --all -- --check`、`cargo check --locked --all-targets --all-features` 和 `cargo clippy --locked --all-targets --all-features -- -D warnings` 全部通过 |
| Rust 自动测试 | 全 targets/features 共 401 项：400 项通过、0 项失败、1 项十万目录手工基准按设计忽略 |
| Rust 覆盖率 | `cargo-llvm-cov 0.8.6` 行覆盖率 77.55%（16,446 行中 3,692 行未覆盖），通过 70% 强制门槛 |
| 前端源码与文档 | 18 个 JavaScript 文件通过语法、格式和 Acorn AST/词法常量安全门；8 个 Markdown 文件通过格式、本地链接、锚点、围栏代码及 symlink fail-closed 检查 |
| 浏览器端到端 | Chromium 40/40 通过；Firefox 最终完整复跑 40/40 通过。此前一次 Firefox 矩阵曾出现 1 项测试夹具启动波动，该项自动重试及单项复跑均通过，最终完整复跑未复现 |
| 依赖安全 | RustSec 从本地 1170 条 advisory 数据扫描 251 个 Cargo 依赖，未发现漏洞；`npm audit --audit-level=high` 为 0 个漏洞 |
| 脚本与部署 | Shell/Node 语法、JavaScript/Markdown 门、SBOM 规范化、第三方 notice、私有 npm cache、发布目录原子 no-clobber 自测以及隔离 nginx 主动部署检查均通过 |
| 工作树完整性 | `git diff --check` 与 `git diff --cached --check` 通过；该轮工作树按整改任务预期尚未提交，因此不能满足正式发布的 clean-tag 前置条件 |
| 正式制品边界 | 该轮临时签名发布尝试因共享盘 99%/`ENOSPC` 在隔离 `cargo test` 阶段中止；本报告没有据此宣称完整签名发布链或正式 `v0.48.0` 制品已经通过 |

## 14. 最终一致性复核与保留边界

最终复核再次逐项对照了代码、回归测试、README、CHANGELOG、安全政策、运维手册、工作流、特性清单和浏览器专项报告。当前一致语义包括：目录遍历复核全部访问目录但不是原子 FS snapshot；ZIP 直接生成有 64 MiB 上限的预算化计划/索引且所有 waiter 有 owner；上传按“路径租约 → 上传许可 → 受跟踪 route metadata → owner state/mutation”进入，槽满不查旧 state 而返回 response-only `not-started`，Retry 必须先 HEAD；终态按账号隔离并区分 committed/rejected/歧义 running，state 与 partial stage 都以同 fd 分类，满偏移记录无条件作为歧义屏障；特权 metadata 拒绝和精确长度 xattr 内存预算；operation 的 Reserved/CommitStarted 生命周期；同 inode 硬链接 move 拒绝；purge 公平退避、有限失败占槽和周期重捕获；实际 temp device 的磁盘 revision/metadata/rounding 记账及乘法溢出失败关闭；解析中路径 waiter 的词法公平性与最终语义复验；未认证导航的精确 `Accept` 解析；30 + 10 秒停机硬截止、专用最终日志刷新线程、第二信号即时退出和 30 秒响应 write-idle；严格分离的 operation/upload 前端词汇；Acorn AST 防御纵深门、隔离 mandatory quality gate、exact-source、特殊树条目拒绝、强制 SPDX 表达式、SBOM、两类 notice 及主动 nginx 测试。

未发现仍未记录的同等级代码缺陷。保留项是明确的产品或部署边界，而不是本轮可以通过局部补丁消除的问题：

- 所有账号共享同一权限域，不提供互不信任租户之间的文件隔离；
- 仅支持 Linux 64 位及 `openat2`，每个共享根只允许一个实例；
- TLS、响应总时长/最低速率和公网入口策略依赖可信网关；应用只提供 30 秒 socket write-idle；
- 具有共享根写权限的本地进程或 virtiofs 宿主属于受信任边界；
- 遍历期目录复核不能提供原子文件系统快照；强一致导出需只读存储快照或等价版本化源；
- 首次停机信号约 40 秒后存在应用硬截止，届时跳过日志 flush 并立即终止，卡住的内核/存储提交和尾部日志可能丢失；正常完成 tracked cleanup 后由专用 OS thread 只做一次、最多 5 秒的日志 flush，再显式 `exit(0)`，不让 runtime drop 或阻塞池耗尽突破该承诺；flush 期间第二信号仍立即以 130/143 强退；
- 发布签名若与构建使用同一 UID，短命子进程和延迟打开只能缩小暴露窗口，不能形成安全隔离；正式签名仍应使用独立账号、主机或 HSM；
- 仓库现有只读分层 GitHub Actions 定义，但当前 checkout 没有 remote，尚无本轮托管运行记录；Acorn AST/词法常量门与 strict `checkJs` 仍不等价于完整跨过程污点分析或 ESLint，保守 Markdown 门不等价于通用 CommonMark 工具链，SBOM 规范化也不替代完整 CycloneDX schema validator；隔离质量门的 npm 缺失包/审计和无本地副本时的 RustSec 数据库仍可能需要受控网络。

最终综合评价：**9.2/10，具备受控环境发布条件。** 相比初始 7.8/10，已确认的代码级 High/Medium/Low 缺陷和工程发布阻断项均已关闭。正式发布仍须基于最终提交创建精确 `v0.48.0` tag，并在该干净 tag 上重新运行统一门禁和发布脚本。

## 15. 本轮全量修复与终态验收（2026-08-15）

本节记录用户要求“修复上述所有问题并同步文档”后、最终提交创建前的验收终态，并取代第 14 节的当前结论；前文仍作为初始发现、历史整改和设计取舍的追溯记录。本节验收时尚未创建 commit、tag 或正式 release，也没有改写用户已有的无关工作树内容。

### 15.1 已关闭问题

| 范围 | 最终实现 | 回归证据 |
| --- | --- | --- |
| 登录退避隔离 | 失败历史只按“来源 IP + 用户名 SHA-256 摘要”组合键累计；成功只清除对应组合，不能再由一个来源跨 IP 定向锁定同一账号 | 组合键限流 5 项单测及跨来源登录集成测试通过 |
| 登录输入边界 | CLI、公开哈希入口、配置和登录统一拒绝空密码及超过 1024 个 UTF-8 字节的密码；浏览器以 `TextEncoder` 校验，登录页不再用 UTF-16 `maxlength` 冒充字节上限 | Rust 边界测试、CLI 测试及 Chromium/Firefox 多字节密码测试通过；内联脚本 CSP SHA-256 与嵌入内容一致 |
| 登录限流 HTTP 语义 | 剩余时间向上取整；限流 POST 的 PRG `303` 只带一次性 Location，不再带会被解释为“延迟跟随重定向”的 `Retry-After`。首次 GET 原子消费记录并返回 `429 + Retry-After + 登录错误页`；刷新、无效 token 和普通凭据错误仍为 `200` | session 12 项定向单测、auth 14 项集成测试及双浏览器认证测试通过 |
| Cleartext HTTP/2 | HTTP/2 prior knowledge 和 h2c Upgrade 都不会得到 `101`，生产依赖仍只启用 HTTP/1 server | `tests/protocol.rs` 2 项通过 |
| 普通下载特殊文件替换 | `RootedFs::open_read` 使用 `O_NONBLOCK`；下载在同一已打开 fd 上强制普通文件分类，路径在分类后被替换为 FIFO 时不会阻塞 | GET/FIFO 单测及下载/Range/缓存回归通过 |
| ZIP 文件身份、容量与一致性 | 计划保存文件和目录的 dev、inode、mode、nlink、size、mtime/ctime 纳秒快照；普通文件非阻塞打开后同 fd 在复制前后复核类型与快照，目录在写 entry 前复核，finalize 前再次复核所有访问目录。临时文件创建/写入的实际 `ENOSPC`、`EDQUOT` 稳定映射为 `507` | 原子替换、FIFO、目录变化、真实 ZIP HTTP、归档解析及 StorageFull/QuotaExceeded 映射测试通过；对象变化统一返回可重试 `409` |
| 大目录排序取消 | 列表和搜索改用稳定的自底向上索引归并排序，在索引构造、每次合并选择、逆映射和每个置换步骤检查停机标志与 deadline | 排序中途取消、deadline 映射、1,205 项分页和排序集成测试通过 |
| 空文件 fresh PUT | 独立校验器只接受 `200/201 + committed + 同一 upload ID + 精确 length/offset`；`202/204` 等异常成功状态视为结果未知，仅用原 ID 做一次 HEAD，不重放 PUT；绑定 ID 的 `429 not-started` 保留已知未启动语义 | 完整状态码/状态矩阵、槽满、异常 2xx、单次 HEAD 和“不重放”浏览器测试通过 |
| 前端响应体资源上限 | Fetch 读取前检查 `Content-Length`，再用 `ReadableStream` 逐块累计；错误/成功正文硬上限分别为 16 KiB/16 MiB，越界立即 cancel。合法分块直接构造重放流，不再合并为第二份连续 `Uint8Array`；重建的 `Response` 可继续调用 `text()`/`json()`/`clone()`，只保留 body、status、statusText 和 headers，不保留原响应的 url、redirected 或 type。Problem Details 的 `detail`/`title` 最多接受 1024 个 JavaScript UTF-16 code units，超限整条丢弃；上传 XHR 在 header、progress 和最终 UTF-8 长度三处拒绝任何超过 16 KiB 的响应（正常成功响应应为空）。上传协议头、状态码及按当前文件总长度绑定的单一解析由共享模块集中维护 | 超限声明、超限流取消、约 12 MiB 合法 500 项 JSON、当前调用 API 保留、共享协议矩阵及 XHR 上限测试通过 |
| JavaScript 防御纵深门 | 动态 computed `ObjectPattern` 无法静态求值时一律失败关闭；覆盖变量声明、赋值表达式、默认参数、嵌套、const alias 及运行时传入 `globalThis` 的旁路。原生 `alert/confirm/prompt` 的直接、别名、计算属性和反射访问同样拒绝 | 20 个 JS 文件及内置正负对抗 fixture 通过 |
| JavaScript 类型与 Shell 门 | TypeScript 5.9.3 以 `allowJs + checkJs + strict + noEmit` 覆盖 `assets/index.js`、登录脚本和全部生产模块；外部/解析输入保持 `unknown` 并经守卫收窄，生产源码不保留显式或隐式 `any`；五个 Bash 源经 `bash -n`，ShellCheck 0.11.0 以 warning 级别检查 | `npm run check:types` 和官方 SHA-256 固定的 ShellCheck 0.11.0 本地验证均退出 0；类型门无需迁移 `.ts`，但不等价于 ESLint 或完整跨过程污点证明 |
| 只读远程 CI 定义 | GitHub Actions 只有 `contents: read`，checkout 不持久化凭据，Action 固定完整 SHA；静态、Rust、Chromium/Firefox 三层固定关键工具并记录 runner 环境，不签名或发布 | 工作流 YAML、本地等价静态命令和矩阵命令入口已检查；当前 checkout 无 remote，因此不宣称已有托管运行结果 |
| 发布签名算法 | allowlist 为 Ed25519、Ed448、RSA ≥3072 bit、ECDSA `prime256v1`/`secp384r1`/`secp521r1`；弱 RSA、DSA、`secp256k1`、X25519 和未知类型失败关闭。签名关键 Shell 命令显式传播失败，不依赖可能被 `if function` 抑制的 errexit | 发布自测覆盖允许/拒绝矩阵、签名输出失败、mode 输出失败、验签和 no-clobber，全部通过 |
| 发布来源持续复核与环境记录 | 隔离质量门后、签名前和公开前均复核 HEAD、tag、Cargo 版本和含未跟踪项的 clean worktree；`BUILD-ENVIRONMENT.txt` 记录完整 SHA/版本/epoch/target 和实际工具版本并进入包内 checksum，但不冒充全宿主链钉扎 | 发布自测覆盖版本不匹配、dirty worktree、环境字段/mode 和 checksum；运维文档逐字段验收 |
| 部署行为门 | 分别验证未知 SNI、合法 SNI 下未知 Host、伪造入站 XFF 被 `$remote_addr` 覆盖、连接限制和请求令牌桶在 `429` 后恢复 `200` | 隔离真实 nginx + mock upstream 主动检查通过；YAML/systemd/nginx 生产解析通过 |
| 桌面 400% 缩放回流 | 删除 `body` 的 538 px 最小宽度；不超过 537 CSS px 时工具栏换行、文件表格两行 grid 回流，Modified、Size 和操作按钮均保留 | Chromium/Firefox 在 320 CSS px 断言无页面横向溢出、操作区在视口内且关键信息可见 |
| SBOM source revision | 规范化器只接受恰为 40 或 64 位的小写十六进制对象 ID，不再接受 41–63 位伪“完整 SHA” | 39/41/63/65 位负例与递归规范化自测通过 |
| 文档一致性 | README、CHANGELOG、SECURITY、运维手册、工作流、特性清单及浏览器专项报告同步了协议状态、字节上限、排序检查点、ZIP 快照、签名 allowlist、部署断言、回流、JS 门和 SBOM 精确规则；安全响应头测试归属也校正为 Rust 精确断言 + 浏览器执行验证 | 8 个 Markdown 文件通过格式、本地链接、标题锚点、代码围栏和 symlink fail-closed 检查 |

### 15.2 最终验证结果

| 验收面 | 2026-08-15 实测结果 |
| --- | --- |
| Rust 格式与静态分析 | `cargo fmt --all -- --check` 通过；`cargo clippy --locked --all-targets --all-features -- -D warnings` 通过 |
| Rust 全量自动测试 | 全 targets/features 共 415 项通过、0 项失败；1 项十万目录手工基准按设计忽略 |
| Rust 覆盖率 | `cargo-llvm-cov 0.8.6` 行覆盖率 78.15%（17,053 行中 3,726 行未覆盖），通过 70% 强制门槛 |
| JavaScript 与文档 | 生产及测试 JavaScript 通过语法、格式、Acorn AST/词法常量安全门；全部生产 JavaScript 通过 TypeScript 5.9.3 strict `checkJs`，包括 `unknown` 收窄及无显式/隐式 `any`；全部 Markdown 文件通过项目文档门 |
| Shell 与远程门定义 | 5 个 Bash 文件通过 `bash -n` 和 ShellCheck 0.11.0 warning 门；只读 GitHub Actions 工作流通过本地 YAML/命令入口检查，尚未在无 remote 的当前 checkout 上托管执行 |
| 浏览器端到端 | Chromium 51/51、Firefox 51/51；无失败、无 flaky 假绿 |
| 部署 | 隔离真实 nginx 行为门通过，包括 SNI/Host 分离、XFF 覆盖、HTTP/1.1 回源、连接限制和限流恢复；Dufs YAML 与 systemd/nginx 语法通过 |
| 发布辅助链 | Shell 语法、构建环境清单字段/mode、exact-source 的版本/dirty 负例、递归 SBOM 规范化、第三方 notice、私有 npm cache 播种、签名算法矩阵及原子发布目录 no-clobber 自测通过 |
| 依赖审计 | 本地 RustSec 数据库 1,170 条 advisory 扫描 251 个 Cargo 依赖，未发现漏洞；`npm audit --audit-level=high` 为 0 个漏洞；锁定安装成功 |
| 工作树检查 | `git diff --check` 与 `git diff --cached --check` 通过 |

首次在受限沙箱内运行需要监听本地端口的部署、浏览器和覆盖率测试时，操作系统按环境策略返回 `EPERM`；在允许本地测试进程后，同一命令完整通过。它是执行环境限制，不是产品回归。

### 15.3 保留边界与发布结论

当前没有遗留的、与本轮已报告问题同等级的可操作代码缺陷。以下仍是有意保留的产品或交付边界：

- 所有账号共享同一文件权限域，不提供互不信任租户隔离；
- 生产仅支持 Linux 64 位、`openat2` 和每共享根单实例；
- TLS、公网入口、真实来源 IP 限速、响应总时长/最低速率仍依赖可信 HTTPS 网关；
- 具有共享根写权限的本地进程/宿主属于可信边界，目录前后复核和 ZIP 快照不能替代原子存储快照；
- Fetch 响应有严格流式字节上限；上传 XHR 受浏览器 API 约束，只能在 header/progress/最终文本阶段尽早中止，网关仍应限制异常响应大小；
- 320 CSS px 是桌面浏览器 400% 缩放回流验收，不改变“不支持手机 Web”的产品范围；Microsoft Edge 仍是安装后可选矩阵，本轮必需的 Chromium/Firefox 已全部通过；
- 远程 CI 使用 GitHub 托管 `ubuntu-24.04` 标签并记录实际 image version；它固定关键工具但不逐包固定宿主内核/系统库，也不替代本地覆盖率、审计、部署行为和 exact-tag 发布门；
- 本节验收时工作树按整改任务预期尚未提交，因此没有运行会要求 clean tree/exact tag 的正式制品发布流程。这里证明的是发布脚本自测和全部组成门禁通过，不宣称已经从真实 `v0.48.0` tag 生成正式签名制品。

终态判断：**本轮确认的全部代码、测试和文档问题均已关闭；本节验收结论为最终改动已具备提交条件。** 正式发布仍须基于最终提交创建与 Cargo 版本一致、精确指向该提交的 tag，然后从该干净 tag 重新执行 `scripts/check.sh` 与 `scripts/package-release.sh`。

## 16. 上传预检与条件覆盖终态（2026-08-20）

本节记录“提交前检查目标、提交时仍以原子条件复核、真正冲突时只对受影响文件二次确认”的当前实现，并取代前文“每批上传总是先确认”以及 schema v2 不迁移等历史口径。

| 审查面 | 当前实现与边界 |
| --- | --- |
| 有界预检 | 浏览器把文件转换为最终绝对逻辑路径后调用 `POST /__dufs__/api/upload/preflight`。每次必须有 1～512 个互不重复的路径，解码 UTF-8 路径总量最多 256 KiB，JSON wire body 最多 2 MiB；服务按原顺序返回 `path/exists/replaceable/revision`，前端严格绑定数量、顺序、路径和字段类型。预检只是观察，不冒充文件系统锁或原子快照。 |
| 确认交互 | 没有已存在目标时零确认直接上传；只有已存在且可替换的目标进入覆盖/跳过/取消对话框，不可替换目标不会被自动覆盖。预检后只有实际发生竞态的文件才再次提示，不要求用户手工检查当前目录列表，也不会让一个文件的冲突授权其他文件。 |
| 原子条件写 | 缺少 `X-Dufs-Upload-Overwrite` 或值为 `false` 使用原子 no-replace。值为 `true` 时必须同时携带 64 位小写十六进制 `X-Dufs-Target-Revision`；revision 绑定账号摘要、规范根内路径和完整 replacement CAS identity，并在 rename 紧前重新验证。旧 revision、非法响应或 `unknown` 都不能降级为无条件覆盖。 |
| 晚到冲突 | 完整 stage 在最终检查发现目标出现或 identity 改变时不会发布或删除，而是以同一 upload ID、满 offset 持久化为 `AwaitingConfirmation`，对外状态为 `awaiting-confirmation`。用户接受当前目标后，以同一 ID、满 offset、空正文 PATCH 和最新 revision 发布，通常无需重传；若目标再次变化，条件仍失败并再次确认。 |
| 明确跳过 | 用户跳过晚到冲突时，浏览器向 `POST /__dufs__/api/upload/discard` 发送精确路径和 upload ID。服务在 owner/path/id/state 全部匹配后删除保留 stage、记录 `Rejected` 并返回 `204`；UUID 本身不是跨账号或跨路径删除能力。 |
| metadata 安全例外 | 覆盖 stage 可能已经重放旧目标 uid/gid、mode 和允许的 xattr。若该目标随后消失，服务返回 `upload_metadata_preservation_refused`，拒绝用空 no-replace PATCH 把旧 metadata 发布为新文件；浏览器必须先 discard，再生成新 ID，以完整正文执行 create-only PUT。这个罕见分支有意以一次重传换取 metadata 语义正确。 |
| 状态与迁移 | 统一状态库当前为 SQLite schema v3。`upload_sessions` 新增 target revision 和 `AwaitingConfirmation`，完整状态为 `Running/CommitStarted/AwaitingConfirmation/Committed/Rejected/Unknown`；v2 是唯一支持的旧版本，并在一个 `BEGIN IMMEDIATE` 事务内迁移为 v3，其他版本零修改拒绝。过期 `Running/AwaitingConfirmation` 只有在 DB 行、路径和 stage identity 仍一致时才清理。 |
| 未知语义 | 网络中断、超时、非法状态、缺失/错误 revision、ID/长度/offset 不匹配和显式 `unknown` 均失败关闭。前端暂停剩余队列或按既有 HEAD-first 恢复协议处理，不会自动重发可能已经发布的请求，也不会自动覆盖当前目标。 |

这套设计把两类责任分开：预检负责减少不必要的对话框，no-replace/revision CAS 负责提交时正确性。它没有声称隔离拥有共享根写权限的外部进程，也没有把预检结果视为长期授权；任何目标变化都必须在最终条件提交中重新证明。
