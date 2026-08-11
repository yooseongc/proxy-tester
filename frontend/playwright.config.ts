import {defineConfig} from '@playwright/test';

export default defineConfig({
 testDir:'./tests',
 outputDir:'./test-results',
 use:{baseURL:'http://localhost:18080',trace:'retain-on-failure'},
 reporter:'line'
});
