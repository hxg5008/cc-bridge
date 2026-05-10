<script setup lang="ts">
import { ref, computed, onMounted } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { api, type Dashboard as DashboardData, type AccountCategory } from '../api';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { logout } from '../router';

const route = useRoute();
const router = useRouter();

/** 仪表盘统计数据 */
const dashboard = ref<DashboardData | null>(null);

/** 加载仪表盘数据 */
async function loadDashboard() {
  try {
    dashboard.value = await api.getDashboard();
  } catch {
    // 忽略瞬态错误
  }
}

/** 格式化大数字为千分位 */
function formatNum(n: number): string {
  return n.toLocaleString();
}

function platformLabel(p: string): string {
  switch (p) {
    case 'claude': return 'Anthropic';
    case 'openai': return 'OpenAI';
    case 'gemini': return 'Gemini';
    case 'antigravity': return 'Antigravity';
    default: return p;
  }
}

function platformBadgeClass(p: string): string {
  switch (p) {
    case 'claude': return 'bg-orange-100 text-orange-700';
    case 'openai': return 'bg-emerald-100 text-emerald-700';
    case 'gemini': return 'bg-blue-100 text-blue-700';
    case 'antigravity': return 'bg-purple-100 text-purple-700';
    default: return 'bg-gray-100 text-gray-700';
  }
}

/** 当前筛选 (来自 URL ?filter=) */
const currentFilter = computed<AccountCategory | 'all'>(() => {
  const f = route.query.filter as string | undefined;
  if (!f) return 'all';
  if (['available', 'rate_limited', 'invalid', 'banned', 'stopped'].includes(f)) {
    return f as AccountCategory;
  }
  return 'all';
});

/** 点击顶部卡片切换筛选 */
function setFilter(f: AccountCategory | 'all') {
  if (route.name !== 'accounts' && route.name !== 'dashboard') {
    router.push({ name: 'accounts', query: f === 'all' ? {} : { filter: f } });
    return;
  }
  router.push({
    name: route.name as string,
    query: f === 'all' ? {} : { filter: f },
  });
}

/** 可调度百分比文字 */
const schedulablePctText = computed(() => {
  const d = dashboard.value;
  if (!d) return '';
  const pct = d.accounts.schedulable_pct ?? 0;
  return `${(pct * 100).toFixed(1)}%`;
});

onMounted(loadDashboard);
</script>

<template>
  <div class="min-h-screen">
    <!-- 顶部导航栏 -->
    <header class="sticky top-0 z-40 bg-white/80 backdrop-blur-md border-b border-[#e8e2d9]/60 px-6 py-3">
      <div class="max-w-7xl mx-auto flex items-center justify-between">
        <div class="flex items-center gap-6">
          <div class="flex items-center gap-2">
            <img src="/favicon.svg" alt="Logo" class="w-6 h-6" />
            <h1 class="text-lg font-semibold text-[#29261e] tracking-tight">cc-bridge</h1>
          </div>
          <nav class="flex items-center gap-1">
            <router-link
              :to="{ name: 'accounts' }"
              class="px-3 py-1.5 text-sm rounded-lg transition-colors"
              :class="route.name === 'accounts' || route.name === 'dashboard'
                ? 'bg-[#c4704f]/10 text-[#c4704f] font-medium'
                : 'text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]'"
            >
              账号
            </router-link>
            <router-link
              :to="{ name: 'tokens' }"
              class="px-3 py-1.5 text-sm rounded-lg transition-colors"
              :class="route.name === 'tokens'
                ? 'bg-[#c4704f]/10 text-[#c4704f] font-medium'
                : 'text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]'"
            >
              令牌
            </router-link>
            <router-link
              :to="{ name: 'cache-stats' }"
              class="px-3 py-1.5 text-sm rounded-lg transition-colors"
              :class="route.name === 'cache-stats'
                ? 'bg-[#c4704f]/10 text-[#c4704f] font-medium'
                : 'text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]'"
            >
              缓存统计
            </router-link>
          </nav>
        </div>
        <Button
          variant="ghost"
          size="sm"
          @click="logout"
          class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]"
        >
          退出
        </Button>
      </div>
    </header>

    <main class="max-w-7xl mx-auto px-6 py-6 space-y-6">
      <!-- 顶部统计：7 卡片 (总账号 / 可用 / 限流 / 失效 / 封禁 / 停用 / 令牌) -->
      <div v-if="dashboard" class="space-y-3">
        <div class="grid grid-cols-2 md:grid-cols-4 lg:grid-cols-7 gap-3">
          <!-- 总账号: 点击重置筛选 -->
          <Card
            class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 !py-0 !gap-0 cursor-pointer"
            :class="currentFilter === 'all' ? 'ring-2 ring-[#c4704f]' : ''"
            @click="setFilter('all')"
          >
            <CardContent class="py-3 px-4">
              <p class="text-[#8c8475] text-xs mb-1">总账号</p>
              <p class="text-2xl font-bold text-[#29261e]">{{ formatNum(dashboard.accounts.total) }}</p>
              <div v-if="dashboard.accounts.by_platform" class="flex flex-wrap gap-1 mt-1">
                <span v-for="(count, platform) in dashboard.accounts.by_platform" :key="platform"
                  class="px-1.5 py-0.5 rounded text-[10px] font-medium"
                  :class="platformBadgeClass(platform as string)">
                  {{ platformLabel(platform as string) }} {{ count }}
                </span>
              </div>
            </CardContent>
          </Card>

          <!-- 可用 (绿) -->
          <Card
            class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 !py-0 !gap-0 cursor-pointer"
            :class="currentFilter === 'available' ? 'ring-2 ring-emerald-500' : ''"
            @click="setFilter('available')"
          >
            <CardContent class="py-3 px-4">
              <p class="text-[#8c8475] text-xs mb-1">可用</p>
              <p class="text-2xl font-bold text-emerald-600">{{ formatNum(dashboard.accounts.available ?? 0) }}</p>
            </CardContent>
          </Card>

          <!-- 限流中 (橙) -->
          <Card
            class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 !py-0 !gap-0 cursor-pointer"
            :class="currentFilter === 'rate_limited' ? 'ring-2 ring-orange-500' : ''"
            @click="setFilter('rate_limited')"
          >
            <CardContent class="py-3 px-4">
              <p class="text-[#8c8475] text-xs mb-1">限流中</p>
              <p class="text-2xl font-bold text-orange-500">{{ formatNum(dashboard.accounts.rate_limited ?? 0) }}</p>
            </CardContent>
          </Card>

          <!-- 失效 (红) -->
          <Card
            class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 !py-0 !gap-0 cursor-pointer"
            :class="currentFilter === 'invalid' ? 'ring-2 ring-red-500' : ''"
            @click="setFilter('invalid')"
          >
            <CardContent class="py-3 px-4">
              <p class="text-[#8c8475] text-xs mb-1">失效</p>
              <p class="text-2xl font-bold text-red-500">{{ formatNum(dashboard.accounts.invalid ?? 0) }}</p>
            </CardContent>
          </Card>

          <!-- 封禁 (深灰) -->
          <Card
            class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 !py-0 !gap-0 cursor-pointer"
            :class="currentFilter === 'banned' ? 'ring-2 ring-gray-700' : ''"
            @click="setFilter('banned')"
          >
            <CardContent class="py-3 px-4">
              <p class="text-[#8c8475] text-xs mb-1">封禁</p>
              <p class="text-2xl font-bold text-gray-700">{{ formatNum(dashboard.accounts.banned ?? 0) }}</p>
            </CardContent>
          </Card>

          <!-- 停用 (浅灰) -->
          <Card
            class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 !py-0 !gap-0 cursor-pointer"
            :class="currentFilter === 'stopped' ? 'ring-2 ring-[#b5b0a6]' : ''"
            @click="setFilter('stopped')"
          >
            <CardContent class="py-3 px-4">
              <p class="text-[#8c8475] text-xs mb-1">停用</p>
              <p class="text-2xl font-bold text-[#b5b0a6]">{{ formatNum(dashboard.accounts.stopped ?? 0) }}</p>
            </CardContent>
          </Card>

          <!-- 令牌 -->
          <Card class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 !py-0 !gap-0">
            <CardContent class="py-3 px-4">
              <p class="text-[#8c8475] text-xs mb-1">令牌</p>
              <p class="text-2xl font-bold text-[#29261e]">{{ formatNum(dashboard.tokens) }}</p>
            </CardContent>
          </Card>
        </div>

        <!-- 可调度比例 -->
        <div v-if="dashboard.accounts.total > 0" class="text-xs text-[#8c8475] px-1">
          可调度: <span class="font-medium text-[#5c5647]">{{ dashboard.accounts.available ?? 0 }} / {{ dashboard.accounts.total }}</span>
          <span class="mx-1">·</span>
          <span class="font-medium"
            :class="(dashboard.accounts.schedulable_pct ?? 0) >= 0.5
              ? 'text-emerald-600'
              : (dashboard.accounts.schedulable_pct ?? 0) >= 0.2
                ? 'text-orange-500'
                : 'text-red-500'">
            {{ schedulablePctText }}
          </span>
        </div>
      </div>

      <!-- 子路由内容 -->
      <router-view @refresh="loadDashboard" />
    </main>
  </div>
</template>
