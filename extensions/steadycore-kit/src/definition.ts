/** 稳态核心 World 的定义。 */
import { readFileSync } from 'node:fs';
import { describeTools, type WorldDefinition } from 'cortico/world.ts';
import type { ToolDef } from 'cortico/core/types.ts';
import { STEADY_CORE_DEFAULTS, STEADY_CORE_CONFIG_GROUP, type SteadyCoreConfigSection } from './config.ts';
import { SteadyCoreWorld } from './world.ts';

/** describe.json 是工具清单的唯一出处。 */
interface DescribeFile {
  tools?: ReadonlyArray<Omit<ToolDef, 'handler'>>;
}

const DESCRIBE = JSON.parse(
  readFileSync(new URL('../describe.json', import.meta.url), 'utf8'),
) as DescribeFile;

export const STEADY_CORE: WorldDefinition<SteadyCoreConfigSection> = {
  // 页面 id 是 `world:<id>`，而框架的 PAGE_ID_RE 只收小写字母/数字/连字符
  // （`/^(world|persona|llm|memory):[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/`）。
  // 原来写 `steadyCore` 带大写 C → 整页被丢掉 → 侧栏没有这一项、点进去「页面不存在」。
  id: 'steadycore',
  label: '稳态核心·steadycore',
  defaults: () => ({ ...STEADY_CORE_DEFAULTS }),
  create: (ctx) => new SteadyCoreWorld({
    cfg: ctx.cfg,
    dataDir: ctx.dataDir,
    timezone: ctx.timezone,
  }),
  describe: {
    tools: describeTools(DESCRIBE.tools ?? [], {}),
    events: [
      {
        type: 'steadyCore.started',
        description: { zh: '「稳态核心」挂载后开始看着它的事件。', en: 'The steadycore World started watching its outbox.' },
      },
      {
        type: 'steady.wake',
        description: { zh: '核心说该叫一次模型了，带着它写好的那段唤醒包。', en: 'The core asks for a model call, carrying its wake brief.' },
      },
    ],
  },
  envPromptVars: () => ({}),
};

export { STEADY_CORE_CONFIG_GROUP };
