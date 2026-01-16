const { defineConfig, devices } = require("@playwright/test");
const path = require("path");

module.exports = defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1,
  reporter: process.env.CI ? "github" : "list",
  timeout: 30000,
  use: {
    baseURL: process.env.BASE_URL || "http://localhost:9800",
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    // Disable HTTP/2 to avoid connection issues
    ignoreHTTPSErrors: true,
  },
  projects: [
    {
      name: "firefox",
      use: { ...devices["Desktop Firefox"] },
    },
  ],
  // In CI, we manage the server externally; locally, start it automatically
  webServer: process.env.CI
    ? undefined
    : {
        command: `NOAA_ORACLE_DATA_DIR=${path.join(__dirname, "fixtures/weather_data")} just run-oracle`,
        url: "http://localhost:9800",
        reuseExistingServer: true,
        timeout: 120000,
        cwd: "..",
      },
});
