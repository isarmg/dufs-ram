# 10. 源码阅读路线、练习与术语表

前九章按主题解释系统，本章把它们变成一条可以执行的学习计划。目标不是逐行读完仓库，而是建立“从用户行为定位到协议、代码和测试”的能力。

## 10.1 不推荐从最大的文件开始硬读

直接打开 [upload.rs](../../src/server/upload.rs) 或 [upload/manager.js](../../assets/modules/upload/manager.js) 从第一行读到最后，通常会同时遇到路径、状态、协议、DOM、取消和恢复，难以建立主线。

更有效的方法是：

1. 选一个外部行为；
2. 找它的浏览器触发点；
3. 记录 HTTP 方法、路径、头和正文；
4. 找 Router 分支；
5. 追到领域操作与持久化边界；
6. 找对应测试；
7. 用测试验证自己的理解。

## 10.2 推荐的十次阅读

每次只解决一组问题。第一次可以每次安排 45～90 分钟。

### 第 1 次：产品和启动

阅读：

1. [README.md](../../README.md) 的支持范围、环境要求和快速开始；
2. [Cargo.toml](../../Cargo.toml)；
3. [src/lib.rs](../../src/lib.rs)；
4. [src/main.rs](../../src/main.rs) 的 `main`、`serve` 和 shutdown；
5. [src/args.rs](../../src/args.rs) 的参数结构与校验入口。

回答：

- 开发需要哪些工具，生产需要哪些？
- `Args` 与校验后的配置有什么区别？
- 进程何时才开始接受连接？
- 第一次和第二次停止信号有什么不同？

### 第 2 次：最短的 HTTP 路径

从 `GET /__dufs__/health` 开始：

1. [src/main.rs](../../src/main.rs) 中连接交给 Hyper；
2. [src/server/router.rs](../../src/server/router.rs) 的 `Server::call`；
3. [src/server/router/request.rs](../../src/server/router/request.rs) 的请求画像；
4. [src/server/router/dispatch.rs](../../src/server/router/dispatch.rs) 的公共路由。

回答：

- 哪些工作在认证之前发生？
- 为什么 health 不访问文件内容？
- 哪些响应可以长期缓存，哪些必须 no-store？

### 第 3 次：登录与会话

阅读：

- [src/auth.rs](../../src/auth.rs)；
- [src/server/session.rs](../../src/server/session.rs)；
- [src/server/login_rate_limit.rs](../../src/server/login_rate_limit.rs)；
- [assets/login.html](../../assets/login.html)；
- [assets/login.js](../../assets/login.js)；
- [tests/auth.rs](../../tests/auth.rs) 与 [tests/frontend/auth.spec.js](../../tests/frontend/auth.spec.js)。

回答：

- 登录为何用标准表单和 `303`，不是前端 fetch？
- Cookie 认证与 CSRF 各解决什么？
- 为什么修改 `login.js` 可能还要更新 CSP 哈希？
- 应用和 nginx 的登录限流如何叠加？

### 第 4 次：目录页面与列表

阅读：

- [src/server/listing.rs](../../src/server/listing.rs)；
- [src/server/listing/snapshot.rs](../../src/server/listing/snapshot.rs)；
- [src/server/listing/walk.rs](../../src/server/listing/walk.rs)；
- [assets/index.html](../../assets/index.html)；
- [assets/modules/app.js](../../assets/modules/app.js)；
- [assets/modules/listing/controller.js](../../assets/modules/listing/controller.js)。

回答：

- HTML 初始数据和文件项数据分别来自哪里？
- cursor 绑定哪些请求事实？
- DOM 上限 1000 与后端快照上限有什么区别？
- 写操作后为什么不能继续使用旧 cursor？

### 第 5 次：单文件下载

阅读：

- [src/server/download.rs](../../src/server/download.rs)；
- [tests/range.rs](../../tests/range.rs)；
- [tests/cache.rs](../../tests/cache.rs)；
- [tests/http.rs](../../tests/http.rs) 中下载相关场景。

回答：

- 为什么下载始终 attachment？
- 为什么只支持一个单段 Range？
- 打开文件后外部追加内容，当前响应为何不会无限增长？
- ETag 描述的是内容还是对象 metadata？

### 第 6 次：路径安全

阅读顺序：

1. [path_policy.rs](../../src/server/path_policy.rs)；
2. [path_coordinator.rs](../../src/server/path_coordinator.rs)；
3. [rooted_fs.rs](../../src/server/rooted_fs.rs) 中的运行时文件 identity 类型；
4. [state_store/model.rs](../../src/server/state_store/model.rs) 中的持久化文件 identity；
5. [tests/symlink.rs](../../tests/symlink.rs)；
6. [rooted_fs/tests.rs](../../src/server/rooted_fs/tests.rs)。

[identity.rs](../../src/server/identity.rs) 名字容易误导：它只定义账号的 `OwnerId` 摘要，不定义文件系统对象 identity，适合在阅读状态 owner 隔离时再看。

先画出：

```text
URI → RoutePath → RootedPath → 根 FD 相对打开 → fstat identity
```

回答：词法验证、进程内租约和内核路径解析各自解决什么，为什么不能互相替代？

### 第 7 次：一个普通写操作

推荐先读重命名：

- [assets/modules/operations/file_operations.js](../../assets/modules/operations/file_operations.js)；
- [assets/modules/http/client.js](../../assets/modules/http/client.js)；
- [src/server/browser_api.rs](../../src/server/browser_api.rs)；
- [src/server/operation_registry.rs](../../src/server/operation_registry.rs)；
- [src/server/problem.rs](../../src/server/problem.rs)；
- [tests/browser_api.rs](../../tests/browser_api.rs)。

回答：

- Move 和 Rename 为什么是两个 API，却共用底层迁移引擎？
- Operation ID 如何绑定请求内容？
- 哪个时刻以后超时只能 unknown？
- committed、outcome-unknown、refresh-required、not-committed 如何影响列表？

### 第 8 次：删除和 SQLite actor

阅读：

- [src/server/delete.rs](../../src/server/delete.rs)；
- [src/server/purge.rs](../../src/server/purge.rs)；
- [src/server/rooted_fs/purge.rs](../../src/server/rooted_fs/purge.rs)；
- [src/server/state_store.rs](../../src/server/state_store.rs)；
- [src/server/state_store/actor.rs](../../src/server/state_store/actor.rs)；
- [src/server/state_store/database.rs](../../src/server/state_store/database.rs)；
- [src/server/state_store/model.rs](../../src/server/state_store/model.rs)；
- [src/server/state_store/purge.rs](../../src/server/state_store/purge.rs)。

画出 `Prepared → Ready → Claimed`，并推演每条边发生前后崩溃时如何恢复。

### 第 9 次：上传

不要一次读完。分三轮：

1. 前端选择、preflight、队列；
2. PUT 准备、stage 和传输；
3. checkpoint、PATCH、冲突、提交与 unknown。

对应文件见[上传协议代码地图](07-upload-protocol.md#73-代码地图)。每读一个状态，立即在 [src/server/upload/tests.rs](../../src/server/upload/tests.rs) 和 [tests/frontend/upload.spec.js](../../tests/frontend/upload.spec.js) 找断言。

### 第 10 次：测试与生产边界

阅读：

- [scripts/check.sh](../../scripts/check.sh)；
- [scripts/check-js.mjs](../../scripts/check-js.mjs)；
- [scripts/check-docs.mjs](../../scripts/check-docs.mjs)；
- [playwright.config.js](../../playwright.config.js)；
- [tests/support/fixtures.rs](../../tests/support/fixtures.rs)；
- [deploy/dufs.service](../../deploy/dufs.service)；
- [deploy/nginx-dufs.conf](../../deploy/nginx-dufs.conf)；
- [docs/operations.md](../operations.md)。

回答：每类错误由哪一层测试最便宜地发现？哪些生产事实无法由单元测试证明，需要部署检查和恢复演练？

## 10.3 从用户动作反查代码

| 用户动作/现象 | 前端起点 | 后端起点 | 先看测试 |
| --- | --- | --- | --- |
| 登录 | `login.html/js` | `session.rs` | `tests/auth.rs`、`auth.spec.js` |
| 打开目录 | `app.js`、`listing/controller.js` | `listing.rs` | `tests/pagination.rs`、`browse.spec.js` |
| 点击文件下载 | `listing/controller.js` | `download.rs` | `tests/range.rs` |
| 点击房子回根目录 | `app.js` | 普通目录 GET | `browse.spec.js` |
| 新建文件夹 | `operations/file_operations.js` | `browser_api.rs` mkdir | `browser_api.rs` 测试、`operations.spec.js` |
| 新建空文件 | `operations/file_operations.js` | `upload.rs` PUT | upload 测试 |
| 行内重命名 | `listing/controller.js`、`operations/file_operations.js` | `browser_api.rs` rename | `operations.spec.js` |
| 移动 | `operations/file_operations.js` | `browser_api.rs` move | `browser_api.rs`、`operations.spec.js` |
| 删除 | `operations/file_operations.js` | `delete.rs`、`purge.rs` | delete/purge 集成测试 |
| 上传重名确认 | `upload/manager.js` | `browser_api.rs` preflight、`upload.rs` | `upload.spec.js` |
| 页面仍显示旧界面 | asset URL/cache | `assets.rs` | `tests/assets.rs` |
| 操作 unknown | `http/client.js` | router/protocol/operation registry | browser API 与前端 unknown 测试 |
| ready 503 | 页面/探针客户端 | `server.rs`、RootedFs、StateStore | `tests/health.rs` |

## 10.4 从错误关键词反查

机器可读 code、协议头和状态值比界面英文更适合搜索：

```sh
rg "path_exists" src assets tests
rg "OperationPublicState|operation_state|x-dufs-operation-state" src assets tests
rg "awaiting-confirmation" src assets tests
rg "X-Dufs-Upload-Offset" src assets tests
```

建议记录搜索结果中的四个位置：

1. 常量或枚举定义；
2. 服务端产生位置；
3. 前端解析位置；
4. 测试断言位置。

若一个字符串只在 UI 文案里出现，不要用它驱动安全决策。

## 10.5 一次修改应该怎样缩小范围

假设需求是“给重命名增加一个名称规则”，先画影响面：

```text
用户输入
  → 前端即时验证
  → JSON request
  → 后端权威验证
  → 路径类型
  → 文件系统提交
  → Problem code
  → 行内错误显示
  → Rust + JS + Playwright 测试
```

然后判断：

- 这是展示规则，还是安全不变量？
- API 客户端绕过前端时后端是否仍拒绝？
- 新规则是否按字符还是 UTF-8 字节？
- Move 和 Rename 是否应共用同一个 basename validator？
- 是否影响新建默认名和覆盖对话框？
- 旧 Operation ID replay 是否仍得到同一结果？

先写或更新最接近不变量的测试，再实现最小变更。

## 10.6 练习项目：从易到难

以下练习适合在临时分支和可丢弃共享根中完成。

### 练习 A：修改非安全文案

修改一个普通状态提示：

1. 在前端查找文案；
2. 确认它不参与状态判断；
3. 修改对应测试；
4. 运行 JS、类型、单元和定向 Playwright；
5. 重新编译；若修改的是 `EMBEDDED_ASSETS` 哈希白名单中的 CSS、ES module 或图标，确认资源摘要变化，否则核对新 document 和适用的 CSP。

学习点：资源嵌入、缓存和测试选择。

### 练习 B：增加只读列表字段

给列表响应增加一个明确可计算、无敏感性的字段：

1. 修改 Rust response struct；
2. 在前端以 `unknown` 接收并验证；
3. 决定旧服务/旧页面兼容策略；
4. 更新 Rust API、前端单元和浏览器测试。

学习点：wire schema、运行时校验和渐进兼容。

### 练习 C：增加稳定错误 code

为一个现有失败原因拆出类型化 code：

1. 在 `problem.rs`/领域错误中定义；
2. 映射 HTTP 状态和恢复建议；
3. 前端按 code 处理，不匹配 message；
4. 增加正反测试。

学习点：避免字符串驱动协议。

### 练习 D：新增普通 mutation

只在充分理解后尝试：

1. 定义独立路由与请求 schema；
2. 路径策略和输入预算；
3. Operation ID + fingerprint；
4. 路径租约与 StateStore 冲突；
5. detached commit 和 unknown；
6. 原子文件系统操作与 fsync；
7. 统一列表失效；
8. Rust、前端和停机/超时测试。

学习点：写操作不是“加一个 handler”就结束。

## 10.7 代码审查提问清单

### 输入与类型

- 原始请求数据是否先按 `unknown`/未验证类型处理？
- 大小、数量、深度和 UTF-8 字节预算是否在分配前检查？
- 是否用明确 enum/struct 代替自由字符串和松散对象？
- URL path、逻辑根内 path 与 OS path 是否分开？

### 并发与取消

- 这个锁、租约或信号量保护的是哪种资源？
- 是否跨 `.await` 持有不该长时间占用的锁？
- 外部进程不受 PathCoordinator 控制时，这个提交使用 `RENAME_NOREPLACE`，还是只做 identity 复核后普通 rename；剩余竞争窗是什么？
- Future 取消后，副作用是否可能继续？

### 持久化

- 原子可见点在哪里？
- 文件和父目录何时 `fsync`？
- SQLite 最后可靠状态是什么？
- 文件系统成功、SQLite 失败时返回什么？
- 重启恢复是否有足够 identity，不会按名字猜？

### 协议

- 相同 ID 的相同请求能否安全重放？
- 相同 ID 的不同请求是否拒绝？
- HTTP status、头和 JSON 是否联合验证？
- timeout/5xx 是否错误地一律标成可重试？
- 前端是否从 message 文本猜业务状态？

### 前端

- 是否使用 `textContent`/安全 DOM 创建，不动态注入 HTML？
- DOM、队列和历史是否有上限？
- 不支持的操作是否保留固定布局槽？
- 写操作是否统一通知列表失效？
- 焦点、键盘、读屏器、forced-colors 和 320px 回流是否保持？

### 测试与运维

- 最便宜的单元测试是否覆盖纯逻辑？
- Rust 集成测试是否覆盖真实 HTTP/文件系统边界？
- Playwright 是否只承担浏览器才能证明的行为？
- 文档、配置和部署样例是否同步？
- 失败是否可通过日志和 ID 定位？

## 10.8 术语表

### A

**actor**
一个拥有内部状态、通过消息顺序处理命令的执行单元。Dufs 的 SQLite connection 由专用 StateStore actor 线程拥有。

**ACL**
Access Control List，文件系统的额外访问控制条目。覆盖文件时需要谨慎保留。

**Argon2id**
面向密码的哈希算法，结合内存和计算成本减慢离线猜测。配置保存 PHC 字符串，不保存明文密码。

**atomic / 原子**
外部观察者看到操作完成或未发生，而不是中间一半。原子可见不自动等于掉电持久。

### B

**backpressure / 背压**
下游变慢时让上游等待，避免无限缓冲数据。文件上传和下载都需要背压。

**basename**
路径最后一个名称，如 `/a/report.pdf` 的 basename 是 `report.pdf`。Rename 只改变 basename。

**blocking / 阻塞**
调用期间占住 OS 线程直到完成。SQLite 和某些文件系统调用即使写在 async 项目中仍可能阻塞。

### C

**CAS**
Compare-And-Swap 的一般思想：只有当前事实仍等于预期版本时才提交。上传 revision 是一种 CAS 凭据。

**checkpoint / 检查点**
已同步并记录、可安全作为续传起点的上传 offset，不等于浏览器最后发出的字节数。

**CSP**
Content Security Policy，限制页面可执行脚本和加载资源。登录页内联脚本使用固定摘要许可。

**CSRF**
Cross-Site Request Forgery，恶意站点借登录 Cookie 诱导浏览器发送写请求。CSRF token 与来源校验用于防护。

**cursor**
分页继续凭据。Dufs cursor 绑定账号、目录快照、查询、排序和页大小，不只是数组下标。

### D

**detached commit**
已交给受跟踪任务、即使 HTTP 等待者离开也会继续完成或收尾的任务所有权边界。它早于持久化 `CommitStarted` 和磁盘 rename；“detached”不表示文件已经提交，只表示 Router 不能再靠取消 HTTP future 来证明没有副作用。

**device/inode**
Linux 中识别文件系统对象的重要字段。路径名变化不一定改变 inode，名字相同也可能已经换成新 inode。

**durable / 持久**
按操作系统和存储同步语义，成功后应能跨崩溃保存。依赖正确的写入、`fsync` 和目录同步顺序。

### E

**ES Modules**
浏览器原生的 `import`/`export` JavaScript 模块系统。本项目没有生产前端 bundler。

**ETag**
HTTP 资源版本标识。下载 ETag 用对象 metadata 构造，不应自动理解为正文加密摘要。

### F

**FD**
File Descriptor，Linux 进程引用已打开内核对象的句柄。RootedFs 从共享根 FD 相对访问文件。

**fingerprint**
对方法、端点和请求正文等生成的稳定摘要，用于确认同一 Operation ID 是否仍代表完全相同请求。

**flock**
Linux 文件锁机制。Dufs 在共享根 FD 上取得非阻塞独占锁，避免同一根的第二实例。

**fsync**
请求操作系统把文件数据或目录 metadata 同步到持久存储语义。调用成功仍依赖底层存储正确兑现。

**Future**
Rust 中表示将来可能完成的异步计算。丢弃 Future 不会撤销已经发生的外部副作用。

### H

**HTTP Range**
请求单个文件部分字节的下载机制。当前只支持单个单段范围，与上传 PATCH 续传无关。

**Hyper**
本项目使用的底层 Rust HTTP 库，负责 HTTP/1.1 连接和请求/响应基础能力。

### I

**idempotency / 幂等性**
重复同一逻辑操作不产生额外副作用。Dufs 用 owner + ID + fingerprint 提供受限重放语义。

**identity / 身份快照**
描述一个文件系统对象当时是谁的字段集合，用于在提交前发现路径已经指向另一对象。

### J

**JSDoc checkJs**
在 `.js` 注释中声明类型，再由 TypeScript 静态检查。它不生成代码，也不能代替网络输入运行时校验。

### L

**liveness**
进程是否还活着并能响应。Dufs 的公开 `/__dufs__/health` 是 liveness。

### M

**metadata**
文件内容以外的属性，如类型、大小、owner、mode、时间、ACL 和 xattr。

**mutation**
会改变共享根或持久状态的操作，如上传、移动、重命名、删除。

### N

**no-replace**
提交瞬间要求目标不存在的原子策略，不等于先检查一次“当前不存在”。

### O

**Operation ID**
普通写操作的 UUID，用于登记、去重、结果查询和响应丢失后的对账。

**outbox**
先持久记录待执行副作用，再由 worker 可靠消费的模式。删除 purge job 是文件系统与 SQLite 之间的 durable outbox。

**owner digest**
按具体用途从账号生成的不可逆 `OwnerId`。operation/upload/purge 的持久记录使用兼容历史格式的 `OwnerId::persistent`；列表快照和登录限流使用各自 domain-separated 摘要。它们避免在这些边界直接保存或比较用户名，但不能混成一个全局通用摘要。

### P

**PathCoordinator**
当前 Dufs 进程内的路径租约协调器，处理相同、父子和部分别名冲突；不能控制外部进程。

**PHC string**
保存密码哈希算法、参数、盐和结果的标准文本格式，如 `$argon2id$...`。

**preflight**
上传前对目标当前状态的批量观察。它不是锁，也不是最终覆盖保证。

**Problem Details**
结构化 HTTP 错误表示。本项目增加稳定 code、恢复建议和状态，避免客户端解析错误句子。

**purge**
把已移入隐藏 trash 的删除目标在后台分批真正移除。

### R

**RAII**
资源获取即初始化；值离开作用域时自动释放或执行守卫收尾。

**readiness**
服务当前是否具备接收业务的关键条件。`/__dufs__/ready` 会实际探测共享根和 SQLite 写路径。

**revision**
上传覆盖确认使用的不透明目标版本 token，绑定账号、路径和 identity，不等同于内容哈希。

**rollback journal**
SQLite 的事务日志模式之一。活跃事务期间只复制主 DB 文件不能构成可靠备份。

**RootedFs**
以共享根 FD 为锚、使用 Linux 受约束系统调用执行文件操作的安全边界。

**RootedPath**
词法上已证明属于共享根命名空间的路径类型；它还不是内核级路径安全保证。

**RoutePath**
已按 HTTP 路由规则解码、规范化的路径类型。

### S

**secure context**
浏览器认为安全的上下文，通常是 HTTPS。`crypto.randomUUID()` 等能力和安全会话设计依赖它。

**Semaphore / 信号量**
持有有限许可的并发控制工具，适合表达“最多同时 N 个”，不等于路径锁。

**snapshot / 快照**
一次列表扫描形成的不可变结果集。后续 cursor 对它分页，目录变化会使其失效。

**stage**
上传正文先写入的同目录隐藏暂存文件，完整同步并通过最终检查后原子发布。

**StateStore**
管理 operations、upload sessions 和 purge jobs 的持久状态 façade/actor，不保存用户文件正文。

### T

**Tokio**
Rust 异步运行时，提供任务调度、网络、定时器、信号和同步原语。

**TOCTOU**
Time Of Check To Time Of Use：检查与使用之间事实被并发改变的竞争。

### U

**unknown**
系统当前无法可靠证明操作成功，也无法证明未发生的结果分类。持久化 Operation/Upload 记录中的 `Unknown` 是终态；Router 在普通 detached operation 或已经跨过首次 mutation boundary 的上传 task 超时时先返回的 HTTP `unknown`，则只是当时观察，后台工作随后仍可能收束。上传仅仅进入 tracked task 还不够：若总 deadline 在首次 filesystem/upload-state mutation 前先关闭原子边界，task 会被 abort 并返回 `408 not-started + retry`；只读准备未处理 I/O 也是 `408/503 not-started + retry`。无论 definite not-started 还是 unknown，都要先按原 ID 查询，不能直接换 ID 自动重放。

**upload ID**
单个上传会话的 UUID，绑定 owner、路径、长度、stage identity、offset 和状态。

### X

**xattr**
Extended Attribute，Linux 文件扩展属性。覆盖重放时必须限制特权命名空间和内存预算。

## 10.9 常见问题

### 这个项目是前后端分离吗？

源码按前后端模块分开，但部署不是两个服务。前端资源编译进 Rust 二进制，由同一后端返回。

### 修改 `assets/` 后为什么刷新没变化？

运行中的旧二进制仍包含旧资源。重新 Cargo 构建、重启服务并重新取得页面。若修改的是 `EMBEDDED_ASSETS` 白名单中的 CSS、ES module、图标或其 MIME 声明，再确认页面请求了新的资源摘要 URL；`index.html`、`login.html` 和内联 `login.js` 不参与该摘要，修改它们时摘要前缀可以不变。

### 为什么不用 React/Vue？

当前页面规模和功能边界适合原生 ES modules。减少生产构建链的同时，也要求项目自己严格管理状态、DOM 和运行时数据校验。

### SQLite 是文件索引数据库吗？

不是。列表直接来自共享根；SQLite 只保存普通操作、上传和删除清理的控制状态。

### 为什么不直接相信路径字符串？

字符串检查无法阻止检查后符号链接或目录项被替换。真正操作必须从根 FD 按内核约束解析，并按具体操作检查类型或 identity；外部 writer 不受进程内 PathCoordinator 控制。当前列表 revision 会由 DELETE 的 `If-Match`、Move/Rename 的 `source_revision`（覆盖时再加 `destination_revision`）带回，RootedFs 在紧邻 rename 时复核完整身份，但最后 `statat → renameat2` 的微窗仍要求部署侧排除其他 writer。

### 为什么 Move 和 Rename 不合成一个按钮？

用户语义不同：Move 只改变父目录，Rename 只改变 basename。后端公开 API 也独立，但共用底层安全迁移，避免复制复杂提交逻辑。

### 为什么目录没有 Download，后面的按钮还留空？

每行固定 `Move | Download | Delete | Rename` 四个槽。目录不支持单文件下载时保留空槽，避免其他按钮横向跳动。

### 点击新建后按 Escape，为什么 `newfolder` 还在？

当前交互是先真正创建默认名，再进入行内重命名。Escape 只取消改名，不撤销已经提交的新建操作。

### 上传为何有时会再次、甚至多次确认覆盖？

第一次基于 preflight；若真正提交时目标又出现或变化，后续确认针对最新 revision。若确认期间 revision 再次变化，这个过程还可能重复。它避免静默覆盖并可复用已经完整上传的 stage。

### 网络断开时一次 PUT 会失败吗？

浏览器请求会中断，但服务端可能尚未开始、保留了 checkpoint、已经提交，或结果未知。必须查询同一 upload ID，不能把网络错误直接等同于未上传。

### 为什么删除成功后磁盘空间没有立即回来？

目标先原子移入隐藏 trash，用户路径立即消失；purge worker 随后分批真正删除，大目录释放空间可能滞后。

### `unknown` 能不能直接再试一次？

不要换新 ID 把它当成新意图重做。先用原 Operation ID 查询 job，或用原 Upload ID 发 HEAD，再刷新目录并按恢复建议处理。普通写操作以同一 Operation ID 和完全相同 fingerprint 精确重放会被去重或重放保存结果，不会再次执行副作用，但若保存结果本身是 unknown，它也未必能消除不确定性；记录过期后更不能依赖旧 ID 盲重做。

### health 正常为什么 ready 失败？

health 只证明 HTTP 活着；ready 还实际测试共享根和 SQLite 写入、同步、删除、空间和停机状态。

### 可以同时运行两个实例提高可用性吗？

不能让两个进程管理同一共享根。根锁和单进程协调模型有意拒绝这种部署；代理自动把 mutation 切到另一个实例也不安全。

### 能把 state-dir 复制到另一台机器继续断点上传吗？

不能直接这样做。数据库绑定共享根 device/inode，上传记录还绑定 stage identity。新根通常应使用新的空 state-dir。

## 10.10 学完后的验收标准

如果你能独立完成下面任务，就已经从“能运行”进入“能维护”：

- 从浏览器按钮追踪到 HTTP 请求、Router、领域方法、文件系统提交和测试；
- 解释 `RoutePath`、`RootedPath`、`RootedFs` 的差别；
- 解释 Operation ID、upload ID 和 revision 的不同用途；
- 分别列出普通 Operation 的 `running/succeeded/failed/rejected/unknown`，以及 Upload 的 `running/awaiting-confirmation/committed/rejected/not-seen/not-started/unknown`；
- 画出删除 outbox 与上传 stage 的重启恢复路径；
- 为前端网络 JSON 写运行时守卫，而不是用 `any` 绕过检查；
- 修改写操作时同时维护列表失效、错误协议和无障碍焦点；
- 根据改动范围选择 Rust、Node、Playwright、部署和完整门禁；
- 说明 health、ready、备份和恢复演练分别证明什么；
- 遇到不确定提交时保全证据，而不是盲目重试。

完成这些后，可以继续阅读更紧凑、更接近设计规范的[项目工作流程](../project-workflow.md)和[功能取舍清单](../feature-inventory-and-tradeoffs.md)。
