const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const { chromium } = require(process.env.PLAYWRIGHT_CORE || "playwright");

// Run against an isolated oracle using the checked-in January weather fixtures.
const baseURL = process.env.BASE_URL || "http://127.0.0.1:9800";
const artifacts = process.env.ORACLE_BROWSER_ARTIFACTS || "/tmp/oracle-browser-artifacts";
const dashboard = "/?start=2026-01-17T00%3A00%3A00Z&end=2026-01-18T00%3A00%3A00Z";

test("Chromium navigation, station search and map detail fit desktop and phone", { timeout: 120_000 }, async (t) => {
  const browser = await chromium.launch({ headless: true });
  await fs.mkdir(artifacts, { recursive: true });
  try {
    for (const viewport of [{ width: 1280, height: 900 }, { width: 360, height: 800 }]) {
      await t.test(`${viewport.width}px`, async () => {
        const context = await browser.newContext({ baseURL, viewport, timezoneId: "America/New_York" });
        const page = await context.newPage();
        const errors = [];
        page.on("pageerror", (error) => errors.push(error.message));
        page.on("console", (message) => {
          if (message.type() === "error" && !message.text().includes("favicon")) errors.push(message.text());
        });
        const oneLayout = async () => {
          assert.equal(await page.locator(".site-header").count(), 1, "one site header after htmx navigation");
          assert.equal(await page.locator("#main-content").count(), 1, "one main content container");
          assert.equal(await page.locator(".site-tabs li.is-active").count(), 1, "one active navigation tab");
          const width = await page.evaluate(() => document.documentElement.scrollWidth);
          assert.ok(width <= viewport.width, `page width ${width} exceeds viewport ${viewport.width}`);
        };
        try {
          await page.goto(`${dashboard}&view=list`);
          await page.locator("#weather-list details.wx-station").first().waitFor();
          await oneLayout();
          const initialCount = await page.locator("details.wx-station").count();
          const stationId = (await page.locator(".wx-name strong").first().textContent()).trim();
          const search = page.locator("#weather-search");
          await search.fill(stationId);
          await page.waitForFunction(() => document.querySelectorAll("details.wx-station").length === 1);
          assert.ok(await search.evaluate((element) => element === document.activeElement), "search retains focus after swap");
          assert.ok(page.url().includes(`q=${stationId}`), "station search updates the URL");
          await page.locator("details.wx-station summary").click();
          await page.locator("details.wx-station .forecast-detail").waitFor();
          await oneLayout();
          await search.fill("no such station anywhere");
          await page.getByText("No station in this list matches", { exact: false }).waitFor();
          await search.fill("");
          await page.waitForFunction((count) => document.querySelectorAll("details.wx-station").length === count, initialCount);

          await page.getByRole("link", { name: "Map", exact: true }).click();
          await page.locator(".station-markers .pin").first().waitFor();
          // Keyboard activation also checks the accessible SVG button behavior.
          await page.locator(".station-markers .pin").first().focus();
          await page.keyboard.press("Enter");
          const detail = page.locator("#map-station .station-detail");
          await detail.waitFor();
          assert.equal(await detail.locator(".forecast-detail").count(), 1);
          const secondPin = page.locator(".station-markers .pin").nth(1);
          const secondStation = (await secondPin.getAttribute("hx-get")).split("/").pop();
          await secondPin.focus();
          await page.keyboard.press("Enter");
          await page.waitForFunction((id) => document.querySelector("#map-station .station-detail h3")?.textContent === id, secondStation);
          assert.equal(await page.locator("#map-station").count(), 1, "map target survives repeated selections");
          await detail.scrollIntoViewIfNeeded();
          const bounds = await detail.boundingBox();
          assert.ok(bounds.x >= -1 && bounds.x + bounds.width <= viewport.width + 1, "map detail stays within horizontal viewport bounds");
          await oneLayout();
          await page.screenshot({ path: path.join(artifacts, `map-detail-${viewport.width}.png`), fullPage: true });

          for (const destination of ["/raw", "/events", "/raw", "/events"]) {
            await page.locator(`.site-tabs a[href='${destination}']`).click();
            await page.waitForURL((url) => url.pathname === destination);
            await page.locator(`.site-tabs li.is-active a[href='${destination}']`).waitFor();
            await oneLayout();
          }
          await page.goBack();
          await page.locator(".site-tabs li.is-active a[href='/raw']").waitFor();
          await oneLayout();
          await page.goForward();
          await page.locator(".site-tabs li.is-active a[href='/events']").waitFor();
          await oneLayout();
          await page.screenshot({ path: path.join(artifacts, `navigation-${viewport.width}.png`), fullPage: true });
          assert.deepEqual(errors, [], "browser console and page errors");
        } catch (error) {
          if (errors.length) console.error("Browser errors:", errors);
          await page.screenshot({ path: path.join(artifacts, `failure-${viewport.width}.png`), fullPage: true });
          throw error;
        } finally {
          await context.close();
        }
      });
    }
  } finally {
    await browser.close();
  }
});
