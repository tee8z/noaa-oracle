const { test, expect } = require("@playwright/test");

test("a response before htmx initialization already has its Trusted Types policy", async ({ page }) => {
  // Force the ordering that occurs intermittently with deferred scripts in
  // WebKit. Do not change htmx's configuration, policy, or response parsing.
  await page.addInitScript(() => {
    window.qaViolations = [];
    document.addEventListener("securitypolicyviolation", event => {
      window.qaViolations.push(event.effectiveDirective);
    });
    const schedule = window.setTimeout;
    window.setTimeout = function (callback, delay, ...args) {
      if (typeof callback === "function" && callback.toString().includes("this.initialize()")) {
        window.qaInitializationHeld = true;
        return schedule.call(this, () => {
          window.qaInitializationRan = true;
          callback(...args);
        }, 3000);
      }
      return schedule.call(this, callback, delay, ...args);
    };
  });
  await page.route("**/__qa/early-fragment", route => route.fulfill({
    contentType: "text/html", body: '<p id="early-policy-ready">Early response loaded</p>',
  }));
  await page.route("**/assets/site.*.js", async route => {
    const response = await route.fetch();
    const earlyRequest = `
      const qaTarget = document.createElement("aside");
      qaTarget.id = "qa-early-target";
      document.body.append(qaTarget);
      htmx.ajax("GET", "/__qa/early-fragment", { target: qaTarget, swap: "innerHTML" });
    `;
    await route.fulfill({ response, body: earlyRequest + await response.text() });
  });
  await page.goto("/events", { waitUntil: "domcontentloaded" });
  await expect(page.locator("#early-policy-ready")).toHaveText("Early response loaded");
  expect(await page.evaluate(() => window.qaInitializationHeld)).toBe(true);
  expect(await page.evaluate(() => window.qaInitializationRan)).toBeUndefined();
  expect(await page.evaluate(() => window.qaViolations)).toEqual([]);
});

test("Raw data imports its local module without failed CDN preloads", async ({ page }) => {
  test.setTimeout(120000);
  const failures = [];
  const scripts = [];
  page.on("response", response => {
    if (response.status() >= 400) failures.push(new URL(response.url()).pathname);
  });
  page.on("request", request => {
    if (request.resourceType() === "script") scripts.push(request.url());
  });
  await page.goto("/raw");
  const module = await page.locator("#raw-data").getAttribute("data-duckdb-module");
  expect(module).toMatch(/^\/assets\/duckdb\.[a-f0-9]{16}\.js$/);
  await expect(page.locator("#runQuery")).toBeEnabled({ timeout: 60000 });
  await page.locator("#customQuery").fill("SELECT 42 AS answer");
  await page.locator("#runQuery").click();
  await expect(page.locator("#queryResult tbody td")).toHaveText("42");
  expect(scripts.some(url => new URL(url).pathname === module)).toBe(true);
  expect(scripts.some(url => url.includes("/+esm"))).toBe(false);
  expect(failures).toEqual([]);
});
