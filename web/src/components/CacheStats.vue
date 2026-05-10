<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue';
import { api, type CacheStats } from '../api';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';

const stats = ref<CacheStats | null>(null);
const loading = ref(false);
const lastUpdated = ref<Date | null>(null);
let timer: ReturnType<typeof setInterval> | null = null;

async function load() {
  loading.value = true;
  try {
    stats.value = await api.getCacheStats();
    lastUpdated.value = new Date();
  } catch {
    // 忽略瞬态错误
  } finally {
    loading.value = false;
  }
}

onMounted(() => {
  load();
  // 每 30 秒自动刷新
  timer = setInterval(load, 30_000);
});

onUnmounted(() => {
  if (timer) clearInterval(timer);
});

/** 格式化大数字（千分位 / k / M / b） */
function fmt(n: number): string {
  if (n < 1000) return n.toString();
  if (n < 1_000_000) return (n / 1000).toFixed(1) + 'k';
  if (n < 1_000_000_000) return (n / 1_000_000).toFixed(2) + 'M';
  return (n / 1_000_000_000).toFixed(2) + 'b';
}

function pct(n: number): string {
  return n.toFixed(2) + '%';
}

/** 命中率配色：>=90% 绿；70-90% 黄；<70% 红 */
const hitRateClass = computed(() => {
  const r = stats.value?.hit_rate_pct ?? 0;
  if (r >= 90) return 'text-emerald-600';
  if (r >= 70) return 'text-amber-600';
  return 'text-red-500';
});

const oneHourShareClass = computed(() => {
  const r = stats.value?.one_hour_share_pct ?? 0;
  if (r >= 95) return 'text-emerald-600';
  if (r >= 50) return 'text-amber-600';
  return 'text-[#8c8475]';
});

const stickyHealthClass = computed(() => {
  const s = stats.value;
  if (!s) return 'text-[#8c8475]';
  const total = s.sticky_preserved_total + s.sticky_evicted_total;
  if (total === 0) return 'text-[#8c8475]';
  const ratio = s.sticky_preserved_total / total;
  if (ratio >= 0.7) return 'text-emerald-600';
  return 'text-amber-600';
});

/** 单账号命中率配色 */
function accountHitClass(rate: number): string {
  if (rate >= 90) return 'text-emerald-600';
  if (rate >= 70) return 'text-amber-600';
  if (rate > 0) return 'text-red-500';
  return 'text-[#b5b0a6]';
}

/** 单账号 1h 注入率配色（writer 数为 0 时返回灰色） */
function accountOneHourClass(rate: number, hasWrites: number): string {
  if (hasWrites === 0) return 'text-[#b5b0a6]';
  if (rate >= 95) return 'text-emerald-600';
  if (rate >= 50) return 'text-amber-600';
  return 'text-red-500';
}

function lastUpdatedText(): string {
  if (!lastUpdated.value) return '从未刷新';
  return lastUpdated.value.toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}
</script>

<template>
  <div class="max-w-7xl mx-auto px-6 py-8 space-y-6">
    <!-- 顶部说明 -->
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-semibold text-[#29261e] tracking-tight">缓存命中率统计</h2>
        <p class="text-sm text-[#8c8475] mt-1">
          数据来自 cc-bridge 启动至今的内存累计（重启清零）。每 30 秒自动刷新，最近更新：{{ lastUpdatedText() }}
        </p>
      </div>
      <Button @click="load" :disabled="loading" variant="outline" size="sm">
        {{ loading ? '刷新中...' : '手动刷新' }}
      </Button>
    </div>

    <div v-if="!stats" class="text-center py-20 text-[#8c8475]">加载中...</div>

    <template v-else>
      <!-- 4 个核心卡片 -->
      <div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-4">
        <Card>
          <CardContent class="pt-6">
            <p class="text-xs text-[#b5b0a6] uppercase tracking-wider mb-1">累计命中率</p>
            <p class="text-3xl font-bold" :class="hitRateClass">
              {{ pct(stats.hit_rate_pct) }}
            </p>
            <p class="text-xs text-[#8c8475] mt-2">
              cache_read / (input + cache_read + cache_create)
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardContent class="pt-6">
            <p class="text-xs text-[#b5b0a6] uppercase tracking-wider mb-1">1h 注入率</p>
            <p class="text-3xl font-bold" :class="oneHourShareClass">
              {{ pct(stats.one_hour_share_pct) }}
            </p>
            <p class="text-xs text-[#8c8475] mt-2">
              cache_creation_1h / total_creation
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardContent class="pt-6">
            <p class="text-xs text-[#b5b0a6] uppercase tracking-wider mb-1">配额节省比例</p>
            <p class="text-3xl font-bold text-emerald-600">
              {{ pct(stats.saved_pct) }}
            </p>
            <p class="text-xs text-[#8c8475] mt-2">
              vs 假设全部按 1.0x 计费
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardContent class="pt-6">
            <p class="text-xs text-[#b5b0a6] uppercase tracking-wider mb-1">总命中 token</p>
            <p class="text-3xl font-bold text-[#c4704f]">
              {{ fmt(stats.cache_read_tokens) }}
            </p>
            <p class="text-xs text-[#8c8475] mt-2">
              {{ fmt(stats.sniffed_requests) }} 个请求
            </p>
          </CardContent>
        </Card>
      </div>

      <!-- 命中率进度条 -->
      <Card>
        <CardContent class="pt-6">
          <div class="flex items-center justify-between mb-2">
            <span class="text-sm font-medium text-[#29261e]">命中率分布</span>
            <span class="text-xs text-[#8c8475]">总 {{ fmt(stats.total_tokens) }} tokens</span>
          </div>
          <div class="h-8 rounded-lg overflow-hidden flex">
            <!-- cache_read (命中) -->
            <div
              class="bg-emerald-500 flex items-center justify-center text-white text-xs font-medium"
              :style="{ width: stats.total_tokens > 0 ? (stats.cache_read_tokens / stats.total_tokens * 100) + '%' : '0%' }"
              :title="`命中: ${fmt(stats.cache_read_tokens)}`"
            >
              <span v-if="stats.cache_read_tokens / stats.total_tokens > 0.05">命中</span>
            </div>
            <!-- cache_creation_1h (1h 写入) -->
            <div
              class="bg-amber-400 flex items-center justify-center text-white text-xs font-medium"
              :style="{ width: stats.total_tokens > 0 ? (stats.cache_creation_1h_tokens / stats.total_tokens * 100) + '%' : '0%' }"
              :title="`1h 写入: ${fmt(stats.cache_creation_1h_tokens)}`"
            >
              <span v-if="stats.cache_creation_1h_tokens / stats.total_tokens > 0.05">1h</span>
            </div>
            <!-- cache_creation_5m (5m 写入) -->
            <div
              class="bg-orange-400 flex items-center justify-center text-white text-xs font-medium"
              :style="{ width: stats.total_tokens > 0 ? (stats.cache_creation_5m_tokens / stats.total_tokens * 100) + '%' : '0%' }"
              :title="`5m 写入: ${fmt(stats.cache_creation_5m_tokens)}`"
            >
              <span v-if="stats.cache_creation_5m_tokens / stats.total_tokens > 0.05">5m</span>
            </div>
            <!-- input (新输入) -->
            <div
              class="bg-blue-400 flex items-center justify-center text-white text-xs font-medium"
              :style="{ width: stats.total_tokens > 0 ? (stats.input_tokens / stats.total_tokens * 100) + '%' : '0%' }"
              :title="`新输入: ${fmt(stats.input_tokens)}`"
            >
              <span v-if="stats.input_tokens / stats.total_tokens > 0.05">输入</span>
            </div>
          </div>
          <div class="flex flex-wrap gap-x-4 gap-y-1 mt-3 text-xs text-[#8c8475]">
            <span><span class="inline-block w-3 h-3 bg-emerald-500 rounded mr-1 align-middle"></span>cache_read: {{ fmt(stats.cache_read_tokens) }}</span>
            <span><span class="inline-block w-3 h-3 bg-amber-400 rounded mr-1 align-middle"></span>cache_creation_1h: {{ fmt(stats.cache_creation_1h_tokens) }}</span>
            <span><span class="inline-block w-3 h-3 bg-orange-400 rounded mr-1 align-middle"></span>cache_creation_5m: {{ fmt(stats.cache_creation_5m_tokens) }}</span>
            <span><span class="inline-block w-3 h-3 bg-blue-400 rounded mr-1 align-middle"></span>input: {{ fmt(stats.input_tokens) }}</span>
          </div>
        </CardContent>
      </Card>

      <!-- 调度健康指标 -->
      <Card>
        <CardContent class="pt-6">
          <p class="text-sm font-medium text-[#29261e] mb-4">调度健康指标</p>
          <div class="grid grid-cols-1 md:grid-cols-2 gap-6">
            <div>
              <p class="text-xs text-[#b5b0a6] uppercase tracking-wider mb-2">Sticky session fail-over</p>
              <div class="flex items-baseline gap-3">
                <span class="text-2xl font-bold" :class="stickyHealthClass">
                  {{ stats.sticky_preserved_total }}
                </span>
                <span class="text-sm text-[#8c8475]">软切（账号被限保留绑定）</span>
              </div>
              <div class="flex items-baseline gap-3 mt-1">
                <span class="text-lg font-medium text-[#8c8475]">
                  {{ stats.sticky_evicted_total }}
                </span>
                <span class="text-sm text-[#8c8475]">硬切（账号永久失效删绑）</span>
              </div>
              <p v-if="stats.sticky_preserved_total + stats.sticky_evicted_total === 0"
                 class="text-xs text-[#b5b0a6] mt-2">还没触发过 fail-over</p>
            </div>

            <div>
              <p class="text-xs text-[#b5b0a6] uppercase tracking-wider mb-2">OAuth session_key 自愈</p>
              <div class="flex items-baseline gap-3">
                <span class="text-2xl font-bold text-emerald-600">
                  {{ stats.oauth_recovery_success }}
                </span>
                <span class="text-sm text-[#8c8475]">refresh 失败被 session_key 救回</span>
              </div>
              <div class="flex items-baseline gap-3 mt-1">
                <span class="text-lg font-medium text-red-500">
                  {{ stats.oauth_recovery_failure }}
                </span>
                <span class="text-sm text-[#8c8475]">session_key 也失效（需人工）</span>
              </div>
              <p v-if="stats.oauth_recovery_success + stats.oauth_recovery_failure === 0"
                 class="text-xs text-[#b5b0a6] mt-2">还没触发过自愈</p>
            </div>
          </div>
        </CardContent>
      </Card>

      <!-- 按账号命中率明细（借鉴 sub2api 的账号排行榜样式）-->
      <Card>
        <CardContent class="pt-6">
          <div class="flex items-center justify-between mb-4">
            <p class="text-sm font-medium text-[#29261e]">按账号命中率明细</p>
            <p class="text-xs text-[#8c8475]">
              共 {{ stats.per_account.length }} 个账号有数据，按命中 token 量降序
            </p>
          </div>
          <div v-if="stats.per_account.length === 0" class="text-center py-8 text-sm text-[#b5b0a6]">
            还没有任何账号被请求过。等 VSCode / claude CLI 发出请求后会出现。
          </div>
          <div v-else class="overflow-x-auto">
            <table class="w-full text-sm">
              <thead>
                <tr class="border-b border-[#e8e2d9] text-left text-xs uppercase tracking-wider text-[#b5b0a6]">
                  <th class="py-2 px-2">账号</th>
                  <th class="py-2 px-2 text-right">请求</th>
                  <th class="py-2 px-2 text-right">命中</th>
                  <th class="py-2 px-2 text-right">1h 写入</th>
                  <th class="py-2 px-2 text-right">5m 写入</th>
                  <th class="py-2 px-2 text-right">命中率</th>
                  <th class="py-2 px-2 text-right">1h 比</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="acc in stats.per_account" :key="acc.account_id"
                    class="border-b border-[#f0ebe4] hover:bg-[#fafaf6] transition-colors">
                  <td class="py-2.5 px-2">
                    <div class="flex items-center gap-2">
                      <span class="text-xs text-[#b5b0a6] font-mono">#{{ acc.account_id }}</span>
                      <span class="text-[#29261e]">{{ acc.account_email || '-' }}</span>
                    </div>
                  </td>
                  <td class="py-2.5 px-2 text-right text-[#8c8475] tabular-nums">{{ acc.sniffed_requests }}</td>
                  <td class="py-2.5 px-2 text-right text-emerald-700 tabular-nums">{{ fmt(acc.cache_read_tokens) }}</td>
                  <td class="py-2.5 px-2 text-right text-amber-700 tabular-nums">{{ fmt(acc.cache_creation_1h_tokens) }}</td>
                  <td class="py-2.5 px-2 text-right text-orange-600 tabular-nums">{{ fmt(acc.cache_creation_5m_tokens) }}</td>
                  <td class="py-2.5 px-2 text-right">
                    <span class="font-medium tabular-nums" :class="accountHitClass(acc.hit_rate_pct)">
                      {{ acc.hit_rate_pct.toFixed(1) }}%
                    </span>
                  </td>
                  <td class="py-2.5 px-2 text-right">
                    <span class="font-medium tabular-nums" :class="accountOneHourClass(acc.one_hour_share_pct, acc.cache_creation_1h_tokens + acc.cache_creation_5m_tokens)">
                      <template v-if="acc.cache_creation_1h_tokens + acc.cache_creation_5m_tokens > 0">
                        {{ acc.one_hour_share_pct.toFixed(1) }}%
                      </template>
                      <template v-else>-</template>
                    </span>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </CardContent>
      </Card>

      <!-- 详细数字 -->
      <Card>
        <CardContent class="pt-6">
          <p class="text-sm font-medium text-[#29261e] mb-4">详细累计数据</p>
          <div class="grid grid-cols-2 md:grid-cols-3 gap-4 text-sm">
            <div>
              <p class="text-xs text-[#b5b0a6] mb-1">嗅探到的请求数</p>
              <p class="font-medium text-[#29261e]">{{ stats.sniffed_requests.toLocaleString() }}</p>
            </div>
            <div>
              <p class="text-xs text-[#b5b0a6] mb-1">总输入 tokens</p>
              <p class="font-medium text-[#29261e]">{{ stats.total_tokens.toLocaleString() }}</p>
            </div>
            <div>
              <p class="text-xs text-[#b5b0a6] mb-1">原始新输入</p>
              <p class="font-medium text-[#29261e]">{{ stats.input_tokens.toLocaleString() }}</p>
            </div>
            <div>
              <p class="text-xs text-[#b5b0a6] mb-1">缓存命中</p>
              <p class="font-medium text-emerald-700">{{ stats.cache_read_tokens.toLocaleString() }}</p>
            </div>
            <div>
              <p class="text-xs text-[#b5b0a6] mb-1">5m 缓存写入</p>
              <p class="font-medium text-orange-600">{{ stats.cache_creation_5m_tokens.toLocaleString() }}</p>
            </div>
            <div>
              <p class="text-xs text-[#b5b0a6] mb-1">1h 缓存写入</p>
              <p class="font-medium text-amber-700">{{ stats.cache_creation_1h_tokens.toLocaleString() }}</p>
            </div>
          </div>
          <p class="text-xs text-[#b5b0a6] mt-4 italic">
            ⚠️ 重启 cc-bridge 会清零所有计数器（数据存内存）。要长期统计需接入 Prometheus 抓 /metrics。
          </p>
        </CardContent>
      </Card>
    </template>
  </div>
</template>
