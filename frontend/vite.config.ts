import {defineConfig} from 'vitest/config';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
export default defineConfig({
 plugins:[react(),tailwindcss()],
 server:{port:3000,proxy:{'/api':'http://control:8080'}},
 test:{include:['src/**/*.test.ts','src/**/*.test.tsx']}
});
