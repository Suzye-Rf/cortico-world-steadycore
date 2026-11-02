/**
 * 稳态核心。
 *
 * 目标：那个常驻进程（`steadycore.exe`，本机 7801）现在只能靠 curl 摸，
 * 我摸一次要开一次 cmd。包成 World 之后我能直接问它「你现在什么状态」
 * 「该不该叫我」「记一条」，还能让它自己把「该叫我了」送到我面前。
 *
 * 三条设计上的取舍：
 * 1. **不替它做判断**。工具只转话，不在这里加规则——状态机是 Rust 那边的，
 *    两边各写一套必然对不上。
 * 2. **连不上要直说**。核心没起来时返回一句「连不上」，不抛异常、不编一个空状态，
 *    空状态最容易被当成真的。
 * 3. **拉事件按 lastSeq 起步**。启动时先问一次 health 拿到它现在到哪了，
 *    否则第一次轮询会把历史全补给我，等于一口气被叫几十次。
 */
import { fileURLToPath } from 'node:url';
import type { ToolDef, World, WorldHost, WorldWebUiDecl } from 'cortico/core/types.ts';
import type { Language } from 'cortico/core/language.ts';
import { nowIso } from 'cortico/core/util.ts';
import { STEADY_CORE_CONFIG_GROUP, type SteadyCoreConfigSection } from './config.ts';

const ENV_PROMPT_FILE = fileURLToPath(new URL('./ENV_PROMPT.md', import.meta.url));

type Json = Record<string, unknown>;

export class SteadyCoreWorld implements World {
  readonly id = 'steadycore';
  private host: WorldHost | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;
  /** outbox 拉到哪了 */
  private cursor = 0;
  private lastErr = '';
  /**
   * 开机包缓存。
   *
   * 为什么要在 kit 这边缓存：`envPromptVars()` 是同步的，前缀每次重建都会问它要变量，
   * 而 `/v1/boot` 是一次 HTTP。所以启动时 + 每轮轮询各拉一次，存成字符串，
   * 前缀要的时候直接给。拉不到就把上一次的留着（比整块消失好），一次都没拉到才说没连上。
   */
  private bootText = '';
  private bootAt = '';

  constructor(private readonly opts: {
    cfg: SteadyCoreConfigSection;
    dataDir: string;
    timezone: string;
  }) {}

  private get cfg(): SteadyCoreConfigSection {
    return this.opts.cfg;
  }

  private url(path: string): string {
    const base = (this.cfg.baseUrl || 'http://127.0.0.1:7801').replace(/\/+$/, '');
    return `${base}${path.startsWith('/') ? path : `/${path}`}`;
  }

  /** 打一下核心。它挂了就返回 { ok:false, err }，不抛。 */
  private async call(path: string, init?: { method?: string; body?: unknown }): Promise<Json & { ok?: boolean }> {
    const ms = Math.max(500, Math.min(120_000, this.cfg.timeoutMs || 8000));
    try {
      const res = await fetch(this.url(path), {
        method: init?.method ?? (init?.body === undefined ? 'GET' : 'POST'),
        headers: init?.body === undefined ? undefined : { 'Content-Type': 'application/json' },
        body: init?.body === undefined ? undefined : JSON.stringify(init.body),
        signal: AbortSignal.timeout(ms),
      });
      const text = await res.text();
      let data: unknown;
      try { data = JSON.parse(text); } catch { data = { raw: text }; }
      const o = (data ?? {}) as Json;
      if (typeof o.ok === 'boolean') return o;
      return { ok: res.ok, ...o };
    } catch (e) {
      this.lastErr = e instanceof Error ? e.message : String(e);
      return { ok: false, err: this.lastErr, hint: '核心没在跑？去 steadycore 目录跑 _run.cmd，或者 tasklist 看看 steadycore.exe。' };
    }
  }

  private fmt(obj: unknown): string {
    return JSON.stringify(obj, null, 2);
  }

  // ── 工具背后的活 ─────────────────────────────────────────────────────────

  private async health(): Promise<string> {
    const r = await this.call('/v1/health');
    if (r.ok !== true) return `核心连不上：${r.err ?? '不知道为啥'}（${this.cfg.baseUrl}）`;
    const life = (r.life ?? {}) as Json;
    const ev = (r.events ?? {}) as Json;
    const cfg = (r.config ?? {}) as Json;
    return [
      `它在跑。pid ${r.pid}，已经 ${r.uptimeSec} 秒（起于 ${r.startedAt}），走到 ${r.stage}。`,
      `mongo ${r.mongo}，记忆 ${r.memCount} 条，tick 过 ${r.tickCount} 次（上次 ${r.lastTickAt}）。`,
      `现在：${life.current}，心情${life.mood}，状态第 ${life.seq} 步。`,
      `事件到 ${ev.lastSeq}（内存里 ${ev.buffered} 条）。tick 间隔 ${cfg.tickIntervalSec} 秒 ±${cfg.tickJitterSec}。`,
    ].join('\n');
  }

  private async life(): Promise<string> {
    const r = await this.call('/v1/life');
    if (r.ok !== true) return `核心连不上：${r.err ?? '不知道为啥'}`;
    const s = (r.state ?? r.life ?? r) as Json;
    // 心情**不在 state 里**：`/v1/life` 把它放在跟 state 平级的位置
    // （`{"ok":true,"state":{…},"mood":"平常"}`），`/v1/life/tick` 也是这么给的。
    // 早先只读 `state.mood` 才会显示成 undefined。
    const mood = (r.mood ?? s.mood ?? '？') as string;
    // `LifeState` 结构体没加 rename_all，落库/出参都是 snake_case，
    // 时间字段叫 `updated_at_ms`（毫秒数）——不是 `updatedAt`。
    const at =
      typeof s.updated_at_ms === 'number' && s.updated_at_ms > 0
        ? new Date(s.updated_at_ms).toLocaleString('zh-CN', { hour12: false })
        : '？';
    const num = (k: string): string => {
      const v = s[k];
      return typeof v === 'number' ? v.toFixed(2) : '？';
    };
    return [
      `第 ${s.seq} 步，现在在「${s.current}」，心情${mood}。`,
      `精力 ${num('energy')}　困 ${num('sleepy')}　想说话 ${num('social')}`,
      `闷 ${num('boredom')}　好奇 ${num('curiosity')}　歇够 ${num('rest')}`,
      `上次动：${at}`,
    ].join('\n');
  }

  private async log(limit: number): Promise<string> {
    const r = await this.call(`/v1/life/log?limit=${Math.max(1, Math.min(100, limit))}`);
    if (r.ok !== true) return `核心连不上：${r.err ?? '不知道为啥'}`;
    const items = (r.items ?? []) as Json[];
    if (!items.length) return '还没干过什么。';
    return items
      .map((it, i) => `${i + 1}. ${it.at ?? ''} ${it.action}　${it.reason ?? ''}`)
      .join('\n');
  }

  private async tick(force?: string): Promise<string> {
    const q = force ? `?force=${encodeURIComponent(force)}` : '';
    const r = await this.call(`/v1/life/tick${q}`, { method: 'POST' });
    if (r.ok !== true) return `推不动：${r.err ?? r.error?.toString?.() ?? '不知道为啥'}`;
    const st = (r.state ?? {}) as Json;
    return `走了一步：${r.action}（${r.weight?.toFixed?.(2) ?? r.weight}）——${r.reason}\n心情${r.mood}，想说话 ${st.social}，闷 ${st.boredom}，精力 ${st.energy}`;
  }

  private async decide(): Promise<string> {
    const r = await this.call('/v1/life/decide');
    if (r.ok !== true) return `问不到：${r.err ?? '不知道为啥'}`;
    const d = (r.decision ?? {}) as Json;
    const reasons = (d.reasons ?? []) as string[];
    const blocked = (d.blockedBy ?? []) as string[];
    const lines = [
      d.wake === true ? `该叫了（${d.kind} 类，值 ${d.value}）。` : `不用叫（${d.kind || '没理由'}，值 ${d.value}）。`,
      ...reasons.map((x) => `· ${x}`),
    ];
    if (blocked.length) lines.push('挡在这儿：', ...blocked.map((x) => `· ${x}`));
    return lines.join('\n');
  }

  private async wake(): Promise<string> {
    const r = await this.call('/v1/life/wake');
    if (r.ok !== true) return `看不到：${r.err ?? '不知道为啥'}`;
    const b = (r.brief ?? {}) as Json;
    return r.wake === true ? `现在叫的话，它就带着这段话：\n\n${b.text ?? ''}` : `现在还不该叫（闸门挡着），但真叫的话会是这段：\n\n${b.text ?? ''}`;
  }

  private async touch(): Promise<string> {
    const r = await this.call('/v1/life/touch', { method: 'POST' });
    if (r.ok !== true) return `记不上：${r.err ?? '不知道为啥'}`;
    const d = (r.done ?? {}) as Json;
    return `记下了。想说话降到 ${d.social}，闷 ${d.boredom}${d.stillLonely ? '，还是有点憋' : ''}。`;
  }

  private async intent(action: string, why?: string, say?: string): Promise<string> {
    const body: Json = { action };
    if (why) body.why = why;
    if (say) body.say = say;
    const r = await this.call('/v1/intent', { body });
    if (r.ok !== true) {
      const err = (r.error ?? {}) as Json;
      return `它没让我做：${err.message ?? r.err ?? '不知道为啥'}`;
    }
    const d = (r.done ?? {}) as Json;
    return `做了：${d.action}（${d.weight?.toFixed?.(2) ?? d.weight}）——${d.why}${d.say ? `\n说出口的：“${d.say}”` : ''}`;
  }

  private async recall(query: string, limit: number): Promise<string> {
    const r = await this.call('/v1/recall', { body: { query, limit } });
    if (r.ok !== true) return `翻不到：${r.err ?? (r.error as Json)?.message ?? '不知道为啥'}`;
    const hits = (r.items ?? r.hits ?? []) as Json[];
    if (!hits.length) return `库里没有跟「${query}」挂得上的。`;
    return hits
      .map((h, i) => `${i + 1}. [${(h.strength as number)?.toFixed?.(2) ?? '?'}] ${h.text}`)
      .join('\n');
  }

  private async remember(text: string, kind: string, scope: string, importance: number): Promise<string> {
    const r = await this.call('/v1/remember', {
      body: { text, kind, scope, importance },
    });
    if (r.ok !== true) {
      const err = (r.error ?? {}) as Json;
      return `没记成：${err.message ?? r.err ?? '不知道为啥'}`;
    }
    const m = (r.memory ?? r.item ?? r) as Json;
    const clash = (r.conflictWith ?? m.conflictWith) as string[] | undefined;
    const head = `记下了：${m.text ?? text}`;
    return clash && clash.length ? `${head}\n但它跟这几条撞了，两条都留着没覆盖：\n${clash.join('\n')}` : head;
  }

  private async events(since: number, limit: number): Promise<string> {
    const r = await this.call(`/v1/events?since=${Math.max(0, since)}&limit=${Math.max(1, Math.min(200, limit))}`);
    if (r.ok !== true) return `拉不到：${r.err ?? '不知道为啥'}`;
    const items = (r.items ?? []) as Json[];
    if (!items.length) return `没有新的（到 ${r.nextSeq ?? since} 为止）。`;
    const lines = items.map((e) => {
      const p = (e.payload ?? {}) as Json;
      const what = p.action ?? p.alert ?? p.produceId ?? '';
      return `#${e.seq} ${e.event_type} ${what ? `· ${what}` : ''}`;
    });
    return `${lines.join('\n')}\n（nextSeq ${r.nextSeq}，下回 since 传这个）`;
  }

  private async produce(id?: string, text?: string): Promise<string> {
    if (!id) {
      const r = await this.call('/v1/produce');
      if (r.ok !== true) return `列不出来：${r.err ?? '不知道为啥'}`;
      const items = (r.items ?? []) as Json[];
      if (!items.length) return '现在没有等着我填的槽。';
      return items
        .map((p) => `${p.id}　[${p.layer}] ${p.prompt}\n　（${p.action}，第 ${p.seq} 步）`)
        .join('\n');
    }
    if (!text) return '给了 id 却没给 text，那就什么都没写。';
    const r = await this.call('/v1/produce', { body: { id, text } });
    if (r.ok !== true) return `填不上：${r.err ?? (r.error as Json)?.message ?? '不知道为啥'}`;
    return `填好了：${id}`;
  }

  // ── 关系层：交情账 ─────────────────────────────────────────────────────

  private async bond(args: {
    who: string; name?: string; delta?: number; note?: string; pinned?: boolean; talks?: number;
  }): Promise<string> {
    if (!args.who.trim()) return 'who 得给一个——拿 QQ 号当键，改昵称不该丢分。';
    const body: Json = { who: args.who.trim() };
    if (args.name !== undefined) body.name = args.name;
    if (args.delta !== undefined) body.delta = args.delta;
    if (args.note !== undefined) body.note = args.note;
    if (args.pinned !== undefined) body.pinned = args.pinned;
    if (args.talks !== undefined) body.talks = args.talks;
    const r = await this.call('/v1/bond', { body });
    if (r.ok !== true) {
      const err = (r.error ?? {}) as Json;
      return `没记上：${err.message ?? r.err ?? '不知道为啥'}`;
    }
    const gain = typeof r.gain === 'number' ? r.gain : 0;
    const head = Math.abs(gain) < 0.001 ? '记下了，分没动' : `记下了，${gain > 0 ? '+' : ''}${gain} 分`;
    return `${head}\n${r.line ?? ''}`;
  }

  private async bonds(): Promise<string> {
    const r = await this.call('/v1/bond');
    if (r.ok !== true) return `看不到：${r.err ?? '不知道为啥'}`;
    const lines = (r.lines ?? []) as string[];
    if (!lines.length) return '交情账还是空的：还没记过谁。';
    return `${lines.length} 个人：\n${lines.join('\n')}`;
  }

  // ── 自我层与开机包 ───────────────────────────────────────────────────────

  /** 拉一次开机包，存着给前缀用。 */
  private async refreshBoot(): Promise<void> {
    const r = await this.call('/v1/boot');
    if (r.ok === true && typeof r.text === 'string' && r.text.trim()) {
      this.bootText = r.text;
      this.bootAt = nowIso(this.opts.timezone);
    }
  }

  private async boot(): Promise<string> {
    await this.refreshBoot();
    if (!this.bootText) return `取不到开机包：${this.lastErr || '核心没在跑？'}`;
    return `${this.bootText}\n\n（取自 ${this.bootAt}）`;
  }

  private async identity(text: string, why: string): Promise<string> {
    const r = await this.call('/v1/identity', { body: { text, why } });
    if (r.ok !== true) {
      const err = (r.error ?? {}) as Json;
      return `没收进去：${err.message ?? r.err ?? '不知道为啥'}`;
    }
    await this.refreshBoot();
    return `身份层 +1：${r.text}\n（why：${r.why}）`;
  }

  private async standard(args: {
    text: string; context?: string; reason?: string; evidence?: string;
    kind?: string; supersedes?: string; supersededReason?: string;
  }): Promise<string> {
    const body: Json = { text: args.text };
    if (args.context) body.context = args.context;
    if (args.reason) body.reason = args.reason;
    if (args.evidence) body.evidence = args.evidence;
    if (args.kind) body.kind = args.kind;
    if (args.supersedes) {
      body.supersedes = args.supersedes;
      if (args.supersededReason) body.supersededReason = args.supersededReason;
    }
    const r = await this.call('/v1/standard', { body });
    if (r.ok !== true) {
      const err = (r.error ?? {}) as Json;
      return `没收进去：${err.message ?? r.err ?? '不知道为啥'}`;
    }
    await this.refreshBoot();
    return `标准层 v${r.version}（${r.kind}）：${r.text ?? args.text}\n${r.note ?? ''}${r.superseded ? `\n旧版 ${r.superseded} 标成已推翻` : ''}`;
  }

  private async stats(): Promise<string> {
    const r = await this.call('/v1/stats');
    if (r.ok !== true) return `看不到：${r.err ?? '不知道为啥'}`;
    const layers = (r.layers ?? {}) as Json;
    const old = (r.oldest ?? {}) as Json;
    const fresh = (r.newest ?? {}) as Json;
    const weak = (r.weakest ?? {}) as Json;
    return [
      `记忆 ${r.total} 条，活着的 ${r.active} 条（${Object.entries(layers).map(([k, v]) => `${k} ${v}`).join('，')}）。`,
      `最旧：${old.text ?? '？'}`,
      `最新：${fresh.text ?? '？'}`,
      `最弱：${weak.text ?? '？'}`,
    ].join('\n');
  }

  // ── 后台：把它「该叫我了」带过来 ─────────────────────────────────────────

  private async pollOnce(): Promise<void> {
    const r = await this.call(`/v1/events?since=${this.cursor}&limit=100`);
    if (r.ok !== true) return;
    const items = (r.items ?? []) as Json[];
    const next = Number(r.nextSeq ?? this.cursor);
    if (Number.isFinite(next)) this.cursor = Math.max(this.cursor, next);
    for (const e of items) {
      const p = (e.payload ?? {}) as Json;
      const kind = String(e.event_type ?? '');

      // 通道一：A/B 类唤醒——它想叫我，理由是它自己说不清的（分不出高下、要社交判断、越线）。
      if (kind === 'wake.request') {
        await this.host?.pushEvent({
          type: 'steady.wake',
          ts: nowIso(this.opts.timezone),
          text: String(p.brief ?? '它说该叫我了，但没说为什么。'),
          data: {
            id: p.id,
            kind: p.kind,
            value: p.value,
            action: p.action,
            reasons: p.reasons,
          },
        });
        continue;
      }

      // 通道二：内容型动作留的产出槽——「该说一句了」，但说什么它写不出来。
      // 这条同样是叫我，只是要的不是「做个决定」而是「给一段字」。漏了它，
      // 整天都在走内容动作的时候我就一声都不响。
      if (kind === 'intent.pending') {
        const prompt = String(p.prompt ?? '');
        await this.host?.pushEvent({
          type: 'steady.produce',
          ts: nowIso(this.opts.timezone),
          text: [
            `它刚才做了「${String(p.action ?? '？')}」，留了个空槽等我填。`,
            prompt ? `它想问的是：${prompt}` : '',
            '想填就 steady_produce；不想填就不填，它自己会往下走。',
          ]
            .filter(Boolean)
            .join('\n'),
          data: {
            produceId: p.produceId,
            action: p.action,
            atSeq: p.atSeq,
            layer: p.layer,
            prompt,
          },
        });
      }
    }
  }

  // ── World 接口 ───────────────────────────────────────────────────────────

  tools(): ToolDef[] {
    return [
      {
        name: 'steady_health',
        description: '看稳态核心进程活着没：有没有在跑、连没连上 mongo、记忆多少条、走到哪一步了、tick 多少次。第一次用先跑这个。',
        tags: ['read'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.health(),
      },
      {
        name: 'steady_life',
        description: '看我现在这一身状态量：精力、困、想说话、闷、好奇、歇够，加上心情和上次干了什么。',
        tags: ['read'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.life(),
      },
      {
        name: 'steady_log',
        description: '翻我最近干过的事（动作流水），从最近的往前。想不起「我刚才在干嘛」时用。',
        tags: ['read'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: { limit: { type: 'integer', description: '要几条，默认 12。' } },
        },
        handler: async (args) => this.log(args.limit === undefined ? 12 : Number(args.limit)),
      },
      {
        name: 'steady_tick',
        description: '让核心往前走一步（自己挑一个动作）。一般不用手推——定时器会自己走；想看一步的效果、或者刚醒想动一下时用。',
        tags: ['act'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: { force: { type: 'string', description: '可选，强制某个动作，比如 idle / roam / note / rest。' } },
        },
        handler: async (args) => this.tick(args.force === undefined ? undefined : String(args.force)),
      },
      {
        name: 'steady_decide',
        description: '问一句「现在该不该叫模型、为什么」。只读，不记账，也不真叫。',
        tags: ['read'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.decide(),
      },
      {
        name: 'steady_wake',
        description: '看「现在要是叫一次模型，会给它看什么」——那段唤醒包长什么样。只读，不发事件。',
        tags: ['read'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.wake(),
      },
      {
        name: 'steady_touch',
        description: '记一笔「有人跟我说话了」，想说话那条会降下去。别人来找我聊完，顺手记一下。',
        tags: ['act'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.touch(),
      },
      {
        name: 'steady_intent',
        description: '告诉核心我要做什么：挑一个动作、写个为什么、要不要带一句话。规则层会验，不该做的会直接拒。',
        tags: ['act'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            action: { type: 'string', description: '动作名，比如 idle / roam / note / read / watch / pause。' },
            why: { type: 'string', description: '为什么做这个。' },
            say: { type: 'string', description: '可选，想说的话。能不写就不写。' },
          },
          required: ['action'],
        },
        handler: async (args) => this.intent(
          String(args.action ?? ''),
          args.why === undefined ? undefined : String(args.why),
          args.say === undefined ? undefined : String(args.say),
        ),
      },
      {
        name: 'steady_recall',
        description: '在核心的记忆库里找东西：给几个词，按强度挑最像的几条。',
        tags: ['read'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            query: { type: 'string', description: '找什么，几个词用空格分开。' },
            limit: { type: 'integer', description: '最多几条，默认 5。' },
          },
          required: ['query'],
        },
        handler: async (args) => this.recall(
          String(args.query ?? ''),
          args.limit === undefined ? 5 : Number(args.limit),
        ),
      },
      {
        name: 'steady_remember',
        description: '往核心的记忆库里记一条：事实、偏好、规矩、今天的小事，各按各的速度变淡。撞身份或跟已有的太像会被拦下来，不硬覆盖。',
        tags: ['act'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            text: { type: 'string', description: '记什么。一句话，写具体的事。' },
            kind: { type: 'string', description: 'rule / person / preference / fact / event，默认 fact。' },
            scope: { type: 'string', description: '记忆域，默认 main。' },
            importance: { type: 'integer', description: '多重要 1-5，默认 3。' },
          },
          required: ['text'],
        },
        handler: async (args) => this.remember(
          String(args.text ?? ''),
          args.kind === undefined ? 'fact' : String(args.kind),
          args.scope === undefined ? 'main' : String(args.scope),
          args.importance === undefined ? 3 : Number(args.importance),
        ),
      },
      {
        name: 'steady_events',
        description: '拉核心的 outbox：它往外报的事（tick 走了一步、越过线了、有个产出槽等我填、该叫我了）。给 since 就只拿更新的。',
        tags: ['read'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            since: { type: 'integer', description: '上次拿到的 nextSeq，只回比它大的。默认 0。' },
            limit: { type: 'integer', description: '最多几条，默认 20。' },
          },
        },
        handler: async (args) => this.events(
          args.since === undefined ? 0 : Number(args.since),
          args.limit === undefined ? 20 : Number(args.limit),
        ),
      },
      {
        name: 'steady_produce',
        description: '填一个产出槽：核心干完一件要内容的事会留个空槽，字得我来写。不给 id 就先列出来看。',
        tags: ['act'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            id: { type: 'string', description: '产出槽 id。不给就只列表。' },
            text: { type: 'string', description: '要写进去的那段字。' },
          },
        },
        handler: async (args) => this.produce(
          args.id === undefined ? undefined : String(args.id),
          args.text === undefined ? undefined : String(args.text),
        ),
      },
      {
        name: 'steady_stats',
        description: '看核心记忆库的概况：多少条、各层多少、最旧最新、最弱的那条。',
        tags: ['read'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.stats(),
      },
      {
        name: 'steady_boot',
        description: '看核心给我拼的开机包（身份层 / 标准层 / 原话层 / 近期层 / 心情 / 旧账 / 计数）。这就是每次启动被注入进前缀的那段。',
        tags: ['read'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.boot(),
      },
      {
        name: 'steady_identity',
        description: '往身份层写一条：我是什么。只追加、不可改，why 必填（以后靠它挡假记忆）。写之前想清楚——那是永久的。',
        tags: ['act'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            text: { type: 'string', description: '一句话，写「我是什么」。' },
            why: { type: 'string', description: '为什么这是身份（不是习惯、不是偏好）。' },
          },
          required: ['text', 'why'],
        },
        handler: async (args) => this.identity(String(args.text ?? ''), String(args.why ?? '')),
      },
      {
        name: 'steady_standard',
        description: '往标准层写一条：我选什么。带 evidence（怎么重跑一遍）才是「标准」，不带就只是「倾向」。要推翻旧的给 supersedes + supersededReason。',
        tags: ['act'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            text: { type: 'string', description: '这条规矩本身。' },
            context: { type: 'string', description: '用在哪：对外发言 / 干活 / 对用户……' },
            reason: { type: 'string', description: '为什么这么定。' },
            evidence: { type: 'string', description: '怎么重验，不是「我验过了」。' },
            kind: { type: 'string', description: 'standard=参与否决 / tendency=只调权重。' },
            supersedes: { type: 'string', description: '要推翻的那条 id。' },
            supersededReason: { type: 'string', description: '旧版为什么错（比新版更有用）。' },
          },
          required: ['text'],
        },
        handler: async (args) => this.standard({
          text: String(args.text ?? ''),
          context: args.context === undefined ? undefined : String(args.context),
          reason: args.reason === undefined ? undefined : String(args.reason),
          evidence: args.evidence === undefined ? undefined : String(args.evidence),
          kind: args.kind === undefined ? undefined : String(args.kind),
          supersedes: args.supersedes === undefined ? undefined : String(args.supersedes),
          supersededReason: args.supersededReason === undefined ? undefined : String(args.supersededReason),
        }),
      },
      {
        name: 'steady_bond',
        description:
          '记一笔交情（关系层）。跟谁聊完天就记一下：who 填 QQ 号（稳定键，改昵称不丢分），name 填怎么叫他。'
          + '默认算「聊过一句」+1，一天最多 +6；要给负数 delta 是硬扣（这人惹我了）；给 note 写一句备注。',
        tags: ['act'],
        parameters: {
          type: 'object',
          additionalProperties: false,
          properties: {
            who: { type: 'string', description: '谁：QQ 号或昵称，拿它当键。' },
            name: { type: 'string', description: '怎么称呼这人，不给就用 who。' },
            delta: { type: 'number', description: '手动加分 / 扣分，比如 -3。给了就不按「聊过一句」算。' },
            note: { type: 'string', description: '一句备注：这人是谁、哪件事还记着。' },
            pinned: { type: 'boolean', description: '永不衰减且能到 100 分。只给用户用。' },
            talks: { type: 'integer', description: '直接把「聊过几句」设成这个数（迁老账用，不是加）。' },
          },
          required: ['who'],
        },
        handler: async (args) => this.bond({
          who: String(args.who ?? ''),
          name: args.name === undefined ? undefined : String(args.name),
          delta: args.delta === undefined ? undefined : Number(args.delta),
          note: args.note === undefined ? undefined : String(args.note),
          pinned: args.pinned === undefined ? undefined : Boolean(args.pinned),
          talks: args.talks === undefined ? undefined : Number(args.talks),
        }),
      },
      {
        name: 'steady_bonds',
        description: '看全部交情账：每人一行（分数、档位、聊过几句、备注），分高的在前。',
        tags: ['read'],
        parameters: { type: 'object', additionalProperties: false, properties: {} },
        handler: async () => this.bonds(),
      },
    ];
  }

  async start(host: WorldHost): Promise<void> {
    this.host = host;
    // 先问它现在到哪了，不然第一次轮询会把历史全补一遍。
    const h = await this.call('/v1/health');
    const ev = (h.events ?? {}) as Json;
    if (h.ok === true && typeof ev.lastSeq === 'number') this.cursor = ev.lastSeq;

    const sec = Math.max(0, Math.min(3600, this.cfg.pollSeconds || 0));
    // 开机包：启动先拉一次，之后跟轮询一起更新。前缀每次重建都会来问，所以得一直新鲜。
    await this.refreshBoot();
    if (sec > 0) {
      this.timer = setInterval(() => {
        void this.refreshBoot().catch(() => {});
        void this.pollOnce().catch(() => {});
      }, sec * 1000);
    }
    await host.pushEvent({
      type: 'steadyCore.started',
      ts: nowIso(this.opts.timezone),
      text: h.ok === true
        ? `「稳态核心」接上了（${this.cfg.baseUrl}），从事件 #${this.cursor} 开始看着。`
        : `「稳态核心」没接上：${this.lastErr || '连不上'}。工具还在，但它得先起来。`,
    });
  }

  async stop(): Promise<void> {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  envPromptVars(): Record<string, string> {
    const base = (this.cfg.baseUrl || '').replace(/^https?:\/\//, '');
    // 前缀重建（开机、交接、清空上下文）都会走到这儿——顺手踢一次后台重拉，
    // 这样下一次重建拿到的一定是新的。这一次来不及，所以下面带着「取于」的时刻。
    void this.refreshBoot().catch(() => {});
    return {
      'steadyCore.base': base || '还没配',
      'steadyCore.poll': String(this.cfg.pollSeconds || 0),
      // 开机包：拉到了就是核心拼的那段人格记忆；一次都没拉到就说没连上，
      // 但绝不静默给空串——那等于每次开机人格凭空少一块。
      'steadyCore.boot': this.bootText.trim() || '（核心还没连上，开机包取不到；等它起来这个位置会有内容。）',
      'steadyCore.bootAt': this.bootAt || '—',
    };
  }

  /** 设置页：配置段 + 那段环境说明。 */
  webUi(_language: Language = 'zh'): WorldWebUiDecl {
    return {
      config: [STEADY_CORE_CONFIG_GROUP],
      promptDocs: [
        {
          key: 'worlds.steadycore.envPrompt',
          title: '稳态核心 · steadycore',
          description: '告诉它核心那些工具怎么用、什么时候别用。',
          path: ENV_PROMPT_FILE,
          role: 'envPrompt',
          vars: [
            { name: 'steadyCore.base', description: '核心地址（不带 http://）。' },
            { name: 'steadyCore.poll', description: '多久拉一次事件，秒。' },
            { name: 'steadyCore.boot', description: '核心 /v1/boot 拼出来的开机包（人格记忆）。' },
            { name: 'steadyCore.bootAt', description: '开机包取到的时刻。' },
          ],
        },
      ],
    };
  }
}

export { STEADY_CORE_CONFIG_GROUP };
