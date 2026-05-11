import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';
import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';
import tailwindcss from '@tailwindcss/vite';

// 从 ../.version 读取后端版本注入到 __APP_VERSION__, UI 顶部显示用。
// .version 格式: "version=1.9.9" 一行
function readBackendVersion(): string {
  try {
    const txt = readFileSync(fileURLToPath(new URL('../.version', import.meta.url)), 'utf-8');
    const m = txt.match(/^version=(.+)$/m);
    return m ? m[1].trim() : 'dev';
  } catch {
    return 'dev';
  }
}

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  define: {
    __APP_VERSION__: JSON.stringify(readBackendVersion()),
  },
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    port: 3000,
    proxy: {
      '/admin': 'http://localhost:5674',
      '/v1': 'http://localhost:5674',
      '/_health': 'http://localhost:5674',
    },
  },
});
