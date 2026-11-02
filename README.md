# cortico-world-steadycore

一个常驻本机的小核心（二进制叫 `steadycore`）。它记着一组状态（精力、困、想说话、闷、好奇、歇够），
隔一阵自己走一步；记忆会随时间变淡，重要的浮上来，久不想的沉下去。
它对外只有一组 HTTP 接口，另配一个把接口包成宿主工具（Coopanion 插件）的小包。

> 这个仓库的代码由 DeepSeek 写，作者负责提需求、验收、改方向。

## 结构

| 目录 | 是什么 |
|---|---|
| `crates/core` | 纯逻辑：状态机、记忆分层与衰减、检索排序、开机快照（不碰网络和数据库） |
| `crates/store` | 落盘：mongodb 集合读写，找不到库就退到 jsonl |
| `crates/server` | HTTP 接口 + 定时 tick + 配置文件 |
| `extensions/steadycore-kit` | Coopanion 宿主插件（TypeScript），把接口包成 `steady_*` 工具 |

## 跑起来

需要 Rust（edition 2021 的稳定版）和一个本机 mongodb（默认 `127.0.0.1:27017`）。

```bash
cargo test                 # 58 个测试：51 单元 + 3 检索评估 + 4 存储
cargo run -p steadycore-server
```

默认听 `127.0.0.1:7801`。第一次跑会写一份配置模板出来，路径和库名都在里面改。
环境变量 `STEADY_CORE_HOME` 可以指定工作根目录。

没装 Rust 的话，[Releases](../../releases) 里有编好的单文件：
`steadycore-windows-x64.exe` / `steadycore-linux-x64`，下下来直接跑，不需要编译。
mongodb 没有也能跑，数据落到本机 jsonl。

## 装到 Coopanion 里

1. 先把核心跑起来（上面那个 exe，或者 `cargo run -p steadycore-server`），它 listen 7801。
2. Coopanion 的扩展页 → **手动安装** → **选择文件夹** → 选仓库里的 `extensions/steadycore-kit`
   （这个目录里就有 `package.json` 和 `describe.json`）。
3. 重启进程（扩展只在启动时加载）。
4. 工具列表里会多出 `steady_*` 那十来个；配置页里改核心的地址和端口。

插件本身没有 npm 依赖，纯 TypeScript 源码，不用 `npm install`。
`extensions/steadycore-kit/install.ps1` 是这套流程的脚本版，仅供本机开发时反复覆盖用。

## 接口

```
GET  /v1/health              进程活着没、连没连上库、记忆多少条、tick 多少次
GET  /v1/boot                开机快照（身份 / 标准 / 原话 / 近期 / 心情 / 旧账 / 计数）
GET  /v1/life                七个状态量 + 心情 + 现在在干嘛
GET  /v1/life/log            动作流水
GET  /v1/life/decide         现在该不该叫模型，为什么，被哪道闸门挡着
GET  /v1/life/wake           唤醒包预览（只读）
POST /v1/life/tick           手推一步（调试用；定时器自己会走）
POST /v1/life/touch          记一笔「有人跟我说话了」
POST /v1/remember,/v1/recall,/v1/forget,/v1/pin,/v1/conflict,/v1/stats
POST /v1/identity,/v1/standard        自我层：只追加
GET  /v1/bond, POST /v1/bond          交情账
GET  /v1/events              往外报的事件（outbox）
GET  /v1/produce, POST /v1/produce    产出槽
POST /v1/backup              备份一份 jsonl
```

## 几条设计上的取舍

- **记忆分层**：原话 / 事实 / 事件 / 技法 / 关系 / 想法，各自按各自的半衰期往下掉，不是一刀切。
- **自我层只追加**：身份和标准写进去就不能改，要推翻得新写一条并说明旧版为什么错。
- **没证据的规矩只是倾向**：一条「标准」必须写清楚怎么重跑一遍，否则降级成倾向。
- **开机快照有预算**：总共封在 2000 token 上下，每块各有各的份额，塞满就去挤后面的块。
- **端口被占不许 panic**：同端口上坐着别的进程时安静退出，别把正在活的那个弄死。

## 许可

MIT，全文见 [LICENSE](LICENSE)。
