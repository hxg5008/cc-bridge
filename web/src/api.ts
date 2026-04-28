const BASE = ''

let authToken = ''

export function setAuth(token: string) {
  authToken = token
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(BASE + path, {
    method,
    headers: {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${authToken}`,
    },
    body: body ? JSON.stringify(body) : undefined,
  })
  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: res.statusText }))
    throw new Error(err.error || res.statusText)
  }
  return res.json()
}

export interface Account {
  id: number
  name: string
  email: string
  status: string
  auth_type: string
  setup_token: string
  access_token: string
  refresh_token: string
  expires_at?: number | null
  oauth_refreshed_at?: string
  auth_error?: string
  proxy_url: string
  device_id: string
  canonical_env?: Record<string, unknown>
  canonical_prompt_env?: Record<string, unknown>
  canonical_process?: {
    constrained_memory?: number
    rss_range?: number[]
    heap_total_range?: number[]
    heap_used_range?: number[]
  }
  billing_mode: string
  platform?: string
  extra?: Record<string, unknown>
  account_uuid?: string | null
  organization_uuid?: string | null
  subscription_type?: string | null
  concurrency: number
  current_concurrency?: number
  priority: number
  auto_telemetry: boolean
  telemetry_count: number
  telemetry_expires_at?: string
  experimental_reveal_thinking?: boolean
  rate_limited_at?: string
  rate_limit_reset_at?: string
  /** 内存里仍有效的短期限流截止时间 (RFC3339); 只在 dashboard 倒计时显示用,不持久化。
   *  当不为 null 时 → 该账号目前被 LimitStore 软挡, 调度器不会派发新请求。 */
  rate_limited_until_runtime?: string
  disable_reason?: string
  usage_data?: UsageData
  usage_fetched_at?: string
  /** 5 类用户视角分类（后端 categorize 返回） */
  category?: AccountCategory
  /** 该状态的子原因 (限流原因 / 停用原因 / 失效错误信息), 给 UI 显示用 */
  category_reason?: string
  /** 限流类账号预计恢复时刻 RFC3339 */
  category_recovers_at?: string
  created_at: string
  updated_at: string
}

export interface PagedResult<T> {
  data: T[]
  total: number
  page: number
  page_size: number
  total_pages: number
}

export interface UsageWindow {
  utilization: number
  resets_at: string
  /** 每窗口独立状态：allowed / allowed_warning / rejected。响应头源才有，/api/oauth/usage 缺失。 */
  status?: string
  /** 上游在撞墙前发出的阈值（0-1），通常 0.8 / 0.9 / 0.97。 */
  surpassed_threshold?: number
}

export interface UsageData {
  five_hour?: UsageWindow
  seven_day?: UsageWindow
  seven_day_sonnet?: UsageWindow
  /** 数据来源：'headers'（响应头吸取）/ undefined（/api/oauth/usage 旧数据）。 */
  source?: string
  /** 全局状态（所有窗口中最紧张的）。 */
  status?: string
  /** 上游标记的瓶颈窗口：'five_hour' / 'seven_day' / 'seven_day_opus'。 */
  representative_claim?: string
  /** 全局 -reset 头，瓶颈窗口的重置时刻。 */
  resets_at?: string
  /** 回退配额百分比。 */
  fallback_percentage?: number
  /** Overage（超量付费）状态：allowed / allowed_warning / rejected。 */
  overage_status?: string
  /** Overage 被禁用的原因（如 org_level_disabled）。 */
  overage_disabled_reason?: string
}

export interface ApiToken {
  id: number;
  name: string;
  token: string;
  allowed_accounts: string;
  blocked_accounts: string;
  status: string;
  created_at: string;
  updated_at: string;
}

export interface Dashboard {
  accounts: {
    total: number;
    /** 旧字段（DB.status 计数）— 兼容老前端 */
    active: number;
    error: number;
    disabled: number;
    by_platform?: Record<string, number>;
    /** 新分类（5 类用户视角） */
    available?: number;
    rate_limited?: number;
    invalid?: number;
    banned?: number;
    stopped?: number;
    /** 可调度比例: available / total (0.0 ~ 1.0) */
    schedulable_pct?: number;
  };
  tokens: number;
}

/** 与后端 AccountCategory 对应（snake_case） */
export type AccountCategory =
  | 'available'
  | 'rate_limited'
  | 'invalid'
  | 'banned'
  | 'stopped';

export interface OAuthGenerateResult {
  auth_url: string;
  session_id: string;
}

export interface OAuthExchangeResult {
  access_token: string;
  refresh_token: string;
  expires_in: number;
  expires_at: number;
  scope: string;
  account_uuid: string;
  organization_uuid: string;
  email_address: string;
}

// SessionKey 自动 OAuth 相关
export interface CookieAuthBatchResultItem {
  session_key_preview: string;
  success: boolean;
  account_id?: number;
  email?: string;
  error?: string;
}

export interface CookieAuthBatchResponse {
  total: number;
  success: number;
  failed: number;
  results: CookieAuthBatchResultItem[];
}

export const api = {
  listAccounts: (page = 1, pageSize = 12) =>
    request<PagedResult<Account>>('GET', `/admin/accounts?page=${page}&page_size=${pageSize}`),
  createAccount: (a: Partial<Account>) => request<Account>('POST', '/admin/accounts', a),
  updateAccount: (id: number, a: Partial<Account>) => request<Account>('PUT', `/admin/accounts/${id}`, a),
  deleteAccount: (id: number) => request<void>('DELETE', `/admin/accounts/${id}`),
  testAccount: (id: number) => request<{ status: string; message?: string }>('POST', `/admin/accounts/${id}/test`),
  refreshUsage: (id: number) => request<{ status: string; usage?: UsageData; message?: string }>('POST', `/admin/accounts/${id}/usage`),
  /** 批量刷新所有 OAuth 账号用量。后端走每账号 60s 缓存,频繁调不会真打上游。 */
  refreshAllUsage: () => request<{ status: string; ok: number; skipped: number; failed: number; errors: any[] }>(
    'POST', '/admin/accounts/refresh-all-usage'
  ),
  /** 手动清除内存里残留的短期限流标记 (rate_limited_until / status=Rejected)。
   *  典型用法: dashboard 显示账号用量正常但调度器仍持续过滤该账号 (死锁场景)。 */
  clearLimit: (id: number) => request<{ status: string; cleared: boolean }>('POST', `/admin/accounts/${id}/clear_limit`),
  listTokens: (page = 1, pageSize = 20) =>
    request<PagedResult<ApiToken>>('GET', `/admin/tokens?page=${page}&page_size=${pageSize}`),
  createToken: (t: Partial<ApiToken>) => request<ApiToken>('POST', '/admin/tokens', t),
  updateToken: (id: number, t: Partial<ApiToken>) => request<ApiToken>('PUT', `/admin/tokens/${id}`, t),
  deleteToken: (id: number) => request<void>('DELETE', `/admin/tokens/${id}`),
  getDashboard: () => request<Dashboard>('GET', '/admin/dashboard'),

  generateAuthUrl: (proxyUrl?: string) =>
    request<OAuthGenerateResult>('POST', '/admin/oauth/generate-auth-url', { proxy_url: proxyUrl || null }),
  generateSetupTokenUrl: (proxyUrl?: string) =>
    request<OAuthGenerateResult>('POST', '/admin/oauth/generate-setup-token-url', { proxy_url: proxyUrl || null }),
  exchangeCode: (sessionId: string, code: string) =>
    request<OAuthExchangeResult>('POST', '/admin/oauth/exchange-code', { session_id: sessionId, code }),
  exchangeSetupTokenCode: (sessionId: string, code: string) =>
    request<OAuthExchangeResult>('POST', '/admin/oauth/exchange-setup-token-code', { session_id: sessionId, code }),

  // SessionKey-based 自动 OAuth (sub2api 风格)
  cookieAuth: (sessionKey: string, proxyUrl?: string, scope: 'full' | 'inference' = 'full') =>
    request<OAuthExchangeResult>('POST', '/admin/accounts/cookie-auth', {
      session_key: sessionKey,
      proxy_url: proxyUrl || null,
      scope,
    }),
  cookieAuthCreate: (params: {
    session_key: string;
    proxy_url?: string;
    scope?: 'full' | 'inference';
    name?: string;
    priority?: number;
    concurrency?: number;
    billing_mode?: string;
    auto_telemetry?: boolean;
    subscription_type?: string;
  }) => request<Account>('POST', '/admin/accounts/cookie-auth-create', params),
  cookieAuthCreateBatch: (params: {
    session_keys: string[];
    proxy_url?: string;
    scope?: 'full' | 'inference';
    concurrency_limit?: number;
    priority?: number;
    concurrency?: number;
    billing_mode?: string;
    auto_telemetry?: boolean;
    subscription_type?: string;
  }) => request<CookieAuthBatchResponse>('POST', '/admin/accounts/cookie-auth-create/batch', params),

  // OpenAI 账号 (Phase 2)
  createOpenAIAccount: (params: {
    name?: string;
    email: string;
    credential_type?: 'api_key' | 'codex_token' | 'oauth' | 'cookie';
    api_key?: string;
    access_token?: string;
    refresh_token?: string;
    base_url?: string;
    user_agent?: string;
    chatgpt_account_id?: string;
    organization_id?: string;
    proxy_url?: string;
    priority?: number;
    concurrency?: number;
  }) => request<Account>('POST', '/admin/accounts/openai', params),

  // OpenAI OAuth (Phase 4)
  openaiGenerateAuthUrl: (params: { redirect_uri?: string; proxy_url?: string }) =>
    request<{ auth_url: string; session_id: string; state: string }>(
      'POST',
      '/admin/openai-oauth/generate-auth-url',
      params,
    ),
  openaiExchangeCode: (params: { session_id: string; code: string; state?: string }) =>
    request<OpenAITokenInfo>('POST', '/admin/openai-oauth/exchange-code', params),
  openaiRefreshToken: (params: { refresh_token: string; proxy_url?: string }) =>
    request<OpenAITokenInfo>('POST', '/admin/openai-oauth/refresh-token', params),

  // OpenAI Refresh Token 一键导入 (sub2api 风格)
  openaiRtImport: (params: {
    refresh_token: string;
    email?: string;
    proxy_url?: string;
    user_agent?: string;
    base_url?: string;
    priority?: number;
    concurrency?: number;
  }) => request<Account>('POST', '/admin/accounts/openai-rt-import', params),
  openaiRtImportBatch: (params: {
    refresh_tokens: string[];
    proxy_url?: string;
    user_agent?: string;
    base_url?: string;
    concurrency_limit?: number;
    priority?: number;
    concurrency?: number;
  }) => request<OpenAIRtBatchResponse>('POST', '/admin/accounts/openai-rt-import/batch', params),
}

export interface OpenAIRtBatchItem {
  rt_preview: string;
  success: boolean;
  account_id?: number;
  email?: string;
  error?: string;
}

export interface OpenAIRtBatchResponse {
  total: number;
  success: number;
  failed: number;
  results: OpenAIRtBatchItem[];
}

export interface OpenAITokenInfo {
  access_token: string;
  refresh_token: string;
  id_token: string;
  expires_in: number;
  expires_at: number;
  email: string;
  chatgpt_account_id: string;
  chatgpt_user_id: string;
  plan_type: string;
  organization_id: string;
}
