const { test, expect } = require("@playwright/test");

// The checked-in observations cover January 17, 2026 (UTC).
const DAY = "start=2026-01-17T00%3A00%3A00Z&end=2026-01-18T00%3A00%3A00Z";
const dashboard = (extra = "") => `/?${DAY}${extra}`;

function collectErrors(page) {
  const errors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text());
  });
  page.on("pageerror", (error) => errors.push(error.message));
  return () => errors.filter((e) => !e.includes("favicon"));
}

test.describe("Dashboard", () => {
  test("loads without errors, weather first", async ({ page }) => {
    const errors = collectErrors(page);
    await page.goto(dashboard());
    await expect(page).toHaveTitle(/4cast Truth Oracle/);
    await expect(page.locator(".site-header")).toHaveCount(1);

    const main = page.locator("#main-content > *");
    await expect(main.first()).toHaveId("weather-table-container");
    await expect(page.getByRole("heading", { name: "Current weather" })).toBeVisible();
    await expect(page.locator("text=No weather data available")).toHaveCount(0);

    // The keys are folded away near the bottom.
    const keys = page.locator("details.oracle-info");
    await expect(keys).not.toHaveAttribute("open", "");
    await keys.locator("summary").click();
    await expect(page.getByText("Public key (base64)")).toBeVisible();

    expect(errors()).toHaveLength(0);
  });

  test("map pins are coloured by temperature, with a legend and values on hover", async ({ page }) => {
    await page.goto(dashboard("&view=map"));
    const pins = page.locator(".station-markers .pin");
    expect(await pins.count()).toBeGreaterThan(0);
    await expect(page.locator(".map-legend")).toContainText("90°F and above");
    await expect(pins.first().locator("title")).toContainText("Latest");

    await pins.first().click();
    const panel = page.locator("#map-station .station-detail");
    await expect(panel).toBeVisible({ timeout: 10000 });
    await expect(panel.locator(".forecast-detail")).toHaveCount(1);
  });

  test("the list opens a station's forecast and the search filters on the server", async ({ page }) => {
    await page.goto(dashboard("&view=map"));
    await page.getByRole("link", { name: "List", exact: true }).click();
    await expect(page).toHaveURL(/view=list/);

    const stations = page.locator("#weather-list details.wx-station");
    expect(await stations.count()).toBeGreaterThan(0);
    await expect(page.locator(".wx-header")).toBeVisible();

    const first = stations.first();
    await first.locator("summary").click();
    await expect(first.locator(".forecast-detail")).toBeVisible({ timeout: 10000 });
    await first.locator("summary").click();
    await expect(first.locator(".forecast-detail")).toBeHidden();

    const id = (await first.locator(".wx-name strong").textContent()).trim();
    const search = page.locator("#weather-search");
    await search.fill(id);
    await expect(stations).toHaveCount(1);
    await expect(search).toBeFocused();
    await expect(page).toHaveURL(new RegExp(`q=${id}`));

    await search.fill("no such station anywhere");
    await expect(page.locator("#weather-list")).toContainText("No station in this list matches");
  });

  test("the chosen view is remembered", async ({ page }) => {
    await page.goto(dashboard("&view=list"));
    await page.goto(dashboard());
    await expect(page.locator("#weather-search")).toBeVisible();
  });

  test("phones see the tabs and a list without sideways scrolling", async ({ page }) => {
    await page.setViewportSize({ width: 360, height: 800 });
    await page.goto(dashboard("&view=list"));
    await expect(page.locator(".site-tabs a[href='/events']")).toBeVisible();
    await expect(page.locator(".site-tabs a[href='/raw']")).toBeVisible();
    await expect(page.locator(".wx-station").first()).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(360);
  });
});

test.describe("Raw Data Page", () => {
  test("defaults to yesterday (UTC) with both kinds ticked", async ({ page }) => {
    await page.goto("/raw");
    const start = await page.locator("#start").inputValue();
    const end = await page.locator("#end").inputValue();
    expect(start).toMatch(/T00:00$/);
    expect(end).toMatch(/T00:00$/);
    expect(Date.parse(`${end}Z`) - Date.parse(`${start}Z`)).toBe(24 * 3600 * 1000);
    await expect(page.locator("#observations")).toBeChecked();
    await expect(page.locator("#forecasts")).toBeChecked();
    await expect(page.locator(".schema-box").first()).toBeVisible();
    await expect(page.getByRole("button", { name: "Run query" })).toBeVisible();
  });
});

test.describe("Events Page", () => {
  test("loads without errors, with filters", async ({ page }) => {
    const errors = collectErrors(page);
    await page.goto("/events");
    await expect(page.locator(".site-header")).toHaveCount(1);
    await expect(page.locator(".status-filter")).toBeVisible();
    await expect(page.getByText("Show test events")).toBeVisible();
    await page.locator(".status-chip", { hasText: "Signed" }).click();
    await expect(page).toHaveURL(/status=signed/);
    expect(errors()).toHaveLength(0);
  });
});

test.describe("HTMX Navigation", () => {
  test("tabs swap the content and keep the current tab marked", async ({ page }) => {
    await page.goto(dashboard());

    await page.click('.site-tabs a[href="/raw"]');
    await expect(page).toHaveURL(/\/raw$/);
    await expect(page.locator(".site-header")).toHaveCount(1);
    await expect(page.locator('.site-tabs li.is-active a[href="/raw"]')).toHaveCount(1);

    await page.click('.site-tabs a[href="/events"]');
    await expect(page).toHaveURL(/\/events$/);
    await expect(page.locator('.site-tabs li.is-active a[href="/events"]')).toHaveCount(1);
    await expect(page.locator(".site-tabs li.is-active")).toHaveCount(1);

    await page.click('.site-tabs a[href="/"]');
    await expect(page.getByRole("heading", { name: "Current weather" })).toBeVisible();
    await expect(page.locator(".site-header")).toHaveCount(1);
  });
});

test.describe("Assets", () => {
  test("the stylesheet and script are served at hashed URLs with long caching", async ({ page, request }) => {
    await page.goto("/events");
    const css = await page.locator('link[href^="/assets/site."]').getAttribute("href");
    const js = await page.locator('script[src^="/assets/site."]').getAttribute("src");
    for (const url of [css, js]) {
      const response = await request.get(url);
      expect(response.ok()).toBeTruthy();
      expect(response.headers()["cache-control"]).toBe("public, max-age=31536000, immutable");
    }
  });
});

test.describe("API Endpoints", () => {
  test("oracle pubkey endpoint returns data", async ({ request }) => {
    const response = await request.get("/oracle/pubkey");
    expect(response.ok()).toBeTruthy();
    expect(await response.json()).toHaveProperty("key");
  });

  test("files endpoint returns list with valid params", async ({ request }) => {
    const response = await request.get("/files?start=2026-01-01T00:00:00Z&end=2026-01-20T00:00:00Z");
    expect(response.ok()).toBeTruthy();
    expect(await response.json()).toHaveProperty("file_names");
  });

  test("stations endpoint returns data", async ({ request }) => {
    const response = await request.get("/stations");
    expect(response.ok()).toBeTruthy();
  });

  test("forecast fragment endpoint returns HTML", async ({ request }) => {
    const response = await request.get("/fragments/forecast/KATL");
    expect(response.ok()).toBeTruthy();
    expect(await response.text()).toContain("forecast");
  });
});
