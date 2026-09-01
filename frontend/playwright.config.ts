import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  outputDir: "./test-results",
  use: { baseURL: "http://127.0.0.1:18080", trace: "retain-on-failure" },
  reporter: "line",
});
