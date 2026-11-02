/** 稳态核心：worlds.steadycore 配置段。 */
import type { ConfigGroup } from 'cortico/core/types.ts';

export interface SteadyCoreConfigSection {
  enabled: boolean;
  /** 核心进程的地址，一般就是本机 */
  baseUrl: string;
  /** 一次请求最多等多久（毫秒） */
  timeoutMs: number;
  /** 后台多久拉一次 outbox（秒）。0 = 不拉，只在我自己调工具时才看 */
  pollSeconds: number;
}

export const STEADY_CORE_DEFAULTS: SteadyCoreConfigSection = {
  enabled: true,
  baseUrl: 'http://127.0.0.1:7801',
  timeoutMs: 8000,
  pollSeconds: 60,
};

export const STEADY_CORE_CONFIG_GROUP: ConfigGroup = {
  id: 'world:steadycore',
  owner: 'world:steadycore',
  schema: {
    type: 'object',
    title: '稳态核心 · steadycore',
    description:
      '把那个常驻进程的接口变成工具：看状态、推一步、问该不该叫、记一条、翻记忆。核心没起来时工具会直说连不上。',
    properties: {
      'worlds.steadycore.baseUrl': {
        type: 'string', title: '核心地址', 'x-hot': true,
        description: '默认 http://127.0.0.1:7801。改了端口就改这里。',
      },
      'worlds.steadycore.pollSeconds': {
        type: 'integer', title: '多久拉一次事件', minimum: 0, maximum: 3600,
        'x-suffix': '秒', 'x-hot': true,
        description: '0 = 我不去看它就不拉。它要叫我的时候，靠这个把话带过来。',
      },
      'worlds.steadycore.timeoutMs': {
        type: 'integer', title: '一次最多等', minimum: 500, maximum: 120000,
        'x-suffix': '毫秒', 'x-hot': false, description: '本机调一下就回来，一般不用改。',
      },
    },
  },
};
