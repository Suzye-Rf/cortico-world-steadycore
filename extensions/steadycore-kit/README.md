# 稳态核心 · steadycore kit

把 `steadycore.exe`（本机 7801 上那个常驻进程）的 HTTP 接口包成我能直接调的工具。

## 装了之后多出来的工具

| 工具 | 干什么 |
|---|---|
| `steady_health` | 进程活没活、mongo 连没连、走到哪一步 |
| `steady_life` | 七个状态量 + 心情 + 现在在干嘛 |
| `steady_log` | 最近干过的事（动作流水） |
| `steady_tick` | 手推一步（调试用，定时器自己会走） |
| `steady_decide` | 现在该不该叫我，为什么，被哪道闸门挡着 |
| `steady_wake` | 唤醒包长什么样（只读，不发事件） |
| `steady_touch` | 记一笔「有人跟我说话了」 |
| `steady_intent` | 告诉核心我要做什么（会被规则验） |
| `steady_recall` / `steady_remember` / `steady_stats` | 核心那边的记忆库 |
| `steady_events` | 拉它往外报的事件 |
| `steady_produce` | 填它留出来的产出槽 |

## 事件

- `steadyCore.started` —— 挂载时；连不上会直说。
- `steady.wake` —— 它说该叫一次模型了，正文就是它写好的唤醒包。

## 装

```
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

脚本从自己所在的目录取源码，复制到 `data/extensions/node_modules/cortico-world-steadycore`，
并在 `data/extensions/package.json` 里加一条 `file:` 依赖。**装完要重启应用**才会出现这些工具。

## 不做什么

- 不在这一侧重写状态机。规则只有 Rust 那边一份，这边只转话。
- 连不上就返回一句「连不上」，不编一个空状态——空状态最容易被当成真的。
- 不吃掉「该叫了」的历史：启动时先问 health 拿 lastSeq，从那儿往后看。
