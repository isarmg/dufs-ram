# 03. 本项目需要的 Rust 与 Web 基础

这一章不是完整的 Rust 或 HTTP 教科书，只解释阅读 Dufs 时会反复遇到的概念。已经熟悉 Rust Web 开发的读者可以跳到[后端请求完整旅程](04-backend-request-lifecycle.md)。

## 3.1 从“程序”到“进程、线程、任务”

- **程序**：磁盘上的 `dufs` 可执行文件；
- **进程**：启动 `dufs` 后，操作系统中正在运行的实例；
- **OS 线程**：由操作系统调度的执行线程；
- **异步任务**：由 Tokio 在少量工作线程上轮流推进的 Future；
- **阻塞操作**：调用后可能长时间不返回，使当前 OS 线程无法做其他工作。

Dufs 的网络连接主要运行在 Tokio 异步任务中，SQLite 则有专用 OS 线程。部分文件系统工作会通过 [blocking_io.rs](../../src/server/blocking_io.rs) 进入有界的 blocking task。原因是：`async fn` 只允许函数在等待时让出执行权，并不会自动把一个阻塞的系统调用变成非阻塞；请求等待者被取消后，已经开始的阻塞系统调用也不会因此自动停止，所以 permit 会一直保留到真实 worker 退出。

```text
Tokio 工作线程：HTTP、定时器、信号、异步协调
专用状态线程：SQLite 命令
阻塞任务池：可能阻塞的文件系统工作
```

如果把慢 SQLite 查询直接放进普通异步任务，承载大量连接的 Tokio 工作线程可能一起被堵住。

## 3.2 Cargo 包和 Rust 模块

[Cargo.toml](../../Cargo.toml) 定义一个名为 `dufs` 的 package。该 package 同时包含：

- `src/lib.rs`：库 crate，公开模块和可复用类型；
- `src/main.rs`：二进制 crate，生成 `dufs` 可执行文件。

`mod` 声明模块，`pub` 决定其他模块是否可见：

```rust
pub mod server;
```

Rust 允许同一个类型的实现分布在多个文件。例如 [server/download.rs](../../src/server/download.rs) 和 [server/upload.rs](../../src/server/upload.rs) 都可以包含 `impl Server`。它们不是继承关系，而是在不同功能文件中为同一个 `Server` 类型实现方法。

阅读模块时先看 [src/lib.rs](../../src/lib.rs)，再看 [src/server.rs](../../src/server.rs) 顶部的 `mod` 声明，能快速知道文件如何进入编译。

## 3.3 变量、借用和所有权的最小直觉

Rust 的核心规则是：每个值有明确所有者；借用允许临时读取或修改它，但不能制造失控的悬空引用或数据竞争。

常见形式：

```rust
fn inspect(path: &Path) { /* 只借用 */ }
fn update(path: &mut PathBuf) { /* 可变借用 */ }
fn consume(path: PathBuf) { /* 取得所有权 */ }
```

Dufs 中经常把已验证的路径、请求正文或 permit 移入异步任务。这样任务拥有自己完成工作所需的数据，即使原调用栈已经返回，也不会引用失效内存。

### `String` 与 `&str`

- `String` 拥有一段可增长的 UTF-8 文本；
- `&str` 只是借用一段 UTF-8 文本。

文件名长度限制常按 UTF-8 **字节**计算，而不是按用户看到的字符数计算。一个汉字通常占 3 字节。

### `PathBuf` 与项目路径类型

`PathBuf` 是操作系统路径容器，但 Dufs 不会让任意 `PathBuf` 直接成为共享根内操作凭据。项目还定义 `RoutePath`、`RootedPath` 等类型，把“已经完成哪一步验证”编码进类型。详见第 5 章。

## 3.4 `struct`、`enum` 和类型化状态

### `struct`

结构体把相关字段组织成一个类型：

```rust
struct UploadCheckpoint {
    offset: u64,
    length: u64,
}
```

项目使用结构体表达配置、请求上下文、文件身份、上传记录等。读结构体时先判断每个字段是“原始输入”还是“已验证事实”。

### `enum`

枚举表达有限种互斥情况：

```rust
enum OperationPublicState {
    Running,
    Succeeded,
    Failed,
    Rejected,
    Unknown,
}
```

与任意字符串相比，枚举让编译器要求调用者处理所有分支，也避免把 `"suceeded"` 之类的拼写错误带到运行时。项目在 Rust 内部大量使用类型化状态，到 HTTP 边界才转换为稳定字符串。

### `match`

`match` 对枚举或模式逐项处理：

```rust
match state {
    OperationPublicState::Succeeded => { /* ... */ }
    OperationPublicState::Failed | OperationPublicState::Rejected => { /* ... */ }
    OperationPublicState::Running | OperationPublicState::Unknown => { /* ... */ }
}
```

当枚举新增成员时，没有兜底分支的 `match` 会编译失败，这是一种有价值的维护提醒。

## 3.5 `Option` 和 `Result`

### `Option<T>`：可能没有值

```rust
enum Option<T> {
    Some(T),
    None,
}
```

例如后续分页可能没有 `next_cursor`。Rust 不使用任意 `null` 代替所有缺失情况，而是要求显式处理。

### `Result<T, E>`：成功或失败

```rust
enum Result<T, E> {
    Ok(T),
    Err(E),
}
```

几乎所有文件系统、数据库、解析和网络操作都可能失败。`?` 表示：成功就取出值，失败就把错误转换后提前返回。

```rust
let metadata = file.metadata()?;
```

它不是忽略错误，而是在类型允许的情况下把错误交给上一层。

Dufs 最终把内部错误映射为：

- HTTP 状态码；
- 稳定的机器可读错误 code；
- 用户可显示的安全消息；
- 可选恢复建议；
- 操作或上传状态头。

前端不应根据英文错误句子猜测下一步。

## 3.6 RAII：离开作用域时自动收尾

RAII 可以理解为“一个值活着时代表资源被持有，值被销毁时自动释放”。常见例子：

- 文件对象销毁时关闭文件描述符；
- `SemaphorePermit` 销毁时归还并发许可；
- 锁 guard 销毁时解锁；
- `OperationGuard` 在异常退出时撤销预留或标记未知。

这比要求每个错误分支都手写 `release()` 更可靠。但 RAII 只能执行类型 `Drop` 中能够安全完成的收尾；需要异步等待、磁盘同步或跨数据库事务的恢复仍要显式设计。

## 3.7 `Arc`、锁、信号量和 channel

### `Arc<T>`

`Arc` 是线程安全引用计数。`Arc<Server>::clone()` 只增加引用计数，不会复制整个服务器、缓存或数据库。

多个连接共享同一个 `Server`，是因为它们需要共享会话表、列表快照、路径协调器和关闭状态。

### `Mutex` / `RwLock`

锁保护一段共享内存，使多个任务不会同时不安全地修改它。锁的范围应小，不能拿着普通同步锁等待慢 I/O。

### `Semaphore`

信号量是一组有限许可。Dufs 用不同信号量限制连接、上传、搜索、密码计算等资源。它表达“最多同时有 N 个”，不是同一路径的互斥，也不是磁盘事务。

### channel

channel 是消息队列。StateStore 把类型化命令发送给专用 SQLite 线程，再通过 oneshot 返回本次结果：

```text
普通请求任务 --Command--> 有界 commands 队列 --> SQLite actor
普通请求任务 <--Result---- oneshot <-----------+

Drop/关闭路径 --Abandon/Shutdown--> 独立 control channel
                                     + Wake 唤醒 actor
```

图中的有界队列是普通命令通道。reservation 的 `Abandon` 和 actor 的 `Shutdown` 走独立 control channel，并在需要时向普通通道发送 `Wake`，因此普通队列满时仍保留清理和关闭路径。普通队列有界很重要：如果生产命令的速度长期大于消费速度，无界队列会把压力变成持续增长的内存。

## 3.8 `async`、`.await` 和取消

调用 `async fn` 得到 Future。`.await` 推进它，遇到尚未完成的异步事件时允许当前线程处理其他任务。

```rust
let response = server.call(request).await;
```

Future 被丢弃通常代表调用者不再等待，但这不等于外部世界中的副作用自动回滚。例如：

1. 文件已经成功 `rename`；
2. 客户端连接断开；
3. 等待响应的 Future 被取消。

磁盘改名不会因为 Future 消失而倒放。因此 Dufs 会在准备进入不可取消提交前，把任务登记为受跟踪的 detached commit。任务分离早于真正的 `CommitStarted` 和磁盘 rename；只要它能够比 HTTP 等待者活得更久，Router 就保守允许结果成为 `unknown`。

### `CancellationToken`

取消令牌让多个任务观察同一个关闭信号。它是协作式取消：代码必须在安全点检查令牌。不能在原子提交进行到一半时任意中断。

### `TaskTracker`

任务跟踪器记录已启动任务，让停机流程能够停止接收新任务并等待旧任务排空。仅仅 `spawn` 后忘记任务，会让优雅停机无法判断后台工作是否结束。

## 3.9 HTTP 请求由什么组成

一个 HTTP 请求包含：

```text
方法 + 路径和查询参数 + 头部 + 可选正文
```

例如：

```http
POST /__dufs__/api/rename HTTP/1.1
Host: files.example.com
Cookie: __Host-dufs-session=...
Content-Type: application/json
X-CSRF-Token: ...
X-Dufs-Operation-Id: 123e4567-e89b-42d3-a456-426614174000

{"source":"/old.txt","name":"new.txt","overwrite":false}
```

响应包含状态码、头部和可选正文。例如，假设 `/new.txt` 已存在且本次请求不允许覆盖，响应会是：

```http
HTTP/1.1 409 Conflict
Content-Type: application/problem+json
X-Dufs-Operation-Id: 123e4567-e89b-42d3-a456-426614174000
X-Dufs-Operation-State: failed

{"type":"urn:dufs:problem:destination_exists","title":"Conflict","status":409,"detail":"Destination already exists","code":"destination_exists","operation_id":"123e4567-e89b-42d3-a456-426614174000","state":"failed","http_status":409}
```

## 3.10 HTTP 方法在本项目中的含义

| 方法 | 本项目中的典型用途 | 是否应改变状态 |
| --- | --- | --- |
| `GET` | 页面、列表、下载、查询 job | 否 |
| `HEAD` | 下载元数据、查询上传检查点 | 否，不返回正文 |
| `POST` | 登录、注销、新建、移动、重命名、预检、discard | 视端点而定 |
| `PUT` | 从头建立上传会话并发送正文 | 是 |
| `PATCH` | 从检查点续传，或发布已完成 stage | 是 |
| `DELETE` | 删除目标 | 是 |

HTTP 方法只是协议意图，安全性仍取决于后端校验。例如 `HEAD /path` 在有上传会话头时查询的是上传状态，不应和普通文件 `HEAD` 混淆。

## 3.11 常见状态码不要只按“成功/失败”二分

| 状态 | 一般含义 | 在维护时应问 |
| ---: | --- | --- |
| `200` | 成功且有表示 | 表示结构和协议头是否也有效？ |
| `202` | 已接受或仍在运行 | 是否需要按 ID 查询？ |
| `204` | 成功但无正文 | 操作终态头是否匹配？ |
| `303` | 应用另一个 GET 地址 | 未认证的 HTML 导航应跳到登录页面吗？ |
| `400` | 输入或协议格式错误 | 哪个字段没有通过验证？ |
| `401` | 未认证或会话失效 | 页面是否应回到登录？ |
| `403` | 已识别但禁止 | CSRF、来源或路径策略失败？ |
| `404` | 目标或私有记录不可见 | 是否有意避免信息泄漏？ |
| `409` | 当前状态冲突 | 可确认覆盖、刷新还是放弃？ |
| `416` | Range 无法满足 | 是否重复或多段 Range？ |
| `429` | 并发或速率预算不足 | 是否有 `Retry-After`？ |
| `500/503` | 服务内部或暂时不可用 | 结果是否确定未提交？ |

状态码不能单独表达上传和写操作的全部语义，所以项目还使用类型化 JSON 问题和 `X-Dufs-*` 协议头。

## 3.12 Header、Cookie 和 Body

### Header

Header 是请求或响应的元数据。Dufs 会对 Operation ID、上传协议头、Cookie、Foundation CSRF/Origin/Host/Fetch Metadata 和受信代理头等安全相关字段拒绝重复、逗号拼接或非规范值，因为代理和服务端可能对歧义字段产生不同解释。普通展示型 header 是否允许重复仍需逐项确认；不能从某一路由的策略外推全部协议。

### Cookie

登录成功后，服务端设置 `__Host-dufs-session` Cookie。`__Host-` 前缀要求更严格的 Cookie 属性，当前会话还使用 `Secure`、`HttpOnly` 等限制。浏览器自动携带 Cookie，JavaScript 不需要也不应该读取会话秘密。

### CSRF token

Cookie 会被浏览器自动附带，因此恶意站点可能诱导用户浏览器发出写请求。CSRF token 是当前页面从服务端取得、再显式放入 `X-CSRF-Token` 的随机值；服务端以常量时间验证它，并与 Foundation 的严格同源检查共同防护。`Origin`、effective Host（含 URI authority）和 `Sec-Fetch-Site: same-origin` 都必须存在、唯一、规范且互相一致；缺失或歧义不会被兼容放行。

认证回答“你是谁”，CSRF token 与来源检查共同回答“这个写请求是否具备当前会话页面应持有的证明”；两者不可互相替代。

### Body

JSON browser API、Foundation 登录 JSON 和大文件正文有不同大小及时间限制。服务端必须在分配内存前限制正文，前端也对响应正文做有界读取，防止异常对端用巨大错误页耗尽内存。

## 3.13 URL 路径不等于磁盘路径

项目实际有两条不同的词法管线：

```text
HTTP URI path
  → 对合法转义做 percent decode（畸形 `%` 不一定仅因此被拒绝）
  → 要求解码结果是 UTF-8、拒绝 NUL/父目录组件/内部上传名
  → 普通路径会规范化重复 `/` 与 `.`
  → 内部 `__dufs__`/哈希资源必须使用唯一规范编码
  → RoutePath

浏览器 API JSON 中的绝对逻辑路径
  → 不做 URI percent decode，`%2F` 只是文件名文字
  → 严格拒绝空段、`.`、`..`、NUL、内部上传名和保留首段
  → RootedPath
```

随后文件系统操作才从已经打开的共享根 FD 相对解析，并按具体操作需要检查对象类型或 identity。列表项携带绑定当前对象完整身份的 revision；DELETE 通过 `If-Match` 回传，Move/Rename 在 JSON 中回传 `source_revision`，允许覆盖时还回传 `destination_revision`。后端把这些 token 绑定 owner、规范路径和身份，并在紧邻 rename 的提交检查中重新核对。最终 `statat → renameat2` 仍是两个相邻系统调用，不能把它宣传成对恶意外部 writer 的隔离。

直接写 `shared_root.join(user_path)` 并不足以抵抗符号链接逃逸或检查后替换。第 5 章会说明项目的路径类型和 `RootedFs`。

## 3.14 流式正文和背压

大文件不能一次读入内存。下载和上传都把正文分块处理：

```text
来源块 → 校验/计数 → 目标 → 下一块
```

如果接收方慢，发送方应等待，这叫**背压**。没有背压的实现可能把整个文件积压在内存中。

上传进度显示“浏览器已经发送的字节”，不等于服务器已经完成 `fsync` 和最终发布。因此正文发完后，页面还会显示独立的 `Submitting…` 阶段。

## 3.15 原子性、持久性和幂等性

这三个词经常一起出现，但含义不同：

- **原子性**：外界看到操作发生或没发生，不看到中间一半；同文件系统 `rename` 常用于原子发布。
- **持久性**：成功返回后，即使崩溃重启，变化仍应保存；这需要合适的文件和目录 `fsync` 顺序，并依赖存储正确实现同步。
- **幂等性**：重复同一逻辑请求不会产生额外副作用。Dufs 用 owner + Operation ID + 请求 fingerprint 对普通写操作提供受限重放。

原子 rename 不自动保证掉电持久，HTTP `PUT` 的名字也不自动让一段自定义上传协议具备业务幂等性。

## 3.16 浏览器端的 ES Modules 和类型

前端使用原生 ES modules：

```js
import { start } from "./modules/app.js";
start();
```

没有 React/Vue，也没有把源码编译成一个 bundle。JavaScript 文件通过 JSDoc 声明类型，再由 TypeScript `checkJs` 检查：

```sh
npm run check:types
```

静态类型只能检查开发者已经声明的关系。网络 JSON、DOM dataset、HTTP header 仍属于不可信运行时输入，必须先以 `unknown` 接收，再通过守卫验证。`as` 式断言或把值写成 `any` 只是让检查器闭嘴，并没有验证现实数据。

## 3.17 本章阅读练习

1. 在 [src/main.rs](../../src/main.rs) 中搜索 `Arc::new`、`Semaphore`、`TaskTracker` 和 `CancellationToken`，分别说明它们管理什么资源。
2. 在 [src/server/protocol.rs](../../src/server/protocol.rs) 中找类型化协议状态，观察它们在哪里才转成字符串。
3. 在 [clients/web/modules/http/client.js](../../clients/web/modules/http/client.js) 中搜索 `unknown` 或类型守卫，找出网络响应从“不可信值”变成可用对象的位置。
4. 思考：如果上传正文已经写完，但浏览器在等待提交响应时断网，为什么页面不能直接显示“上传失败”？

下一章会把这些概念放进真实的后端请求链。
