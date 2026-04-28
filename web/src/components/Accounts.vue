<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue';
import { useRoute } from 'vue-router';
import { api, type Account, type AccountCategory, type OAuthExchangeResult, type OpenAITokenInfo, type UsageData } from '../api';
import { Card } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Textarea } from '@/components/ui/textarea';
import { Badge } from '@/components/ui/badge';
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter,
} from '@/components/ui/dialog';
import { useToast } from '../composables/useToast';

const emit = defineEmits<{ refresh: [] }>();
const { show: toast } = useToast();

/** 账号列表 */
const accounts = ref<Account[]>([]);
/** 分页状态 */
const currentPage = ref(1);
const totalPages = ref(1);
const totalCount = ref(0);
/** 视图模式: 'detail' = 现有卡片详情视图, 'compact' = 表格紧凑视图 (大量账号场景) */
const viewMode = ref<'detail' | 'compact'>(
  (localStorage.getItem('cc-bridge.view_mode') as 'detail' | 'compact') || 'detail',
);
/** pageSize 自适应: detail 12 / compact 50 */
const pageSize = computed(() => (viewMode.value === 'compact' ? 50 : 12));
function onViewModeChange(mode: 'detail' | 'compact') {
  if (viewMode.value === mode) return;
  viewMode.value = mode;
  localStorage.setItem('cc-bridge.view_mode', mode);
  currentPage.value = 1; // 切换视图重置到第 1 页, 因 pageSize 变了
  load();
}
/** 关键字搜索 (前端 client-side, 按 email/name 过滤) */
const searchQuery = ref<string>('');
/** 平台过滤 (UI 端筛选; '' = 全部) */
const platformFilter = ref<string>('');
/** 分类筛选 (来自 URL ?filter=, 跟 dashboard 顶部卡片联动) */
const route = useRoute();
const categoryFilter = computed<AccountCategory | 'all'>(() => {
  const f = route.query.filter as string | undefined;
  if (!f) return 'all';
  if (['available', 'rate_limited', 'invalid', 'banned', 'stopped'].includes(f)) {
    return f as AccountCategory;
  }
  return 'all';
});
const filteredAccounts = computed(() => {
  let list = accounts.value;
  if (platformFilter.value) {
    list = list.filter((a) =>
      platformFilter.value === 'claude'
        ? !a.platform || a.platform === 'claude'
        : a.platform === platformFilter.value,
    );
  }
  if (categoryFilter.value !== 'all') {
    list = list.filter((a) => a.category === categoryFilter.value);
  }
  if (searchQuery.value.trim()) {
    const q = searchQuery.value.trim().toLowerCase();
    list = list.filter((a) =>
      (a.email || '').toLowerCase().includes(q) ||
      (a.name || '').toLowerCase().includes(q),
    );
  }
  return list;
});

/** 5 类徽章中文文字 */
function categoryLabel(c?: AccountCategory): string {
  switch (c) {
    case 'available': return '可用';
    case 'rate_limited': return '限流中';
    case 'invalid': return '失效';
    case 'banned': return '封禁';
    case 'stopped': return '停用';
    default: return '未知';
  }
}

/** 5 类徽章颜色 */
function categoryBadgeClass(c?: AccountCategory): string {
  switch (c) {
    case 'available': return 'bg-emerald-100 text-emerald-700';
    case 'rate_limited': return 'bg-orange-100 text-orange-700';
    case 'invalid': return 'bg-red-100 text-red-700';
    case 'banned': return 'bg-gray-700 text-white';
    case 'stopped': return 'bg-gray-200 text-gray-600';
    default: return 'bg-gray-100 text-gray-500';
  }
}

/** 限流恢复倒计时（人类可读） */
function recoversInText(iso?: string): string {
  if (!iso) return '';
  const t = Date.parse(iso);
  if (isNaN(t)) return '';
  const sec = Math.max(0, Math.ceil((t - Date.now()) / 1000));
  if (sec === 0) return '即将恢复';
  if (sec < 60) return `${sec}s 后恢复`;
  if (sec < 3600) return `${Math.ceil(sec / 60)}m 后恢复`;
  return `${Math.floor(sec / 3600)}h${Math.ceil((sec % 3600) / 60)}m 后恢复`;
}
/** 表单弹窗是否可见 */
const showForm = ref(false);
/** 删除确认弹窗是否可见 */
const showDeleteConfirm = ref(false);
/** 待删除账号 ID */
const deleteTargetId = ref<number | null>(null);
/** 当前编辑的账号（null 表示新建） */
const editing = ref<Account | null>(null);
/** 是否在编辑 OpenAI 账号 (用于分平台渲染表单字段) */
const isOpenAIEdit = computed(() => editing.value?.platform === 'openai');
/** 表单数据 */
const form = ref({
  name: '',
  email: '',
  auth_type: 'setup_token',
  setup_token: '',
  access_token: '',
  refresh_token: '',
  expires_at: '',
  proxy_url: '',
  billing_mode: 'strip',
  account_uuid: '',
  organization_uuid: '',
  subscription_type: '',
  concurrency: 5,
  priority: 50,
  auto_telemetry: false,
  experimental_reveal_thinking: false,
  // OpenAI 专用 (走 account.extra,仅在编辑 platform=openai 账号时显示)
  chatgpt_account_id: '',
  organization_id: '',
  base_url: '',
});
/** 正在测试的账号 ID */
const testing = ref<number | null>(null);
/** 测试结果 */
const testResult = ref<{ status: string; message?: string } | null>(null);
/** 正在刷新用量的账号 ID */
const refreshingUsage = ref<number | null>(null);

/** 加载账号列表 */
async function load() {
  try {
    const res = await api.listAccounts(currentPage.value, pageSize.value);
    accounts.value = res.data ?? [];
    totalPages.value = res.total_pages;
    totalCount.value = res.total;
  } catch {
    accounts.value = [];
  }
}

/** 翻页 */
function goToPage(page: number) {
  if (page < 1 || page > totalPages.value) return;
  currentPage.value = page;
  load();
}

/** 可见的页码列表 */
const visiblePages = computed(() => {
  const pages: number[] = [];
  const total = totalPages.value;
  const current = currentPage.value;
  let start = Math.max(1, current - 2);
  let end = Math.min(total, start + 4);
  start = Math.max(1, end - 4);
  for (let i = start; i <= end; i++) pages.push(i);
  return pages;
});

/** 自动重载定时器 + 间隔（秒,0=关闭） */
let autoReloadTimer: ReturnType<typeof setInterval> | null = null;
const AUTO_REFRESH_OPTIONS = [
  { value: 0, label: '不自动' },
  { value: 5, label: '5 秒' },
  { value: 10, label: '10 秒' },
  { value: 30, label: '30 秒' },
  { value: 60, label: '60 秒' },
];
const autoRefreshSec = ref<number>(
  Number(localStorage.getItem('cc-bridge.auto_refresh_sec') ?? 60),
);

function applyAutoRefreshTimer() {
  if (autoReloadTimer) {
    clearInterval(autoReloadTimer);
    autoReloadTimer = null;
  }
  if (autoRefreshSec.value > 0) {
    autoReloadTimer = setInterval(async () => {
      // 1. 触发后端批量刷新所有 OAuth 账号用量 (走 60s 服务端缓存,不会真频繁打上游)
      try {
        await api.refreshAllUsage();
      } catch {
        // 失败不阻断本地 reload
      }
      // 2. 重拉账号列表 + dashboard 计数
      await load();
      emit('refresh');
    }, autoRefreshSec.value * 1000);
  }
}

function onAutoRefreshChange() {
  localStorage.setItem('cc-bridge.auto_refresh_sec', String(autoRefreshSec.value));
  applyAutoRefreshTimer();
}

onMounted(() => {
  load();
  applyAutoRefreshTimer();
  // 每秒推进一次 tick，让 rate_limited_until 倒计时活起来
  tickTimer = setInterval(() => {
    tick.value = (tick.value + 1) % 1_000_000;
  }, 1000);
});

onUnmounted(() => {
  if (autoReloadTimer) {
    clearInterval(autoReloadTimer);
    autoReloadTimer = null;
  }
  if (tickTimer) {
    clearInterval(tickTimer);
    tickTimer = null;
  }
});

/** 打开新建账号弹窗 */
function openCreate() {
  editing.value = null;
  form.value = {
    name: '',
    email: '',
    auth_type: 'setup_token',
    setup_token: '',
    access_token: '',
    refresh_token: '',
    expires_at: '',
    proxy_url: '',
    billing_mode: 'strip',
    account_uuid: '',
    organization_uuid: '',
    subscription_type: '',
    concurrency: 5,
    priority: 50,
    auto_telemetry: false,
    experimental_reveal_thinking: false,
    chatgpt_account_id: '',
    organization_id: '',
    base_url: '',
  };
  showForm.value = true;
}

/**
 * 打开编辑账号弹窗
 * @param a 要编辑的账号对象
 */
function openEdit(a: Account) {
  editing.value = a;
  const extra = (a.extra ?? {}) as Record<string, unknown>;
  form.value = {
    name: a.name,
    email: a.email,
    auth_type: a.auth_type || 'setup_token',
    setup_token: '',
    access_token: '',
    refresh_token: '',
    expires_at: a.expires_at ? String(a.expires_at) : '',
    proxy_url: a.proxy_url,
    billing_mode: a.billing_mode || 'strip',
    account_uuid: a.account_uuid || '',
    organization_uuid: a.organization_uuid || '',
    subscription_type: a.subscription_type || '',
    concurrency: a.concurrency,
    priority: a.priority,
    auto_telemetry: a.auto_telemetry ?? false,
    experimental_reveal_thinking: a.experimental_reveal_thinking ?? false,
    chatgpt_account_id: typeof extra.chatgpt_account_id === 'string' ? extra.chatgpt_account_id : '',
    organization_id: typeof extra.organization_id === 'string' ? extra.organization_id : '',
    base_url: typeof extra.base_url === 'string' ? extra.base_url : '',
  };
  showForm.value = true;
}

/** 保存账号（新建或更新） */
async function save() {
  try {
    const expiresAt = form.value.expires_at.trim();
    const normalizedExpiresAt = normalizeExpiresAtInput(expiresAt);
    if (editing.value) {
      const isOpenAI = editing.value.platform === 'openai';
      if (form.value.auth_type === 'setup_token'
        && !form.value.setup_token.trim()
        && editing.value.auth_type !== 'setup_token') {
        throw new Error('切换到 Setup Token 模式时必须填写 Setup Token');
      }
      if (form.value.auth_type === 'oauth'
        && !form.value.refresh_token.trim()
        && editing.value.auth_type !== 'oauth') {
        throw new Error('切换到 OAuth 模式时必须填写 Refresh Token');
      }
      const updates: Record<string, unknown> = {};
      if (form.value.name) updates.name = form.value.name;
      if (form.value.email) updates.email = form.value.email;
      updates.auth_type = form.value.auth_type;
      if (form.value.setup_token) updates.setup_token = form.value.setup_token;
      if (form.value.access_token) updates.access_token = form.value.access_token;
      if (form.value.refresh_token) updates.refresh_token = form.value.refresh_token;
      if (normalizedExpiresAt) updates.expires_at = normalizedExpiresAt;
      updates.proxy_url = form.value.proxy_url;
      updates.concurrency = form.value.concurrency;
      updates.priority = form.value.priority;
      if (isOpenAI) {
        // OpenAI 账号: 把平台特有字段塞 extra,空串发后端会被当作"删除"
        updates.extra = {
          chatgpt_account_id: form.value.chatgpt_account_id.trim(),
          organization_id: form.value.organization_id.trim(),
          base_url: form.value.base_url.trim(),
        };
      } else {
        // Claude 账号: billing_mode / 订阅 / Account/Org UUID / 自动遥测 / 思考显示
        updates.billing_mode = form.value.billing_mode;
        updates.account_uuid = form.value.account_uuid || null;
        updates.organization_uuid = form.value.organization_uuid || null;
        updates.subscription_type = form.value.subscription_type || null;
        updates.auto_telemetry = form.value.auto_telemetry;
        updates.experimental_reveal_thinking = form.value.experimental_reveal_thinking;
      }
      await api.updateAccount(editing.value.id, updates);
    } else {
      if (form.value.auth_type === 'setup_token' && !form.value.setup_token.trim()) {
        throw new Error('Setup Token 不能为空');
      }
      if (form.value.auth_type === 'oauth' && !form.value.refresh_token.trim()) {
        throw new Error('Refresh Token 不能为空');
      }
      const payload: Record<string, unknown> = {
        name: form.value.name,
        email: form.value.email,
        auth_type: form.value.auth_type,
        setup_token: form.value.setup_token,
        access_token: form.value.access_token,
        refresh_token: form.value.refresh_token,
        proxy_url: form.value.proxy_url,
        billing_mode: form.value.billing_mode,
        account_uuid: form.value.account_uuid || null,
        organization_uuid: form.value.organization_uuid || null,
        subscription_type: form.value.subscription_type || null,
        concurrency: form.value.concurrency,
        priority: form.value.priority,
        auto_telemetry: form.value.auto_telemetry,
        experimental_reveal_thinking: form.value.experimental_reveal_thinking,
      };
      if (normalizedExpiresAt) payload.expires_at = normalizedExpiresAt;
      await api.createAccount(payload);
    }
    showForm.value = false;
    await load();
    emit('refresh');
  } catch (e: unknown) {
    toast((e as Error).message || '保存失败');
  }
}

function normalizeExpiresAtInput(raw: string): string | null {
  if (!raw) return null;

  if (/^\d+$/.test(raw)) {
    const date = new Date(Number(raw));
    if (Number.isNaN(date.getTime())) {
      throw new Error('expires_at 不是合法的毫秒时间戳');
    }
    return date.toISOString();
  }

  const date = new Date(raw);
  if (Number.isNaN(date.getTime())) {
    throw new Error('expires_at 不是合法的时间格式');
  }
  return date.toISOString();
}

/**
 * 确认删除账号
 * @param id 账号 ID
 */
function confirmDelete(id: number) {
  deleteTargetId.value = id;
  showDeleteConfirm.value = true;
}

/** 执行删除账号 */
async function executeDelete() {
  if (deleteTargetId.value === null) return;
  try {
    await api.deleteAccount(deleteTargetId.value);
    showDeleteConfirm.value = false;
    deleteTargetId.value = null;
    await load();
    emit('refresh');
  } catch (e: unknown) {
    toast((e as Error).message || '删除失败');
  }
}

/**
 * 测试账号连接
 * @param id 账号 ID
 */
async function test(id: number) {
  testing.value = id;
  testResult.value = null;
  try {
    testResult.value = await api.testAccount(id);
    if (testResult.value.status === 'error') {
      toast(testResult.value.message || '测试失败');
    }
  } catch (e: unknown) {
    toast((e as Error).message || '测试请求失败');
  }
  setTimeout(() => { testing.value = null; testResult.value = null; }, 3000);
}

/**
 * 刷新账号用量数据
 * @param id 账号 ID
 */
async function refreshUsage(id: number) {
  refreshingUsage.value = id;
  try {
    const res = await api.refreshUsage(id);
    if (res.status === 'ok' && res.usage) {
      const acc = accounts.value.find(a => a.id === id);
      if (acc) {
        acc.usage_data = res.usage;
        acc.usage_fetched_at = new Date().toISOString();
      }
    } else if (res.status === 'error') {
      toast(res.message || '刷新用量失败');
    }
  } catch (e: unknown) {
    toast((e as Error).message || '刷新用量失败');
  }
  refreshingUsage.value = null;
}

/** 清除内存中残留的短期限流标记（死锁逃生口） */
const clearingLimit = ref<number | null>(null);
async function clearLimit(id: number) {
  clearingLimit.value = id;
  try {
    const res = await api.clearLimit(id);
    if (res.cleared) {
      toast('已清除限流标记，账号已恢复调度');
      // 立即从前端模型里去掉倒计时,不等下次刷新
      const acc = accounts.value.find(a => a.id === id);
      if (acc) acc.rate_limited_until_runtime = undefined;
    } else {
      toast('当前没有可清除的限流标记');
    }
    await load();
  } catch (e: unknown) {
    toast((e as Error).message || '清除失败');
  }
  clearingLimit.value = null;
}

/** 倒计时计算: rate_limited_until_runtime - now，单位秒 */
function rateLimitRemainingSec(iso?: string): number {
  if (!iso) return 0;
  const t = Date.parse(iso);
  if (isNaN(t)) return 0;
  const sec = Math.ceil((t - Date.now()) / 1000);
  return sec > 0 ? sec : 0;
}

/** UI 触发的"现在"，每秒滴答一次让倒计时刷新（不刷数据） */
const tick = ref(0);
let tickTimer: ReturnType<typeof setInterval> | null = null;

/**
 * 切换账号调度状态（启用/停用）
 * @param a 账号对象
 */
async function toggleScheduling(a: Account) {
  try {
    const isStopped = a.status === 'disabled' || isRateLimited(a);
    const newStatus = isStopped ? 'active' : 'disabled';
    const res = await api.updateAccount(a.id, { status: newStatus });
    a.status = res.status;
    a.disable_reason = res.disable_reason ?? '';
    a.rate_limited_at = res.rate_limited_at;
    a.rate_limit_reset_at = res.rate_limit_reset_at;
    emit('refresh');
  } catch (e: unknown) {
    toast((e as Error).message || '切换调度失败');
  }
}

/**
 * 格式化剩余时间
 * @param resetsAt ISO 时间字符串
 */
function formatTimeLeft(resetsAt: string): string {
  const diff = new Date(resetsAt).getTime() - Date.now();
  if (diff <= 0) return '已重置';
  const days = Math.floor(diff / 86400000);
  const hours = Math.floor((diff % 86400000) / 3600000);
  const minutes = Math.floor((diff % 3600000) / 60000);
  if (days > 0) return `${days}d${hours}h${minutes}m`;
  if (hours > 0) return `${hours}h${minutes}m`;
  return `${minutes}m`;
}

/**
 * 获取用量进度条颜色
 * @param pct 使用百分比 (0-100)
 */
function usageBarColor(pct: number): string {
  if (pct >= 80) return 'bg-red-400';
  if (pct >= 50) return 'bg-amber-400';
  return 'bg-emerald-400';
}

/**
 * 用量徽章行是否需要展示（任一徽章字段存在即展示）。
 */
function usageHasBadges(ud?: UsageData | null): boolean {
  if (!ud) return false;
  return !!(
    (ud.status && ud.status !== 'allowed') ||
    ud.overage_status === 'rejected' ||
    ud.source === 'headers'
  );
}

/**
 * 瓶颈窗口标识转可读中文。
 */
function formatClaim(claim: string): string {
  switch (claim) {
    case 'five_hour': return '5 小时';
    case 'seven_day': return '7 天';
    case 'seven_day_opus': return '7 天 Opus';
    case 'seven_day_sonnet': return '7 天 Sonnet';
    default: return claim;
  }
}

/**
 * 判断账号是否正在被限流
 */
function isRateLimited(a: Account): boolean {
  return !!(a.rate_limit_reset_at && new Date(a.rate_limit_reset_at) > new Date());
}

/**
 * 获取状态徽章样式 (优先用后端返回的 category, 兜底回退到旧 status 判断)
 */
function statusStyle(a: Account): { class: string; label: string } {
  if (a.category) {
    return {
      class: `${categoryBadgeClass(a.category)} border border-transparent`,
      label: categoryLabel(a.category),
    };
  }
  // 老服务端兼容
  if (a.status === 'active' && isRateLimited(a)) {
    return { class: 'bg-amber-50 text-amber-700 border-amber-200', label: '限流中' };
  }
  if (a.status === 'active') return { class: 'bg-emerald-50 text-emerald-700 border-emerald-200', label: '活跃' };
  if (a.status === 'error') return { class: 'bg-red-50 text-red-600 border-red-200', label: '异常' };
  return { class: 'bg-gray-100 text-gray-500 border-gray-200', label: '停用' };
}

/**
 * 遮蔽 Token 显示
 * @param t Token 原始值
 */
function maskToken(t: string): string {
  if (t.length <= 20) return t;
  return t.slice(0, 20) + '...';
}

/**
 * 获取认证方式标签
 * @param authType 认证方式
 */
function authTypeLabel(authType: string): string {
  return authType === 'oauth' ? 'OAuth' : 'Setup Token';
}

/**
 * 获取当前账号显示的凭证摘要
 * @param account 账号对象
 */
function authSecretPreview(account: Account): string {
  if (account.auth_type === 'oauth') {
    return maskToken(account.refresh_token || account.access_token || '未配置');
  }
  return maskToken(account.setup_token || '未配置');
}

/**
 * 格式化 OAuth 过期时间
 * @param expiresAt 毫秒时间戳
 */
function formatExpiresAt(expiresAt?: number | null): string {
  if (!expiresAt) return '未提供';
  return new Date(expiresAt).toLocaleString('zh-CN');
}

/**
 * 格式化字节数为可读字符串
 */
function formatBytes(bytes?: number): string {
  if (!bytes) return '—';
  if (bytes >= 1_073_741_824) return (bytes / 1_073_741_824).toFixed(0) + 'G';
  if (bytes >= 1_048_576) return (bytes / 1_048_576).toFixed(0) + 'M';
  if (bytes >= 1024) return (bytes / 1024).toFixed(0) + 'K';
  return bytes + 'B';
}

/** 切换认证方式 */
function setAuthType(authType: 'setup_token' | 'oauth') {
  form.value.auth_type = authType;
}

// --- OAuth 授权流程 ---
const showOAuthFlow = ref(false);
const oauthMode = ref<'oauth' | 'setup_token'>('oauth');
const oauthProxyUrl = ref('');
const oauthSessionId = ref('');
const oauthAuthUrl = ref('');
const oauthCode = ref('');
const oauthLoading = ref(false);
const oauthResult = ref<OAuthExchangeResult | null>(null);
const oauthStep = ref<'generate' | 'exchange' | 'done'>('generate');

// ===== SessionKey 一键导入 =====
const showSkImport = ref(false);
const skImportText = ref('');                                    // 多行文本框, 每行一个 sessionKey
const skImportProxyUrl = ref('');                                // 美国/海外代理
const skImportScope = ref<'full' | 'inference'>('full');         // OAuth scope
const skImportConcurrency = ref(3);                              // 并发上限
const skImportBillingMode = ref<'strip' | 'rewrite'>('strip');
const skImportAutoTelemetry = ref(false);
const skImportSubscription = ref<string>('');
const skImportLoading = ref(false);
const skImportStep = ref<'form' | 'result'>('form');             // 'form' = 输入, 'result' = 展示结果
const skImportResults = ref<{ total: number; success: number; failed: number; results: any[] } | null>(null);
const skImportLastInputs = ref<string[]>([]);                    // 最近一次提交的原始 sessionKey, 用于重试匹配

/** 给定原始 sessionKey, 计算后端 mask_session_key 的脱敏值; 用来匹配 result.session_key_preview */
function maskSk(sk: string): string {
  if (sk.length <= 18) return '***';
  return `${sk.slice(0, 12)}...${sk.slice(-6)}`;
}

function openSessionKeyImport() {
  skImportText.value = '';
  skImportProxyUrl.value = oauthProxyUrl.value || '';            // 复用上次填的代理
  skImportScope.value = 'full';
  skImportConcurrency.value = 3;
  skImportBillingMode.value = 'strip';
  skImportAutoTelemetry.value = false;
  skImportSubscription.value = '';
  skImportStep.value = 'form';
  skImportResults.value = null;
  skImportLoading.value = false;
  showSkImport.value = true;
}

async function runSessionKeyImport() {
  // 解析多行 sessionKey, 去空白 + 去重
  const seen = new Set<string>();
  const sessionKeys = skImportText.value
    .split('\n')
    .map((line) => line.trim())
    .filter((s) => {
      if (!s || seen.has(s)) return false;
      seen.add(s);
      return true;
    });

  if (sessionKeys.length === 0) {
    toast('请粘贴 sessionKey', 'error');
    return;
  }

  await runBatchInternal(sessionKeys);
}

/** 重试失败行: 从最近一次结果里拿 FAIL 的 sessionKey 再跑一遍 */
async function retryFailedImports() {
  if (!skImportResults.value || skImportLastInputs.value.length === 0) return;
  const failedPreviews = new Set(
    skImportResults.value.results
      .filter((r: any) => !r.success)
      .map((r: any) => r.session_key_preview)
  );
  const retryKeys = skImportLastInputs.value.filter((sk) =>
    failedPreviews.has(maskSk(sk))
  );
  if (retryKeys.length === 0) {
    toast('没有可重试的失败行');
    return;
  }
  await runBatchInternal(retryKeys);
}

async function runBatchInternal(sessionKeys: string[]) {
  skImportLastInputs.value = sessionKeys;
  skImportLoading.value = true;
  try {
    const resp = await api.cookieAuthCreateBatch({
      session_keys: sessionKeys,
      proxy_url: skImportProxyUrl.value || undefined,
      scope: skImportScope.value,
      concurrency_limit: skImportConcurrency.value,
      billing_mode: skImportBillingMode.value,
      auto_telemetry: skImportAutoTelemetry.value,
      subscription_type: skImportSubscription.value || undefined,
    });
    skImportResults.value = resp;
    skImportStep.value = 'result';
    toast(
      `导入完成: 成功 ${resp.success} / 失败 ${resp.failed} / 共 ${resp.total}`,
      resp.failed === 0 ? 'success' : 'error',
    );
    // 刷新账号列表
    await load();
  } catch (e: any) {
    toast(`导入失败: ${e.message}`, 'error');
  } finally {
    skImportLoading.value = false;
  }
}

function closeSessionKeyImport() {
  showSkImport.value = false;
}

// ===== OpenAI 账号导入 (Phase 7) =====
const showOpenAIImport = ref(false);
const oaMode = ref<'rt' | 'api_key' | 'codex_token' | 'oauth'>('rt');
const oaForm = ref({
  email: '',
  api_key: '',
  base_url: '',
  organization_id: '',
  user_agent: '',
  proxy_url: '',
});
const oaRtText = ref('');                                       // RT 多行文本框
const oaRtConcurrency = ref(3);
const oaRtResults = ref<{ total: number; success: number; failed: number; results: any[] } | null>(null);
const oaRtStep = ref<'form' | 'result'>('form');
const oaOAuthStep = ref<'generate' | 'exchange' | 'done'>('generate');
const oaSessionId = ref('');
const oaState = ref('');
const oaAuthUrl = ref('');
const oaCode = ref('');
const oaResult = ref<OpenAITokenInfo | null>(null);
const oaLoading = ref(false);

function openOpenAIImport() {
  oaMode.value = 'rt';
  oaForm.value = {
    email: '',
    api_key: '',
    base_url: '',
    organization_id: '',
    user_agent: '',
    proxy_url: '',
  };
  oaRtText.value = '';
  oaRtConcurrency.value = 3;
  oaRtResults.value = null;
  oaRtStep.value = 'form';
  oaOAuthStep.value = 'generate';
  oaSessionId.value = '';
  oaState.value = '';
  oaAuthUrl.value = '';
  oaCode.value = '';
  oaResult.value = null;
  oaLoading.value = false;
  showOpenAIImport.value = true;
}

async function submitOpenAIRtImport() {
  const seen = new Set<string>();
  const tokens = oaRtText.value
    .split('\n')
    .map((s) => s.trim())
    .filter((s) => {
      if (!s || seen.has(s)) return false;
      seen.add(s);
      return true;
    });
  if (tokens.length === 0) {
    toast('请粘贴 refresh_token', 'error');
    return;
  }
  oaLoading.value = true;
  try {
    const r = await api.openaiRtImportBatch({
      refresh_tokens: tokens,
      proxy_url: oaForm.value.proxy_url || undefined,
      user_agent: oaForm.value.user_agent || undefined,
      base_url: oaForm.value.base_url || undefined,
      concurrency_limit: oaRtConcurrency.value,
    });
    oaRtResults.value = r;
    oaRtStep.value = 'result';
    toast(
      `导入: 成功 ${r.success} / 失败 ${r.failed} / 共 ${r.total}`,
      r.failed === 0 ? 'success' : 'error',
    );
    await load();
  } catch (e: any) {
    toast(`失败: ${e.message}`, 'error');
  } finally {
    oaLoading.value = false;
  }
}

async function submitOpenAIToken() {
  if (!oaForm.value.email) {
    toast('email 必填', 'error');
    return;
  }
  if (!oaForm.value.api_key) {
    toast('token 必填', 'error');
    return;
  }
  oaLoading.value = true;
  try {
    await api.createOpenAIAccount({
      email: oaForm.value.email,
      credential_type: oaMode.value === 'codex_token' ? 'codex_token' : 'api_key',
      api_key: oaForm.value.api_key,
      base_url: oaForm.value.base_url || undefined,
      organization_id: oaForm.value.organization_id || undefined,
      user_agent: oaForm.value.user_agent || undefined,
      proxy_url: oaForm.value.proxy_url || undefined,
    });
    toast('账号已添加', 'success');
    showOpenAIImport.value = false;
    await load();
  } catch (e: any) {
    toast(`失败: ${e.message}`, 'error');
  } finally {
    oaLoading.value = false;
  }
}

async function generateOpenAIAuthUrl() {
  oaLoading.value = true;
  try {
    const r = await api.openaiGenerateAuthUrl({
      proxy_url: oaForm.value.proxy_url || undefined,
    });
    oaAuthUrl.value = r.auth_url;
    oaSessionId.value = r.session_id;
    oaState.value = r.state;
    oaOAuthStep.value = 'exchange';
    // 自动复制 + 打开浏览器
    try {
      await navigator.clipboard.writeText(r.auth_url);
    } catch {}
    window.open(r.auth_url, '_blank');
  } catch (e: any) {
    toast(`生成失败: ${e.message}`, 'error');
  } finally {
    oaLoading.value = false;
  }
}

async function exchangeOpenAICode() {
  if (!oaCode.value) {
    toast('请粘贴 code', 'error');
    return;
  }
  oaLoading.value = true;
  try {
    const r = await api.openaiExchangeCode({
      session_id: oaSessionId.value,
      code: oaCode.value,
      state: oaState.value || undefined,
    });
    oaResult.value = r;
    if (!oaForm.value.email && r.email) oaForm.value.email = r.email;
    oaOAuthStep.value = 'done';
  } catch (e: any) {
    toast(`交换失败: ${e.message}`, 'error');
  } finally {
    oaLoading.value = false;
  }
}

async function applyOpenAIOAuthAccount() {
  if (!oaResult.value) return;
  if (!oaForm.value.email) {
    toast('email 必填', 'error');
    return;
  }
  oaLoading.value = true;
  try {
    await api.createOpenAIAccount({
      email: oaForm.value.email,
      credential_type: 'oauth',
      access_token: oaResult.value.access_token,
      refresh_token: oaResult.value.refresh_token,
      organization_id:
        oaForm.value.organization_id || oaResult.value.organization_id || undefined,
      chatgpt_account_id: oaResult.value.chatgpt_account_id || undefined,
      base_url: oaForm.value.base_url || undefined,
      user_agent: oaForm.value.user_agent || undefined,
      proxy_url: oaForm.value.proxy_url || undefined,
    });
    toast('OAuth 账号已添加', 'success');
    showOpenAIImport.value = false;
    await load();
  } catch (e: any) {
    toast(`保存失败: ${e.message}`, 'error');
  } finally {
    oaLoading.value = false;
  }
}

/** 打开 OAuth 授权流程弹窗 */
function openOAuthFlow() {
  oauthMode.value = 'oauth';
  oauthProxyUrl.value = '';
  oauthSessionId.value = '';
  oauthAuthUrl.value = '';
  oauthCode.value = '';
  oauthResult.value = null;
  oauthStep.value = 'generate';
  oauthLoading.value = false;
  showOAuthFlow.value = true;
}

/** 生成授权链接 */
async function generateOAuthUrl() {
  oauthLoading.value = true;
  try {
    const proxy = oauthProxyUrl.value.trim() || undefined;
    const res = oauthMode.value === 'oauth'
      ? await api.generateAuthUrl(proxy)
      : await api.generateSetupTokenUrl(proxy);
    oauthSessionId.value = res.session_id;
    oauthAuthUrl.value = res.auth_url;
    oauthStep.value = 'exchange';
  } catch (e: unknown) {
    toast((e as Error).message || '生成授权链接失败');
  }
  oauthLoading.value = false;
}

/** 交换 code */
async function exchangeOAuthCode() {
  const code = oauthCode.value.trim();
  if (!code) {
    toast('请输入授权码');
    return;
  }
  oauthLoading.value = true;
  try {
    const res = oauthMode.value === 'oauth'
      ? await api.exchangeCode(oauthSessionId.value, code)
      : await api.exchangeSetupTokenCode(oauthSessionId.value, code);
    oauthResult.value = res;
    oauthStep.value = 'done';
  } catch (e: unknown) {
    toast((e as Error).message || '交换 Token 失败');
  }
  oauthLoading.value = false;
}

/** 将授权结果填入新建账号表单 */
function applyOAuthResult() {
  const r = oauthResult.value;
  if (!r) return;
  showOAuthFlow.value = false;
  editing.value = null;
  const isSetupToken = oauthMode.value === 'setup_token';
  form.value = {
    name: '',
    email: r.email_address || '',
    auth_type: isSetupToken ? 'setup_token' : 'oauth',
    setup_token: isSetupToken ? r.access_token : '',
    access_token: isSetupToken ? '' : (r.access_token || ''),
    refresh_token: isSetupToken ? '' : (r.refresh_token || ''),
    expires_at: (!isSetupToken && r.expires_at) ? String(r.expires_at * 1000) : '',
    proxy_url: oauthProxyUrl.value || '',
    billing_mode: 'strip',
    account_uuid: r.account_uuid || '',
    organization_uuid: r.organization_uuid || '',
    subscription_type: '',
    concurrency: 5,
    priority: 50,
    auto_telemetry: false,
    experimental_reveal_thinking: false,
    chatgpt_account_id: '',
    organization_id: '',
    base_url: '',
  };
  showForm.value = true;
}

/** 复制文本到剪贴板（兼容非安全上下文） */
async function copyText(text: string) {
  if (!text) {
    toast('没有可复制的内容');
    return;
  }

  // 1. 优先使用 Clipboard API（仅在安全上下文 HTTPS / localhost 可用）
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      toast('已复制');
      return;
    } catch {
      // 失败则继续走降级方案
    }
  }

  // 2. 降级方案：临时 textarea + execCommand('copy')
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.top = '0';
    ta.style.left = '0';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    ta.setSelectionRange(0, text.length);
    const ok = document.execCommand('copy');
    document.body.removeChild(ta);
    toast(ok ? '已复制' : '复制失败');
  } catch {
    toast('复制失败');
  }
}
</script>

<template>
  <div class="space-y-4">
    <!-- 标题栏 -->
    <div class="flex flex-wrap justify-between items-center gap-y-2">
      <div class="flex items-center gap-3 flex-wrap">
        <h2 class="text-lg font-semibold text-[#29261e]">账号管理</h2>
        <!-- 视图切换 -->
        <div class="flex items-center bg-[#f9f6f1] border border-[#e8e2d9] rounded-lg overflow-hidden">
          <button
            type="button"
            @click="onViewModeChange('detail')"
            class="px-3 py-1.5 text-sm transition-colors"
            :class="viewMode === 'detail'
              ? 'bg-[#c4704f] text-white font-medium'
              : 'text-[#5c5647] hover:bg-[#e8e2d9]/40'"
          >
            详情
          </button>
          <button
            type="button"
            @click="onViewModeChange('compact')"
            class="px-3 py-1.5 text-sm transition-colors"
            :class="viewMode === 'compact'
              ? 'bg-[#c4704f] text-white font-medium'
              : 'text-[#5c5647] hover:bg-[#e8e2d9]/40'"
          >
            紧凑
          </button>
        </div>
        <select
          v-model="platformFilter"
          class="text-sm bg-white border border-[#e8e2d9] rounded-lg px-3 py-1.5 text-[#5c5647] hover:border-[#c4704f]/50 focus:outline-none focus:border-[#c4704f]"
        >
          <option value="">全部平台</option>
          <option value="claude">Anthropic</option>
          <option value="openai">OpenAI</option>
          <option value="gemini">Gemini</option>
          <option value="antigravity">Antigravity</option>
        </select>
        <select
          v-model.number="autoRefreshSec"
          @change="onAutoRefreshChange"
          class="text-sm bg-white border border-[#e8e2d9] rounded-lg px-3 py-1.5 text-[#5c5647] hover:border-[#c4704f]/50 focus:outline-none focus:border-[#c4704f]"
          title="自动刷新: 拉用量 + 重渲账号列表 + dashboard 计数。后端走 60s 缓存,不会真高频打上游 API"
        >
          <option v-for="opt in AUTO_REFRESH_OPTIONS" :key="opt.value" :value="opt.value">
            自动刷新: {{ opt.label }}
          </option>
        </select>
        <input
          v-model="searchQuery"
          type="search"
          placeholder="搜索 email / name..."
          class="text-sm bg-white border border-[#e8e2d9] rounded-lg px-3 py-1.5 text-[#5c5647] hover:border-[#c4704f]/50 focus:outline-none focus:border-[#c4704f] w-48"
        />
        <span class="text-xs text-[#8c8475]">{{ filteredAccounts.length }} / {{ accounts.length }}</span>
      </div>
      <div class="flex gap-2">
        <Button
          @click="openSessionKeyImport"
          class="bg-[#5b8a72] hover:bg-[#4a7a62] text-white font-medium rounded-xl transition-all duration-200 hover:shadow-md"
        >
          SessionKey 导入
        </Button>
        <Button
          @click="openOpenAIImport"
          class="bg-[#3a6ea5] hover:bg-[#2f5d8e] text-white font-medium rounded-xl transition-all duration-200 hover:shadow-md"
        >
          OpenAI 导入
        </Button>
        <Button
          @click="openOAuthFlow"
          class="bg-[#c4704f] hover:bg-[#b5623f] text-white font-medium rounded-xl transition-all duration-200 hover:shadow-md"
        >
          授权登录
        </Button>
        <Button
          @click="openCreate"
          class="bg-[#c4704f] hover:bg-[#b5623f] text-white font-medium rounded-xl transition-all duration-200 hover:shadow-md"
        >
          添加账号
        </Button>
      </div>
    </div>

    <!-- 紧凑视图: 表格 -->
    <div v-if="viewMode === 'compact'" class="bg-white border border-[#e8e2d9] rounded-xl overflow-hidden">
      <div class="overflow-x-auto">
        <table class="min-w-full text-sm">
          <thead class="bg-[#f9f6f1] border-b border-[#e8e2d9]">
            <tr class="text-left text-xs font-medium text-[#8c8475]">
              <th class="px-3 py-2.5 whitespace-nowrap">Email / Name</th>
              <th class="px-2 py-2.5 whitespace-nowrap">平台</th>
              <th class="px-2 py-2.5 whitespace-nowrap">鉴权</th>
              <th class="px-2 py-2.5 whitespace-nowrap">状态</th>
              <th class="px-2 py-2.5 whitespace-nowrap text-right">5h</th>
              <th class="px-2 py-2.5 whitespace-nowrap text-right">7d</th>
              <th class="px-2 py-2.5 whitespace-nowrap">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="a in filteredAccounts"
              :key="a.id"
              class="border-b border-[#f0ebe4] hover:bg-[#f9f6f1]/50 cursor-pointer transition-colors"
              @click="openEdit(a)"
            >
              <td class="px-3 py-2 align-middle">
                <div class="font-medium text-[#29261e] truncate max-w-[260px]" :title="a.email">{{ a.email }}</div>
                <div v-if="a.name && a.name !== a.email" class="text-xs text-[#8c8475] truncate max-w-[260px]">{{ a.name }}</div>
              </td>
              <td class="px-2 py-2 align-middle">
                <span class="px-1.5 py-0.5 rounded text-[10px] font-medium"
                      :class="a.platform === 'openai'
                        ? 'bg-emerald-100 text-emerald-700'
                        : 'bg-orange-100 text-orange-700'">
                  {{ a.platform === 'openai' ? 'OpenAI' : 'Anth' }}
                </span>
              </td>
              <td class="px-2 py-2 align-middle text-xs text-[#5c5647]">
                {{ a.auth_type === 'oauth' ? 'OAuth' : 'Token' }}
              </td>
              <td class="px-2 py-2 align-middle">
                <Badge :class="statusStyle(a).class" class="border text-xs font-medium">
                  {{ statusStyle(a).label }}
                </Badge>
                <div v-if="a.category === 'rate_limited' && a.category_recovers_at"
                     class="text-[10px] text-orange-700 mt-0.5">
                  {{ tick, recoversInText(a.category_recovers_at) }}
                </div>
              </td>
              <td class="px-2 py-2 align-middle text-right tabular-nums text-xs"
                  :class="((a.usage_data?.five_hour?.utilization ?? 0) >= 100) ? 'text-orange-600 font-semibold' : 'text-[#5c5647]'">
                {{ a.usage_data?.five_hour?.utilization != null
                    ? (a.usage_data.five_hour.utilization as number).toFixed(0) + '%'
                    : '—' }}
              </td>
              <td class="px-2 py-2 align-middle text-right tabular-nums text-xs"
                  :class="((a.usage_data?.seven_day?.utilization ?? 0) >= 100) ? 'text-orange-600 font-semibold' : 'text-[#5c5647]'">
                {{ a.usage_data?.seven_day?.utilization != null
                    ? (a.usage_data.seven_day.utilization as number).toFixed(0) + '%'
                    : '—' }}
              </td>
              <td class="px-2 py-2 align-middle whitespace-nowrap" @click.stop>
                <div class="flex gap-1">
                  <button
                    type="button"
                    @click="test(a.id)"
                    :disabled="testing === a.id"
                    class="text-xs px-2 py-1 rounded text-[#c4704f] hover:bg-[#c4704f]/10 disabled:opacity-50"
                    title="测试账号活性"
                  >测试</button>
                  <button
                    v-if="(!a.platform || a.platform === 'claude')"
                    type="button"
                    @click="refreshUsage(a.id)"
                    :disabled="refreshingUsage === a.id"
                    class="text-xs px-2 py-1 rounded text-[#c4704f] hover:bg-[#c4704f]/10 disabled:opacity-50"
                    title="刷新用量"
                  >用量</button>
                  <button
                    v-if="rateLimitRemainingSec(a.rate_limited_until_runtime) > 0 || tick === -1"
                    type="button"
                    @click="clearLimit(a.id)"
                    :disabled="clearingLimit === a.id"
                    class="text-xs px-2 py-1 rounded text-amber-600 hover:bg-amber-50 disabled:opacity-50"
                    :title="`清除限流标记 (剩余 ${rateLimitRemainingSec(a.rate_limited_until_runtime)}s)`"
                  >清限流</button>
                  <button
                    type="button"
                    @click="confirmDelete(a.id)"
                    class="text-xs px-2 py-1 rounded text-red-500 hover:bg-red-50"
                    title="删除账号"
                  >删除</button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <div v-if="filteredAccounts.length === 0" class="text-center text-sm text-[#8c8475] py-8">
        没有符合条件的账号
      </div>
    </div>

    <!-- 详情视图: 卡片 (现有布局, 不动) -->
    <div v-else class="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-4">
      <Card
        v-for="a in filteredAccounts"
        :key="a.id"
        class="bg-white border-[#e8e2d9] rounded-xl hover:shadow-md transition-all duration-200 overflow-hidden"
        :class="(a.status === 'disabled' || isRateLimited(a)) ? 'opacity-60' : ''"
      >
        <div class="p-5 space-y-3">
          <!-- 头部：名称 + 状态 -->
          <div class="flex items-center justify-between">
            <div class="flex items-center gap-2 min-w-0">
              <div class="w-8 h-8 rounded-lg flex items-center justify-center flex-shrink-0"
                   :class="a.platform === 'openai' ? 'bg-emerald-100' : 'bg-[#c4704f]/10'">
                <span class="text-sm font-semibold"
                      :class="a.platform === 'openai' ? 'text-emerald-600' : 'text-[#c4704f]'">{{ (a.name || a.email)[0].toUpperCase() }}</span>
              </div>
              <div class="min-w-0">
                <p class="text-sm font-medium text-[#29261e] truncate">{{ a.name || a.email }}</p>
                <p v-if="a.name" class="text-xs text-[#8c8475] truncate">{{ a.email }}</p>
              </div>
            </div>
            <div class="flex flex-col items-end gap-1 flex-shrink-0">
              <Badge :class="statusStyle(a).class" class="border text-xs font-medium">
                {{ statusStyle(a).label }}
              </Badge>
              <!-- 限流中: 显示子原因 + 倒计时 (tick 是 hidden 依赖, 让倒计时每秒重渲染) -->
              <span v-if="a.category === 'rate_limited' && a.category_reason"
                    class="text-[10px] text-orange-700 max-w-[160px] text-right leading-tight"
                    :title="`${a.category_reason}${a.category_recovers_at ? ' · ' + recoversInText(a.category_recovers_at) : ''}`">
                {{ a.category_reason }}
                <span v-if="a.category_recovers_at" class="opacity-70 block">{{ tick, recoversInText(a.category_recovers_at) }}</span>
              </span>
              <!-- 失效/封禁/停用: 显示原因 -->
              <span v-else-if="(a.category === 'invalid' || a.category === 'banned' || a.category === 'stopped') && a.category_reason"
                    class="text-[10px] max-w-[160px] text-right leading-tight"
                    :class="a.category === 'invalid' ? 'text-red-600' : 'text-gray-500'"
                    :title="a.category_reason">
                {{ a.category_reason.length > 24 ? a.category_reason.slice(0, 24) + '…' : a.category_reason }}
              </span>
              <span class="px-1.5 py-0.5 rounded text-[10px] font-medium"
                    :class="a.platform === 'openai'
                      ? 'bg-emerald-100 text-emerald-700'
                      : 'bg-orange-100 text-orange-700'">
                {{ a.platform === 'openai' ? 'OpenAI' : 'Anthropic' }}
                <span class="opacity-70">· {{ a.auth_type === 'oauth' ? 'OAuth' : 'Token' }}</span>
                <span v-if="a.platform === 'openai' && a.extra && (a.extra as any).plan_type"
                      class="ml-1 opacity-70">{{ (a.extra as any).plan_type }}</span>
              </span>
            </div>
          </div>

          <!-- 信息 -->
          <div class="pt-2 border-t border-[#f0ebe4] space-y-2">
            <div class="grid grid-cols-3 gap-3">
              <div class="text-center">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">并发</p>
                <p class="text-sm font-medium text-[#29261e]">{{ Math.max(0, a.current_concurrency ?? 0) }}/{{ a.concurrency }}</p>
              </div>
              <div class="text-center">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">优先级</p>
                <p class="text-sm font-medium text-[#29261e]">{{ a.priority }}</p>
              </div>
              <div class="text-center">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">Billing</p>
                <p class="text-sm font-medium" :class="a.billing_mode === 'rewrite' ? 'text-amber-600' : 'text-[#29261e]'">
                  {{ a.billing_mode === 'rewrite' ? '重写' : '清除' }}
                </p>
              </div>
            </div>
            <div class="space-y-3">
              <div>
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">代理</p>
                <p class="text-sm text-[#8c8475] truncate">{{ a.proxy_url || '直连' }}</p>
              </div>
              <div>
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">认证方式</p>
                <p class="text-sm text-[#8c8475] truncate">{{ authTypeLabel(a.auth_type) }}</p>
              </div>
              <div v-if="(!a.platform || a.platform === 'claude')">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">自动遥测</p>
                <p class="text-sm" :class="a.auto_telemetry ? 'text-emerald-600' : 'text-[#8c8475]'">
                  {{ a.auto_telemetry ? '已开启' : '关闭' }}
                  <span v-if="a.telemetry_count > 0" class="text-[#b5b0a6] text-xs">· 已发送 {{ a.telemetry_count }} 次</span>
                </p>
                <p v-if="a.telemetry_expires_at" class="text-xs text-amber-500 mt-0.5">
                  遥测中 · 停止于 {{ new Date(a.telemetry_expires_at).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', second: '2-digit' }) }}
                </p>
              </div>
              <div>
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">
                  {{ a.auth_type === 'oauth' ? 'Refresh Token' : 'Setup Token' }}
                </p>
                <p class="font-mono text-[11px] text-[#8c8475] truncate">{{ authSecretPreview(a) }}</p>
              </div>
              <div v-if="a.auth_type === 'oauth'">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">过期时间</p>
                <p class="text-sm text-[#8c8475] truncate">{{ formatExpiresAt(a.expires_at) }}</p>
              </div>
              <div v-if="a.auth_type === 'oauth' && a.auth_error">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">认证错误</p>
                <p class="text-xs text-red-500 line-clamp-2">{{ a.auth_error }}</p>
              </div>
              <div v-if="(!a.platform || a.platform === 'claude')">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">环境指纹</p>
                <p class="text-xs text-[#8c8475] truncate">
                  {{ a.canonical_env?.platform || '—' }} / {{ a.canonical_env?.arch || '—' }} · v{{ a.canonical_env?.version || '—' }}
                </p>
              </div>
              <div v-if="(!a.platform || a.platform === 'claude')">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">提示词环境</p>
                <p class="text-xs text-[#8c8475] truncate">
                  {{ a.canonical_prompt_env?.platform || '—' }} · {{ a.canonical_prompt_env?.shell || '—' }} · {{ a.canonical_prompt_env?.working_dir || '—' }}
                </p>
              </div>
              <div v-if="(!a.platform || a.platform === 'claude')">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">进程指纹</p>
                <p class="text-xs text-[#8c8475] truncate">
                  内存 {{ formatBytes(a.canonical_process?.constrained_memory) }} · RSS {{ formatBytes(a.canonical_process?.rss_range?.[0]) }}–{{ formatBytes(a.canonical_process?.rss_range?.[1]) }}
                </p>
              </div>

              <!-- OpenAI 专属信息 -->
              <div v-if="a.platform === 'openai' && a.extra && (a.extra as any).chatgpt_account_id">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">ChatGPT Account</p>
                <p class="font-mono text-[11px] text-[#8c8475] truncate">{{ (a.extra as any).chatgpt_account_id }}</p>
              </div>
              <div v-if="a.platform === 'openai' && a.extra && (a.extra as any).organization_id">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">Organization</p>
                <p class="font-mono text-[11px] text-[#8c8475] truncate">{{ (a.extra as any).organization_id }}</p>
              </div>
              <div v-if="a.platform === 'openai' && a.extra && (a.extra as any).base_url">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider mb-0.5">Base URL</p>
                <p class="text-xs text-[#8c8475] truncate">{{ (a.extra as any).base_url }}</p>
              </div>
            </div>
          </div>

          <!-- 用量窗口 (仅 Claude) -->
          <div v-if="(!a.platform || a.platform === 'claude')" class="pt-2 border-t border-[#f0ebe4] space-y-2">
            <div class="flex items-center justify-between">
              <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">用量</p>
              <p v-if="a.usage_fetched_at" class="text-[10px] text-[#b5b0a6]">
                {{ new Date(a.usage_fetched_at).toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' }) }}
              </p>
              <p v-else class="text-[10px] text-[#b5b0a6]">未刷新</p>
            </div>
            <!-- 状态徽章行：全局 status / overage / 瓶颈 / 数据源 -->
            <div v-if="usageHasBadges(a.usage_data)" class="flex flex-wrap gap-1">
              <span v-if="a.usage_data?.status && a.usage_data.status !== 'allowed'"
                class="px-1.5 py-0.5 rounded text-[10px] font-medium"
                :class="a.usage_data.status === 'rejected' ? 'bg-red-50 text-red-600' : 'bg-amber-50 text-amber-700'">
                {{ a.usage_data.status === 'rejected' ? '已拒绝' : '预警' }}
              </span>
              <span v-if="a.usage_data?.representative_claim && a.usage_data?.status !== 'allowed'"
                class="px-1.5 py-0.5 rounded text-[10px] font-medium bg-orange-50 text-orange-600">
                瓶颈: {{ formatClaim(a.usage_data.representative_claim) }}
              </span>
              <span v-if="a.usage_data?.overage_status === 'rejected'"
                class="px-1.5 py-0.5 rounded text-[10px] font-medium bg-slate-50 text-slate-500"
                :title="a.usage_data?.overage_disabled_reason || ''">
                Overage 禁用
              </span>
              <span v-if="a.usage_data?.source === 'headers'"
                class="px-1.5 py-0.5 rounded text-[10px] font-medium bg-emerald-50 text-emerald-600"
                title="数据来自响应头（实时）">
                实时
              </span>
            </div>
            <!-- 5 小时 -->
            <div class="space-y-0.5">
              <div class="flex justify-between text-[11px]">
                <span class="text-[#8c8475] flex items-center gap-1">
                  5 小时
                  <span v-if="a.usage_data?.five_hour?.status && a.usage_data.five_hour.status !== 'allowed'"
                    class="inline-block w-1.5 h-1.5 rounded-full"
                    :class="a.usage_data.five_hour.status === 'rejected' ? 'bg-red-500' : 'bg-amber-500'"
                    :title="a.usage_data.five_hour.status" />
                </span>
                <span class="text-[#5c5647] font-medium">{{ a.usage_data?.five_hour ? Math.round(a.usage_data.five_hour.utilization) : '0' }}%
                  <span v-if="a.usage_data?.five_hour" class="text-[#b5b0a6] font-normal">· {{ formatTimeLeft(a.usage_data.five_hour.resets_at) }}</span>
                </span>
              </div>
              <div class="h-1.5 bg-[#f0ebe4] rounded-full overflow-hidden">
                <div :class="usageBarColor(a.usage_data?.five_hour ? a.usage_data.five_hour.utilization : 0)"
                  class="h-full rounded-full transition-all duration-300"
                  :style="{ width: (a.usage_data?.five_hour ? Math.min(a.usage_data.five_hour.utilization, 100) : 0) + '%' }" />
              </div>
            </div>
            <!-- 7 天 -->
            <div class="space-y-0.5">
              <div class="flex justify-between text-[11px]">
                <span class="text-[#8c8475] flex items-center gap-1">
                  7 天
                  <span v-if="a.usage_data?.seven_day?.status && a.usage_data.seven_day.status !== 'allowed'"
                    class="inline-block w-1.5 h-1.5 rounded-full"
                    :class="a.usage_data.seven_day.status === 'rejected' ? 'bg-red-500' : 'bg-amber-500'"
                    :title="a.usage_data.seven_day.status" />
                </span>
                <span class="text-[#5c5647] font-medium">{{ a.usage_data?.seven_day ? Math.round(a.usage_data.seven_day.utilization) : '0' }}%
                  <span v-if="a.usage_data?.seven_day" class="text-[#b5b0a6] font-normal">· {{ formatTimeLeft(a.usage_data.seven_day.resets_at) }}</span>
                </span>
              </div>
              <div class="h-1.5 bg-[#f0ebe4] rounded-full overflow-hidden">
                <div :class="usageBarColor(a.usage_data?.seven_day ? a.usage_data.seven_day.utilization : 0)"
                  class="h-full rounded-full transition-all duration-300"
                  :style="{ width: (a.usage_data?.seven_day ? Math.min(a.usage_data.seven_day.utilization, 100) : 0) + '%' }" />
              </div>
            </div>
            <!-- 7 天 Sonnet -->
            <div class="space-y-0.5">
              <div class="flex justify-between text-[11px]">
                <span class="text-[#8c8475]">7 天 Sonnet</span>
                <span class="text-[#5c5647] font-medium">{{ a.usage_data?.seven_day_sonnet ? Math.round(a.usage_data.seven_day_sonnet.utilization) : '0' }}%
                  <span v-if="a.usage_data?.seven_day_sonnet" class="text-[#b5b0a6] font-normal">· {{ formatTimeLeft(a.usage_data.seven_day_sonnet.resets_at) }}</span>
                </span>
              </div>
              <div class="h-1.5 bg-[#f0ebe4] rounded-full overflow-hidden">
                <div :class="usageBarColor(a.usage_data?.seven_day_sonnet ? a.usage_data.seven_day_sonnet.utilization : 0)"
                  class="h-full rounded-full transition-all duration-300"
                  :style="{ width: (a.usage_data?.seven_day_sonnet ? Math.min(a.usage_data.seven_day_sonnet.utilization, 100) : 0) + '%' }" />
              </div>
            </div>
          </div>

          <!-- 停用原因 -->
          <div
            v-if="a.disable_reason && (a.status === 'disabled' || (a.rate_limit_reset_at && new Date(a.rate_limit_reset_at) > new Date()))"
            class="flex items-center gap-1.5 px-2.5 py-1.5 rounded-lg border"
            :class="a.status === 'disabled' ? 'bg-red-50 border-red-200' : 'bg-amber-50 border-amber-200'"
          >
            <span class="text-xs font-medium" :class="a.status === 'disabled' ? 'text-red-600' : 'text-amber-700'">
              {{ a.disable_reason }}
            </span>
            <span v-if="a.rate_limit_reset_at && new Date(a.rate_limit_reset_at) > new Date()" class="text-xs text-amber-500">
              · 剩余 {{ formatTimeLeft(a.rate_limit_reset_at) }}
            </span>
          </div>

          <!-- 测试结果 -->
          <div
            v-if="testing === a.id && testResult"
            class="text-xs font-medium px-2 py-1 rounded-lg text-center"
            :class="testResult.status === 'ok' ? 'bg-emerald-50 text-emerald-600' : 'bg-red-50 text-red-500'"
          >
            {{ testResult.status === 'ok' ? '连接正常' : testResult.message }}
          </div>

          <!-- 操作按钮 -->
          <div class="flex items-center gap-2 pt-2 border-t border-[#f0ebe4]">
            <Button
              variant="ghost"
              size="sm"
              @click="toggleScheduling(a)"
              :class="(a.status === 'disabled' || isRateLimited(a))
                ? 'text-emerald-500 hover:text-emerald-600 hover:bg-emerald-50'
                : 'text-amber-500 hover:text-amber-600 hover:bg-amber-50'"
              class="h-8 px-3 text-xs flex-1"
            >
              {{ (a.status === 'disabled' || isRateLimited(a)) ? '启用' : '停用' }}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              @click="openEdit(a)"
              class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4] h-8 px-3 text-xs flex-1"
            >
              编辑
            </Button>
            <Button
              v-if="(!a.platform || a.platform === 'claude')"
              variant="ghost"
              size="sm"
              @click="refreshUsage(a.id)"
              :disabled="refreshingUsage === a.id"
              class="text-[#c4704f] hover:text-[#b5623f] hover:bg-[#c4704f]/5 h-8 px-3 text-xs flex-1"
            >
              {{ refreshingUsage === a.id ? '刷新中...' : '用量' }}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              @click="test(a.id)"
              :disabled="testing === a.id"
              class="text-[#c4704f] hover:text-[#b5623f] hover:bg-[#c4704f]/5 h-8 px-3 text-xs flex-1"
            >
              {{ testing === a.id ? '测试中...' : '测试' }}
            </Button>
            <Button
              v-if="rateLimitRemainingSec(a.rate_limited_until_runtime) > 0 || tick === -1"
              variant="ghost"
              size="sm"
              @click="clearLimit(a.id)"
              :disabled="clearingLimit === a.id"
              :title="`内存软限流剩余 ${rateLimitRemainingSec(a.rate_limited_until_runtime)}s, 点击清除`"
              class="text-amber-600 hover:text-amber-700 hover:bg-amber-50 h-8 px-3 text-xs flex-1"
            >
              {{ clearingLimit === a.id
                ? '清除中...'
                : `清限流 ${rateLimitRemainingSec(a.rate_limited_until_runtime)}s` }}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              @click="confirmDelete(a.id)"
              class="text-red-400 hover:text-red-500 hover:bg-red-50 h-8 px-3 text-xs flex-1"
            >
              删除
            </Button>
          </div>
        </div>
      </Card>

      <!-- 空状态 -->
      <div
        v-if="filteredAccounts.length === 0"
        class="col-span-full flex flex-col items-center justify-center py-16 text-[#b5b0a6]"
      >
        <div class="w-12 h-12 rounded-xl bg-[#f0ebe4] flex items-center justify-center mb-3">
          <svg class="w-6 h-6 text-[#c4704f]/50" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="1.5">
            <path stroke-linecap="round" stroke-linejoin="round" d="M18 7.5v3m0 0v3m0-3h3m-3 0h-3m-2.25-4.125a3.375 3.375 0 1 1-6.75 0 3.375 3.375 0 0 1 6.75 0ZM3 19.235v-.11a6.375 6.375 0 0 1 12.75 0v.109A12.318 12.318 0 0 1 9.374 21c-2.331 0-4.512-.645-6.374-1.766Z" />
          </svg>
        </div>
        <p class="text-sm">暂无账号，点击"添加账号"开始</p>
      </div>
    </div>

    <!-- 分页 -->
    <div v-if="totalPages > 1" class="flex items-center justify-between pt-2">
      <p class="text-sm text-[#8c8475]">共 {{ totalCount }} 个账号</p>
      <div class="flex items-center gap-1">
        <Button
          variant="ghost"
          size="sm"
          :disabled="currentPage <= 1"
          @click="goToPage(currentPage - 1)"
          class="h-8 px-2 text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4] disabled:opacity-40"
        >
          上一页
        </Button>
        <Button
          v-for="p in visiblePages"
          :key="p"
          variant="ghost"
          size="sm"
          @click="goToPage(p)"
          class="h-8 w-8 p-0 text-sm"
          :class="p === currentPage
            ? 'bg-[#c4704f] text-white hover:bg-[#b5623f]'
            : 'text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]'"
        >
          {{ p }}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          :disabled="currentPage >= totalPages"
          @click="goToPage(currentPage + 1)"
          class="h-8 px-2 text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4] disabled:opacity-40"
        >
          下一页
        </Button>
      </div>
    </div>

    <!-- 新建/编辑账号弹窗 -->
    <Dialog v-model:open="showForm">
      <DialogContent class="bg-white border-[#e8e2d9] rounded-2xl text-[#29261e] sm:max-w-md max-h-[85vh] flex flex-col">
        <DialogHeader class="flex-shrink-0">
          <DialogTitle class="text-[#29261e] text-lg">{{ editing ? '编辑账号' : '添加账号' }}</DialogTitle>
          <DialogDescription class="text-[#8c8475]">
            {{ editing ? '修改账号信息，凭证留空表示不更改' : '填写新账号信息' }}
          </DialogDescription>
        </DialogHeader>

        <form @submit.prevent="save" class="space-y-4 mt-2 overflow-y-auto flex-1 pr-1">
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">备注名（选填）</Label>
            <Input
              v-model="form.name"
              class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20"
            />
          </div>
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">邮箱 <span class="text-red-500">*</span></Label>
            <Input
              v-model="form.email"
              required
              class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20"
            />
          </div>
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">认证方式</Label>
            <div class="flex gap-2">
              <button
                type="button"
                @click="setAuthType('setup_token')"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="form.auth_type === 'setup_token'
                  ? 'bg-[#c4704f]/10 border-[#c4704f] text-[#c4704f]'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-[#c4704f]/40'"
              >
                Setup Token
              </button>
              <button
                type="button"
                @click="setAuthType('oauth')"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="form.auth_type === 'oauth'
                  ? 'bg-amber-50 border-amber-400 text-amber-600'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
              >
                OAuth
              </button>
            </div>
          </div>
          <div v-if="form.auth_type === 'setup_token'" class="space-y-2">
            <Label class="text-[#5c5647] text-sm">
              Setup Token (sk-ant-oat01-...) <span v-if="!editing" class="text-red-500">*</span>
            </Label>
            <Textarea
              v-model="form.setup_token"
              :required="!editing"
              :rows="3"
              :placeholder="editing ? '留空保持不变' : ''"
              class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
            />
          </div>
          <template v-else>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">Access Token（选填）</Label>
              <Textarea
                v-model="form.access_token"
                :rows="2"
                :placeholder="editing ? '留空保持不变' : '已有 access token 时可直接填写'"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">
                Refresh Token <span class="text-red-500">*</span>
              </Label>
              <Textarea
                v-model="form.refresh_token"
                :required="!editing"
                :rows="2"
                :placeholder="editing ? '留空保持不变' : ''"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">Expires At（毫秒时间戳，选填）</Label>
              <Input
                v-model="form.expires_at"
                inputmode="numeric"
                placeholder="例如：1743600000000"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
          </template>
          <div v-if="editing && editing.auth_type === 'oauth' && editing.expires_at" class="rounded-lg bg-[#f9f6f1] px-3 py-2 text-xs text-[#8c8475]">
            当前过期时间：{{ formatExpiresAt(editing.expires_at) }}
          </div>
          <div v-if="editing && editing.auth_type === 'oauth' && editing.auth_error" class="rounded-lg bg-red-50 px-3 py-2 text-xs text-red-500">
            最近认证错误：{{ editing.auth_error }}
          </div>
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">代理地址（选填）</Label>
            <Input
              v-model="form.proxy_url"
              placeholder="http:// 或 socks5://"
              class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20"
            />
          </div>

          <!-- OpenAI 账号专用 -->
          <template v-if="isOpenAIEdit">
            <div class="rounded-lg bg-[#f9f6f1] px-3 py-2 text-xs text-[#8c8475]">
              已识别为 OpenAI 账号,以下字段写入 <code class="text-[#c4704f]">extra</code>。
              UA / Originator / instructions 由网关强制覆盖,无需手填。
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">ChatGPT Account ID（OAuth 账号必填）</Label>
              <Input
                v-model="form.chatgpt_account_id"
                placeholder="走 chatgpt.com 时发 chatgpt-account-id 头"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">Organization ID（选填）</Label>
              <Input
                v-model="form.organization_id"
                placeholder="API Key 模式发 OpenAI-Organization 头"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">Base URL（选填,仅 API Key 模式生效）</Label>
              <Input
                v-model="form.base_url"
                placeholder="https://api.openai.com (默认)"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
            <div
              v-if="editing && editing.extra && (
                (editing.extra as any).plan_type
                  || (editing.extra as any).privacy_mode
                  || (editing.extra as any).subscription_expires_at
              )"
              class="rounded-lg bg-[#f9f6f1] px-3 py-2 text-xs text-[#8c8475] space-y-0.5"
            >
              <div v-if="(editing.extra as any).plan_type">
                订阅: <span class="text-[#29261e] font-medium">{{ (editing.extra as any).plan_type }}</span>
                <span v-if="(editing.extra as any).subscription_expires_at" class="ml-2">
                  到期: {{ (editing.extra as any).subscription_expires_at }}
                </span>
              </div>
              <div v-if="(editing.extra as any).privacy_mode">
                隐私: <span class="text-[#29261e]">{{ (editing.extra as any).privacy_mode }}</span>
              </div>
            </div>
          </template>

          <!-- Claude 账号专用 (Billing / 订阅类型 / Account/Org UUID / 自动遥测) -->
          <template v-else>
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">Billing 模式</Label>
            <div class="flex gap-2">
              <button
                type="button"
                @click="form.billing_mode = 'strip'"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="form.billing_mode === 'strip'
                  ? 'bg-[#c4704f]/10 border-[#c4704f] text-[#c4704f]'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-[#c4704f]/40'"
              >
                清除 (Strip)
              </button>
              <button
                type="button"
                @click="form.billing_mode = 'rewrite'"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="form.billing_mode === 'rewrite'
                  ? 'bg-amber-50 border-amber-400 text-amber-600'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
              >
                重写 (Rewrite)
              </button>
            </div>
          </div>
          <!-- 遥测身份（选填） -->
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">订阅类型（选填，强烈推荐）</Label>
            <div class="flex gap-2 flex-wrap">
              <button
                v-for="opt in [
                  { value: '', label: '未设置' },
                  { value: 'max', label: 'Max' },
                  { value: 'pro', label: 'Pro' },
                  { value: 'team', label: 'Team' },
                  { value: 'enterprise', label: 'Enterprise' },
                ]"
                :key="opt.value"
                type="button"
                @click="form.subscription_type = opt.value"
                class="px-3 py-1.5 rounded-lg text-xs font-medium border transition-all duration-200"
                :class="form.subscription_type === opt.value
                  ? 'bg-[#c4704f]/10 border-[#c4704f] text-[#c4704f]'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-[#c4704f]/40'"
              >
                {{ opt.label }}
              </button>
            </div>
          </div>
          <div class="flex gap-4">
            <div class="flex-1 space-y-2">
              <Label class="text-[#5c5647] text-sm">Account UUID（选填）</Label>
              <Input
                v-model="form.account_uuid"
                placeholder="OAuth account UUID"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
            <div class="flex-1 space-y-2">
              <Label class="text-[#5c5647] text-sm">Organization UUID（选填）</Label>
              <Input
                v-model="form.organization_uuid"
                placeholder="OAuth organization UUID"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
          </div>
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">自动遥测</Label>
            <div class="flex gap-2">
              <button
                type="button"
                @click="form.auto_telemetry = false"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="!form.auto_telemetry
                  ? 'bg-[#f9f6f1] border-[#8c8475] text-[#5c5647]'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-[#8c8475]/40'"
              >
                关闭
              </button>
              <button
                type="button"
                @click="form.auto_telemetry = true"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="form.auto_telemetry
                  ? 'bg-emerald-50 border-emerald-400 text-emerald-600'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-emerald-300'"
              >
                开启
              </button>
            </div>
            <p class="text-xs text-[#b5b0a6]">开启后由网关代替客户端发送遥测请求</p>
          </div>
          <div class="space-y-2">
            <Label class="text-[#5c5647] text-sm">
              显示思考内容
              <span class="ml-2 inline-block px-1.5 py-0.5 text-[10px] rounded border border-amber-400 bg-amber-50 text-amber-700 align-middle">实验性</span>
            </Label>
            <div class="flex gap-2">
              <button
                type="button"
                @click="form.experimental_reveal_thinking = false"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="!form.experimental_reveal_thinking
                  ? 'bg-[#f9f6f1] border-[#8c8475] text-[#5c5647]'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-[#8c8475]/40'"
              >
                关闭
              </button>
              <button
                type="button"
                @click="form.experimental_reveal_thinking = true"
                class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                :class="form.experimental_reveal_thinking
                  ? 'bg-amber-50 border-amber-400 text-amber-700'
                  : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
              >
                开启
              </button>
            </div>
            <p class="text-xs text-[#b5b0a6]">剥离 redact-thinking beta token，让模型思考正文回流到 Claude Code 终端。Anthropic 可能反指纹检测，建议仅在测试号开启，每个账号独立控制。</p>
          </div>
          </template>
          <div class="flex gap-4">
            <div class="flex-1 space-y-2">
              <Label class="text-[#5c5647] text-sm">并发数</Label>
              <Input
                v-model.number="form.concurrency"
                type="number"
                min="1"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] focus:border-[#c4704f] focus:ring-[#c4704f]/20"
              />
            </div>
            <div class="flex-1 space-y-2">
              <Label class="text-[#5c5647] text-sm">优先级</Label>
              <Input
                v-model.number="form.priority"
                type="number"
                min="1"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] focus:border-[#c4704f] focus:ring-[#c4704f]/20"
              />
            </div>
          </div>

          <DialogFooter class="gap-2 pt-2">
            <Button
              type="button"
              variant="ghost"
              @click="showForm = false"
              class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]"
            >
              取消
            </Button>
            <Button
              type="submit"
              class="bg-[#c4704f] hover:bg-[#b5623f] text-white font-medium rounded-xl transition-all duration-200"
            >
              保存
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>

    <!-- 删除确认弹窗 -->
    <Dialog v-model:open="showDeleteConfirm">
      <DialogContent class="bg-white border-[#e8e2d9] rounded-2xl text-[#29261e] sm:max-w-sm">
        <DialogHeader>
          <DialogTitle class="text-[#29261e]">确认删除</DialogTitle>
          <DialogDescription class="text-[#8c8475]">
            此操作不可撤销，确认要删除此账号吗？
          </DialogDescription>
        </DialogHeader>
        <DialogFooter class="gap-2 pt-4">
          <Button
            variant="ghost"
            @click="showDeleteConfirm = false"
            class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]"
          >
            取消
          </Button>
          <Button
            @click="executeDelete"
            class="bg-red-500 hover:bg-red-600 text-white font-medium rounded-xl transition-all duration-200"
          >
            删除
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>

    <!-- OAuth 授权流程弹窗 -->
    <Dialog v-model:open="showOAuthFlow">
      <DialogContent class="bg-white border-[#e8e2d9] rounded-2xl text-[#29261e] sm:max-w-lg max-h-[85vh] flex flex-col">
        <DialogHeader class="flex-shrink-0">
          <DialogTitle class="text-[#29261e] text-lg">OAuth 授权</DialogTitle>
          <DialogDescription class="text-[#8c8475]">
            通过浏览器完成 OAuth 授权，自动获取 Token 和账号信息
          </DialogDescription>
        </DialogHeader>

        <div class="space-y-4 mt-2 overflow-y-auto flex-1 pr-1">
          <!-- 步骤 1：选择模式并生成链接 -->
          <template v-if="oauthStep === 'generate'">
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">授权类型</Label>
              <div class="flex gap-2">
                <button
                  type="button"
                  @click="oauthMode = 'oauth'"
                  class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                  :class="oauthMode === 'oauth'
                    ? 'bg-amber-50 border-amber-400 text-amber-600'
                    : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
                >
                  OAuth（完整）
                </button>
                <button
                  type="button"
                  @click="oauthMode = 'setup_token'"
                  class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                  :class="oauthMode === 'setup_token'
                    ? 'bg-[#c4704f]/10 border-[#c4704f] text-[#c4704f]'
                    : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-[#c4704f]/40'"
                >
                  Setup Token
                </button>
              </div>
              <p class="text-xs text-[#b5b0a6]">
                {{ oauthMode === 'oauth' ? '完整 scope，支持 profile、用量查询等' : '仅 user:inference scope，有效期 1 年' }}
              </p>
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">代理地址（选填）</Label>
              <Input
                v-model="oauthProxyUrl"
                placeholder="http:// 或 socks5://"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20"
              />
            </div>
            <Button
              @click="generateOAuthUrl"
              :disabled="oauthLoading"
              class="w-full bg-amber-500 hover:bg-amber-600 text-white font-medium rounded-xl transition-all duration-200"
            >
              {{ oauthLoading ? '生成中...' : '生成授权链接' }}
            </Button>
          </template>

          <!-- 步骤 2：显示链接 + 输入 code -->
          <template v-if="oauthStep === 'exchange'">
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">授权链接</Label>
              <div class="relative">
                <Textarea
                  :model-value="oauthAuthUrl"
                  readonly
                  :rows="3"
                  class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] font-mono text-xs pr-16"
                />
                <div class="absolute right-2 top-2 flex gap-1">
                  <button
                    type="button"
                    @click="copyText(oauthAuthUrl)"
                    class="px-2 py-1 text-xs bg-[#c4704f] text-white rounded-md hover:bg-[#b5623f] transition-colors"
                  >
                    复制
                  </button>
                </div>
              </div>
              <a
                :href="oauthAuthUrl"
                target="_blank"
                rel="noopener noreferrer"
                class="inline-flex items-center gap-1 text-xs text-amber-600 hover:text-amber-700 underline"
              >
                点击打开授权页面 ↗
              </a>
            </div>
            <div class="rounded-lg bg-amber-50 border border-amber-200 px-3 py-2 text-xs text-amber-700 space-y-1">
              <p class="font-medium">操作步骤：</p>
              <ol class="list-decimal list-inside space-y-0.5 text-amber-600">
                <li>点击上方链接或复制到浏览器打开</li>
                <li>完成 Claude 登录授权</li>
                <li>授权完成后，从回调页面复制授权码</li>
                <li>将授权码粘贴到下方输入框</li>
              </ol>
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">授权码 <span class="text-red-500">*</span></Label>
              <Textarea
                v-model="oauthCode"
                :rows="2"
                placeholder="粘贴授权码（authorization code）"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] placeholder-[#b5b0a6] focus:border-[#c4704f] focus:ring-[#c4704f]/20 font-mono text-sm"
              />
            </div>
            <div class="flex gap-2">
              <Button
                variant="ghost"
                @click="oauthStep = 'generate'"
                class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]"
              >
                返回
              </Button>
              <Button
                @click="exchangeOAuthCode"
                :disabled="oauthLoading || !oauthCode.trim()"
                class="flex-1 bg-amber-500 hover:bg-amber-600 text-white font-medium rounded-xl transition-all duration-200"
              >
                {{ oauthLoading ? '交换中...' : '交换 Token' }}
              </Button>
            </div>
          </template>

          <!-- 步骤 3：显示结果 -->
          <template v-if="oauthStep === 'done' && oauthResult">
            <div class="rounded-lg bg-emerald-50 border border-emerald-200 px-3 py-2 text-sm text-emerald-700 font-medium">
              授权成功
            </div>
            <div class="space-y-3">
              <div v-if="oauthResult.email_address" class="space-y-1">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">邮箱</p>
                <p class="text-sm text-[#29261e]">{{ oauthResult.email_address }}</p>
              </div>
              <div v-if="oauthResult.account_uuid" class="space-y-1">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">Account UUID</p>
                <p class="font-mono text-xs text-[#5c5647] break-all">{{ oauthResult.account_uuid }}</p>
              </div>
              <div v-if="oauthResult.organization_uuid" class="space-y-1">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">Organization UUID</p>
                <p class="font-mono text-xs text-[#5c5647] break-all">{{ oauthResult.organization_uuid }}</p>
              </div>
              <div class="space-y-1">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">Access Token</p>
                <div class="flex items-center gap-2">
                  <p class="font-mono text-xs text-[#8c8475] truncate flex-1">{{ oauthResult.access_token.slice(0, 30) }}...</p>
                  <button
                    type="button"
                    @click="copyText(oauthResult.access_token)"
                    class="px-2 py-0.5 text-[10px] bg-[#f0ebe4] text-[#5c5647] rounded hover:bg-[#e8e2d9] transition-colors flex-shrink-0"
                  >
                    复制
                  </button>
                </div>
              </div>
              <div v-if="oauthResult.refresh_token" class="space-y-1">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">Refresh Token</p>
                <div class="flex items-center gap-2">
                  <p class="font-mono text-xs text-[#8c8475] truncate flex-1">{{ oauthResult.refresh_token.slice(0, 30) }}...</p>
                  <button
                    type="button"
                    @click="copyText(oauthResult.refresh_token)"
                    class="px-2 py-0.5 text-[10px] bg-[#f0ebe4] text-[#5c5647] rounded hover:bg-[#e8e2d9] transition-colors flex-shrink-0"
                  >
                    复制
                  </button>
                </div>
              </div>
              <div class="space-y-1">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">Scope</p>
                <p class="text-xs text-[#8c8475]">{{ oauthResult.scope || '—' }}</p>
              </div>
              <div class="space-y-1">
                <p class="text-[10px] text-[#b5b0a6] uppercase tracking-wider">过期时间</p>
                <p class="text-xs text-[#8c8475]">{{ new Date(oauthResult.expires_at * 1000).toLocaleString('zh-CN') }}</p>
              </div>
            </div>
            <div class="flex gap-2 pt-2">
              <Button
                variant="ghost"
                @click="showOAuthFlow = false"
                class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]"
              >
                关闭
              </Button>
              <Button
                @click="applyOAuthResult"
                class="flex-1 bg-[#c4704f] hover:bg-[#b5623f] text-white font-medium rounded-xl transition-all duration-200"
              >
                填入并创建账号
              </Button>
            </div>
          </template>
        </div>
      </DialogContent>
    </Dialog>

    <!-- SessionKey 一键导入弹窗 -->
    <Dialog v-model:open="showSkImport">
      <DialogContent class="bg-white border-[#e8e2d9] rounded-2xl text-[#29261e] sm:max-w-2xl max-h-[85vh] flex flex-col">
        <DialogHeader class="flex-shrink-0">
          <DialogTitle class="text-[#29261e] text-lg">SessionKey 一键导入</DialogTitle>
          <DialogDescription class="text-[#8c8475]">
            粘贴 claude.ai 的 sessionKey (sk-ant-sid02-...), 自动跑 OAuth 流程并入库账号。支持单个或多个 (一行一个)。
          </DialogDescription>
        </DialogHeader>

        <div class="space-y-4 mt-2 overflow-y-auto flex-1 pr-1">
          <!-- 表单步骤 -->
          <template v-if="skImportStep === 'form'">
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">sessionKey 列表 <span class="text-red-500">*</span></Label>
              <Textarea
                v-model="skImportText"
                rows="6"
                placeholder="sk-ant-sid02-xxxxxxxx&#10;sk-ant-sid02-yyyyyyyy&#10;sk-ant-sid02-zzzzzzzz"
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e] font-mono text-xs"
              />
              <p class="text-xs text-[#8c8475]">每行一个 sessionKey, 自动去重</p>
            </div>

            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">代理 (海外代理, platform.claude.com 必须走代理)</Label>
              <Input
                v-model="skImportProxyUrl"
                placeholder="http://user:pass@host:port 或 socks5://..."
                class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e]"
              />
            </div>

            <div class="grid grid-cols-2 gap-3">
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">授权类型</Label>
                <div class="flex gap-2">
                  <button
                    type="button"
                    @click="skImportScope = 'full'"
                    class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                    :class="skImportScope === 'full' ? 'bg-amber-50 border-amber-400 text-amber-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
                  >
                    Full OAuth
                  </button>
                  <button
                    type="button"
                    @click="skImportScope = 'inference'"
                    class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                    :class="skImportScope === 'inference' ? 'bg-amber-50 border-amber-400 text-amber-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
                  >
                    Setup Token
                  </button>
                </div>
              </div>

              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">并发上限</Label>
                <Input
                  v-model.number="skImportConcurrency"
                  type="number"
                  min="1"
                  max="30"
                  class="bg-[#f9f6f1] border-[#e8e2d9] text-[#29261e]"
                />
                <p class="text-xs text-[#8c8475]">推荐 3-5, 太高会触发风控</p>
              </div>
            </div>

            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">Billing 模式</Label>
              <div class="flex gap-2">
                <button
                  type="button"
                  @click="skImportBillingMode = 'strip'"
                  class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                  :class="skImportBillingMode === 'strip' ? 'bg-amber-50 border-amber-400 text-amber-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
                >
                  清除 (Strip)
                </button>
                <button
                  type="button"
                  @click="skImportBillingMode = 'rewrite'"
                  class="flex-1 px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
                  :class="skImportBillingMode === 'rewrite' ? 'bg-amber-50 border-amber-400 text-amber-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
                >
                  重写 (Rewrite)
                </button>
              </div>
            </div>

            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">订阅类型 (可选)</Label>
              <div class="flex gap-2 flex-wrap">
                <button
                  v-for="opt in [{value:'',label:'未设置'},{value:'max',label:'Max'},{value:'pro',label:'Pro'},{value:'team',label:'Team'},{value:'enterprise',label:'Enterprise'}]"
                  :key="opt.value"
                  type="button"
                  @click="skImportSubscription = opt.value"
                  class="px-3 py-1.5 rounded-lg text-sm font-medium border transition-all duration-200"
                  :class="skImportSubscription === opt.value ? 'bg-amber-50 border-amber-400 text-amber-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-amber-300'"
                >
                  {{ opt.label }}
                </button>
              </div>
            </div>

            <div class="flex items-center gap-2">
              <input
                id="sk-auto-tel"
                type="checkbox"
                v-model="skImportAutoTelemetry"
                class="rounded border-[#e8e2d9]"
              />
              <Label for="sk-auto-tel" class="text-[#5c5647] text-sm cursor-pointer">
                启用自动遥测 (auto_telemetry)
              </Label>
            </div>
          </template>

          <!-- 结果步骤 -->
          <template v-else-if="skImportStep === 'result' && skImportResults">
            <div class="grid grid-cols-3 gap-3">
              <div class="bg-blue-50 border border-blue-200 rounded-xl p-3">
                <p class="text-xs text-[#8c8475]">总数</p>
                <p class="text-2xl font-semibold text-blue-700">{{ skImportResults.total }}</p>
              </div>
              <div class="bg-green-50 border border-green-200 rounded-xl p-3">
                <p class="text-xs text-[#8c8475]">成功</p>
                <p class="text-2xl font-semibold text-green-700">{{ skImportResults.success }}</p>
              </div>
              <div class="bg-red-50 border border-red-200 rounded-xl p-3">
                <p class="text-xs text-[#8c8475]">失败</p>
                <p class="text-2xl font-semibold text-red-700">{{ skImportResults.failed }}</p>
              </div>
            </div>

            <div class="space-y-2 max-h-96 overflow-y-auto">
              <div
                v-for="(r, idx) in skImportResults.results"
                :key="idx"
                class="border rounded-lg p-3 text-xs"
                :class="r.success ? 'bg-green-50 border-green-200' : 'bg-red-50 border-red-200'"
              >
                <div class="flex items-center gap-2 mb-1">
                  <Badge :class="r.success ? 'bg-green-600 text-white' : 'bg-red-600 text-white'">
                    {{ r.success ? 'OK' : 'FAIL' }}
                  </Badge>
                  <span class="font-mono text-[#5c5647]">{{ r.session_key_preview }}</span>
                </div>
                <div v-if="r.success" class="text-[#5c5647]">
                  ID: <span class="font-mono">{{ r.account_id }}</span>
                  &nbsp;|&nbsp; Email: <span class="font-mono">{{ r.email }}</span>
                </div>
                <div v-else class="text-red-700 break-all">
                  {{ r.error }}
                </div>
              </div>
            </div>
          </template>
        </div>

        <DialogFooter class="gap-2 pt-2 flex-shrink-0">
          <template v-if="skImportStep === 'form'">
            <Button
              variant="ghost"
              @click="closeSessionKeyImport"
              :disabled="skImportLoading"
              class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]"
            >
              取消
            </Button>
            <Button
              @click="runSessionKeyImport"
              :disabled="skImportLoading"
              class="bg-[#5b8a72] hover:bg-[#4a7a62] text-white font-medium rounded-xl transition-all duration-200"
            >
              {{ skImportLoading ? '导入中…' : '开始导入' }}
            </Button>
          </template>
          <template v-else>
            <Button
              v-if="skImportResults && skImportResults.failed > 0"
              @click="retryFailedImports"
              :disabled="skImportLoading"
              class="bg-amber-500 hover:bg-amber-600 text-white font-medium rounded-xl"
            >
              {{ skImportLoading ? '重试中…' : `重试失败行 (${skImportResults.failed})` }}
            </Button>
            <Button
              variant="ghost"
              @click="skImportStep = 'form'"
              :disabled="skImportLoading"
              class="text-[#8c8475] hover:text-[#29261e] hover:bg-[#f0ebe4]"
            >
              返回
            </Button>
            <Button
              @click="closeSessionKeyImport"
              :disabled="skImportLoading"
              class="bg-[#c4704f] hover:bg-[#b5623f] text-white font-medium rounded-xl"
            >
              关闭
            </Button>
          </template>
        </DialogFooter>
      </DialogContent>
    </Dialog>

    <!-- OpenAI 账号导入弹窗 (Phase 7) -->
    <Dialog v-model:open="showOpenAIImport">
      <DialogContent class="bg-white border-[#e8e2d9] rounded-2xl text-[#29261e] sm:max-w-2xl max-h-[85vh] flex flex-col">
        <DialogHeader class="flex-shrink-0">
          <DialogTitle class="text-[#29261e] text-lg">OpenAI 账号导入</DialogTitle>
          <DialogDescription class="text-[#8c8475]">
            4 种授权: Refresh Token 批量 / API Key / Codex Token / ChatGPT OAuth 浏览器流程
          </DialogDescription>
        </DialogHeader>

        <div class="space-y-4 mt-2 overflow-y-auto flex-1 pr-1">
          <!-- 模式切换 -->
          <div class="grid grid-cols-4 gap-2">
            <button
              type="button"
              @click="oaMode = 'rt'"
              class="px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
              :class="oaMode === 'rt' ? 'bg-blue-50 border-blue-400 text-blue-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-blue-300'"
            >
              Refresh Token
            </button>
            <button
              type="button"
              @click="oaMode = 'api_key'"
              class="px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
              :class="oaMode === 'api_key' ? 'bg-blue-50 border-blue-400 text-blue-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-blue-300'"
            >
              API Key
            </button>
            <button
              type="button"
              @click="oaMode = 'codex_token'"
              class="px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
              :class="oaMode === 'codex_token' ? 'bg-blue-50 border-blue-400 text-blue-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-blue-300'"
            >
              Codex Token
            </button>
            <button
              type="button"
              @click="oaMode = 'oauth'"
              class="px-3 py-2 rounded-lg text-sm font-medium border transition-all duration-200"
              :class="oaMode === 'oauth' ? 'bg-blue-50 border-blue-400 text-blue-600' : 'bg-[#f9f6f1] border-[#e8e2d9] text-[#8c8475] hover:border-blue-300'"
            >
              手动授权
            </button>
          </div>

          <!-- Refresh Token 批量导入 -->
          <template v-if="oaMode === 'rt'">
            <template v-if="oaRtStep === 'form'">
              <p class="text-sm text-[#5c5647]">
                输入您已有的 OpenAI Refresh Token, 系统将自动验证并创建账号。支持批量(每行一个)。
              </p>
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">Refresh Token 列表 <span class="text-red-500">*</span></Label>
                <Textarea v-model="oaRtText" rows="6" placeholder="粘贴您的 OpenAI Refresh Token...&#10;支持多个, 每行一个"
                          class="bg-[#f9f6f1] border-[#e8e2d9] font-mono text-xs" />
              </div>
              <div class="grid grid-cols-2 gap-3">
                <div class="space-y-2">
                  <Label class="text-[#5c5647] text-sm">代理 (海外 OpenAI 必走)</Label>
                  <Input v-model="oaForm.proxy_url" placeholder="http://user:pass@host:port"
                         class="bg-[#f9f6f1] border-[#e8e2d9]" />
                </div>
                <div class="space-y-2">
                  <Label class="text-[#5c5647] text-sm">并发上限</Label>
                  <Input v-model.number="oaRtConcurrency" type="number" min="1" max="10"
                         class="bg-[#f9f6f1] border-[#e8e2d9]" />
                </div>
              </div>
              <div class="grid grid-cols-2 gap-3">
                <div class="space-y-2">
                  <Label class="text-[#5c5647] text-sm">User-Agent (可选)</Label>
                  <Input v-model="oaForm.user_agent" placeholder="codex_cli_rs/0.104.0"
                         class="bg-[#f9f6f1] border-[#e8e2d9]" />
                </div>
                <div class="space-y-2">
                  <Label class="text-[#5c5647] text-sm">Base URL (可选)</Label>
                  <Input v-model="oaForm.base_url" placeholder="https://api.openai.com"
                         class="bg-[#f9f6f1] border-[#e8e2d9]" />
                </div>
              </div>
            </template>

            <template v-else-if="oaRtResults">
              <div class="grid grid-cols-3 gap-3">
                <div class="bg-blue-50 border border-blue-200 rounded-xl p-3">
                  <p class="text-xs text-[#8c8475]">总数</p>
                  <p class="text-2xl font-semibold text-blue-700">{{ oaRtResults.total }}</p>
                </div>
                <div class="bg-green-50 border border-green-200 rounded-xl p-3">
                  <p class="text-xs text-[#8c8475]">成功</p>
                  <p class="text-2xl font-semibold text-green-700">{{ oaRtResults.success }}</p>
                </div>
                <div class="bg-red-50 border border-red-200 rounded-xl p-3">
                  <p class="text-xs text-[#8c8475]">失败</p>
                  <p class="text-2xl font-semibold text-red-700">{{ oaRtResults.failed }}</p>
                </div>
              </div>
              <div class="space-y-2 max-h-72 overflow-y-auto">
                <div v-for="(r, idx) in oaRtResults.results" :key="idx"
                     class="border rounded-lg p-3 text-xs"
                     :class="r.success ? 'bg-green-50 border-green-200' : 'bg-red-50 border-red-200'">
                  <div class="flex items-center gap-2 mb-1">
                    <Badge :class="r.success ? 'bg-green-600 text-white' : 'bg-red-600 text-white'">
                      {{ r.success ? 'OK' : 'FAIL' }}
                    </Badge>
                    <span class="font-mono">{{ r.rt_preview }}</span>
                  </div>
                  <div v-if="r.success" class="text-[#5c5647]">
                    ID: <span class="font-mono">{{ r.account_id }}</span> | Email: {{ r.email }}
                  </div>
                  <div v-else class="text-red-700 break-all">{{ r.error }}</div>
                </div>
              </div>
            </template>
          </template>

          <!-- API Key / Codex Token 共用表单 -->
          <template v-else-if="oaMode === 'api_key' || oaMode === 'codex_token'">
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">Email <span class="text-red-500">*</span></Label>
              <Input v-model="oaForm.email" placeholder="my-openai@example.com" class="bg-[#f9f6f1] border-[#e8e2d9]" />
            </div>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">{{ oaMode === 'api_key' ? 'API Key (sk-* / sk-proj-*)' : 'Codex Token' }} <span class="text-red-500">*</span></Label>
              <Input v-model="oaForm.api_key" placeholder="sk-..." type="password" class="bg-[#f9f6f1] border-[#e8e2d9] font-mono text-xs" />
            </div>
            <div class="grid grid-cols-2 gap-3">
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">Organization ID (可选)</Label>
                <Input v-model="oaForm.organization_id" placeholder="org-xxxx" class="bg-[#f9f6f1] border-[#e8e2d9]" />
              </div>
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">Base URL (可选)</Label>
                <Input v-model="oaForm.base_url" placeholder="https://api.openai.com" class="bg-[#f9f6f1] border-[#e8e2d9]" />
              </div>
            </div>
            <div class="grid grid-cols-2 gap-3">
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">User-Agent (可选)</Label>
                <Input v-model="oaForm.user_agent" placeholder="codex_cli_rs/0.104.0" class="bg-[#f9f6f1] border-[#e8e2d9]" />
              </div>
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">代理 (可选)</Label>
                <Input v-model="oaForm.proxy_url" placeholder="http://user:pass@host:port" class="bg-[#f9f6f1] border-[#e8e2d9]" />
              </div>
            </div>
          </template>

          <!-- ChatGPT OAuth 浏览器流程 -->
          <template v-else>
            <div class="space-y-2">
              <Label class="text-[#5c5647] text-sm">代理 (可选, OpenAI auth.openai.com 国内通常需代理)</Label>
              <Input v-model="oaForm.proxy_url" placeholder="http://user:pass@host:port" class="bg-[#f9f6f1] border-[#e8e2d9]" />
            </div>

            <template v-if="oaOAuthStep === 'generate'">
              <p class="text-sm text-[#5c5647]">点【生成授权链接】将打开浏览器登录 chatgpt.com, 完成后从地址栏复制 code 回粘到下一步。</p>
            </template>

            <template v-else-if="oaOAuthStep === 'exchange'">
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">授权链接 (已自动复制 + 打开浏览器)</Label>
                <Input v-model="oaAuthUrl" readonly class="bg-[#f9f6f1] border-[#e8e2d9] text-xs" />
              </div>
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">授权码 code <span class="text-red-500">*</span></Label>
                <Input v-model="oaCode" placeholder="从浏览器回调地址中复制 code 参数" class="bg-[#f9f6f1] border-[#e8e2d9] font-mono text-xs" />
              </div>
            </template>

            <template v-else-if="oaOAuthStep === 'done' && oaResult">
              <div class="bg-green-50 border border-green-200 rounded-xl p-3 space-y-1 text-sm">
                <p><strong>Email:</strong> {{ oaResult.email }}</p>
                <p><strong>Plan:</strong> {{ oaResult.plan_type || '(空)' }}</p>
                <p><strong>Org ID:</strong> <span class="font-mono text-xs">{{ oaResult.organization_id || '(空)' }}</span></p>
                <p><strong>ChatGPT Account ID:</strong> <span class="font-mono text-xs">{{ oaResult.chatgpt_account_id || '(空)' }}</span></p>
                <p><strong>Expires in:</strong> {{ oaResult.expires_in }}s</p>
              </div>
              <div class="space-y-2">
                <Label class="text-[#5c5647] text-sm">Email (用于账号唯一标识)</Label>
                <Input v-model="oaForm.email" placeholder="my-openai@example.com" class="bg-[#f9f6f1] border-[#e8e2d9]" />
              </div>
            </template>
          </template>
        </div>

        <DialogFooter class="gap-2 pt-2 flex-shrink-0">
          <template v-if="oaMode === 'rt'">
            <template v-if="oaRtStep === 'form'">
              <Button variant="ghost" @click="showOpenAIImport = false" :disabled="oaLoading"
                      class="text-[#8c8475] hover:bg-[#f0ebe4]">取消</Button>
              <Button @click="submitOpenAIRtImport" :disabled="oaLoading"
                      class="bg-[#3a6ea5] hover:bg-[#2f5d8e] text-white font-medium rounded-xl">
                {{ oaLoading ? '验证中…' : '验证并创建账号' }}
              </Button>
            </template>
            <template v-else>
              <Button variant="ghost" @click="oaRtStep = 'form'" :disabled="oaLoading"
                      class="text-[#8c8475] hover:bg-[#f0ebe4]">返回</Button>
              <Button @click="showOpenAIImport = false" :disabled="oaLoading"
                      class="bg-[#3a6ea5] hover:bg-[#2f5d8e] text-white font-medium rounded-xl">
                关闭
              </Button>
            </template>
          </template>
          <template v-else-if="oaMode === 'api_key' || oaMode === 'codex_token'">
            <Button variant="ghost" @click="showOpenAIImport = false" :disabled="oaLoading"
                    class="text-[#8c8475] hover:bg-[#f0ebe4]">取消</Button>
            <Button @click="submitOpenAIToken" :disabled="oaLoading"
                    class="bg-[#3a6ea5] hover:bg-[#2f5d8e] text-white font-medium rounded-xl">
              {{ oaLoading ? '保存中…' : '保存账号' }}
            </Button>
          </template>
          <template v-else>
            <Button variant="ghost" @click="showOpenAIImport = false" :disabled="oaLoading"
                    class="text-[#8c8475] hover:bg-[#f0ebe4]">取消</Button>
            <Button v-if="oaOAuthStep === 'generate'" @click="generateOpenAIAuthUrl" :disabled="oaLoading"
                    class="bg-[#3a6ea5] hover:bg-[#2f5d8e] text-white font-medium rounded-xl">
              {{ oaLoading ? '生成中…' : '生成授权链接' }}
            </Button>
            <Button v-else-if="oaOAuthStep === 'exchange'" @click="exchangeOpenAICode" :disabled="oaLoading"
                    class="bg-[#3a6ea5] hover:bg-[#2f5d8e] text-white font-medium rounded-xl">
              {{ oaLoading ? '交换中…' : '交换 Token' }}
            </Button>
            <Button v-else @click="applyOpenAIOAuthAccount" :disabled="oaLoading"
                    class="bg-[#3a6ea5] hover:bg-[#2f5d8e] text-white font-medium rounded-xl">
              {{ oaLoading ? '保存中…' : '保存账号' }}
            </Button>
          </template>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  </div>
</template>
