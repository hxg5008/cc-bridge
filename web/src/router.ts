import { createRouter, createWebHistory } from 'vue-router';
import { setAuth, api } from './api';

/** 认证状态 */
let authenticated = false;

/**
 * 验证已保存的凭证
 * @returns 验证是否通过
 */
async function tryRestoreAuth(): Promise<boolean> {
  const saved = localStorage.getItem('claude-code-gateway_auth');
  if (!saved) return false;
  setAuth(saved);
  try {
    await api.getDashboard();
    authenticated = true;
    return true;
  } catch {
    localStorage.removeItem('claude-code-gateway_auth');
    setAuth('');
    return false;
  }
}

/** 初始化 Promise，确保只验证一次 */
let initPromise: Promise<boolean> | null = null;

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: '/login',
      name: 'login',
      component: () => import('./components/Login.vue'),
    },
    {
      path: '/',
      name: 'dashboard',
      component: () => import('./components/Dashboard.vue'),
      meta: { requiresAuth: true },
      children: [
        { path: '', name: 'accounts', component: () => import('./components/Accounts.vue') },
        { path: 'tokens', name: 'tokens', component: () => import('./components/Tokens.vue') },
        { path: 'cache-stats', name: 'cache-stats', component: () => import('./components/CacheStats.vue') },
      ],
    },
  ],
});

router.beforeEach(async (to) => {
  if (!authenticated && !initPromise) {
    initPromise = tryRestoreAuth();
  }
  if (initPromise) {
    await initPromise;
    initPromise = null;
  }

  if (to.meta.requiresAuth && !authenticated) {
    return { name: 'login' };
  }
  if (to.name === 'login' && authenticated) {
    // 同 login() 修复: 跳子路由 accounts, 不跳父 dashboard, 防 router-view 空白
    return { name: 'accounts' };
  }
});

/**
 * 登录并跳转到仪表盘
 * @param password 管理员密码
 */
export async function login(password: string): Promise<void> {
  setAuth(password);
  await api.getDashboard();
  authenticated = true;
  localStorage.setItem('claude-code-gateway_auth', password);
  // 跳到子路由 'accounts' 而非父路由 'dashboard',
  // 否则 Vue Router 4 不会自动激活 default child, 子 <router-view> 为空,
  // 用户必须点切换 tab 才会渲染账号列表 (UX bug)。
  await router.push({ name: 'accounts' });
}

/** 退出登录并跳转到登录页 */
export function logout(): void {
  authenticated = false;
  localStorage.removeItem('claude-code-gateway_auth');
  setAuth('');
  router.push({ name: 'login' });
}

export default router;
