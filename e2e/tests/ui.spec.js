const { test, expect } = require("@playwright/test");

// The checked-in observations cover January 17, 2026 (UTC).
const DAY = "start=2026-01-17T00%3A00%3A00Z&end=2026-01-18T00%3A00%3A00Z";
const dashboard = (extra = "") => `/?${DAY}${extra}`;

// Pins of stations with no close neighbour, so a click at a pin's centre
// can't land on a neighbouring dot.
const pin = (page, station = "KSLC") =>
  page.locator(`.station-markers .pin[hx-get="/fragments/station/${station}"]`);

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

    // Every dot is on top where it is drawn: no neighbour's wider hit area
    // covers it.
    const covered = await page.evaluate(() =>
      [...document.querySelectorAll(".station-markers .pin-dot")]
        .map((dot) => dot.getBoundingClientRect())
        .filter((box) => box.bottom <= innerHeight && box.right <= innerWidth)
        .filter((box) => {
          const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
          return !hit || !hit.classList.contains("pin-dot");
        }).length,
    );
    expect(covered).toBe(0);

    await pin(page).click();
    const panel = page.locator("#map-station .station-detail");
    await expect(panel).toBeVisible({ timeout: 10000 });
    await expect(panel.locator(".station-detail-head h3")).toHaveText("KSLC");
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

  test("on a phone, an opened station's wide table scrolls in its own box", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(dashboard("&view=list"));
    const station = page.locator("#weather-list details.wx-station").first();
    await station.locator("summary").click();
    await expect(station.locator(".forecast-detail")).toHaveCount(1);
    // The checked-in data has no past week to compare, so give the detail
    // a past-week table as wide as a real one.
    // The page enforces Trusted Types, so build it with DOM calls.
    await station.locator(".forecast-detail").evaluate((detail) => {
      const el = (tag, className, text) => {
        const node = document.createElement(tag);
        if (className) node.className = className;
        if (text) node.textContent = text;
        return node;
      };
      const row = el("tr");
      const day = el("th", null, "Tue, Sep 22");
      day.scope = "row";
      row.append(day);
      for (let i = 0; i < 6; i++) {
        const cell = el("td");
        cell.append(el("span", "obs", "61°F −10°F"), el("span", "fcst", "71°F"));
        row.append(cell);
      }
      const body = el("tbody");
      body.append(row);
      const table = el("table", "table is-narrow is-fullwidth past-table");
      table.append(body);
      const container = el("div", "table-container");
      container.append(table);
      const section = el("section");
      section.append(
        el("p", "forecast-note", "By day (UTC). Each forecast was issued the day before."),
        container,
      );
      detail.prepend(section);
    });
    const container = station.locator(".table-container");
    expect(await container.evaluate((box) => box.scrollWidth > box.clientWidth)).toBe(true);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);
  });

  test("the search spinner shows only while a search runs", async ({ page }) => {
    await page.goto(dashboard("&view=list"));
    const spinner = page.locator("#weather-search-loading");
    await expect(spinner).toBeHidden();
    let release;
    const held = new Promise((resolve) => (release = resolve));
    const search = (url) => url.pathname === "/fragments/weather" && url.searchParams.get("q") === "K";
    await page.route(search, async (route) => {
      await held;
      await route.continue();
    });
    await page.locator("#weather-search").fill("K");
    await expect(spinner).toBeVisible();
    release();
    await expect(spinner).toBeHidden();
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

test.describe("Raw Data Page with DuckDB", () => {
  test("loads files, queries them and exports CSV with its header", async ({ page }) => {
    test.setTimeout(120000);
    const errors = collectErrors(page);
    await page.goto("/raw");
    // DuckDB-WASM loads from jsdelivr under the page's narrow policy.
    await expect(page.locator("#raw-data-status")).toHaveText(/^Ready/, { timeout: 60000 });
    await page.fill("#start", "2026-01-17T00:00");
    await page.fill("#end", "2026-01-18T00:00");
    await page.click("#submit");
    await expect(page.locator("#raw-data-status")).toHaveText(/^Loaded \d+ files/, { timeout: 60000 });
    await expect(page.locator("#observations-status")).toContainText("fields");

    await page.getByRole("button", { name: "Station list" }).click();
    await expect(page.locator("#queryResult tbody tr").first()).toBeVisible();

    // Empty text keeps its column; numbers stay numbers.
    await page.fill(
      "#customQuery",
      "SELECT 'a' AS first, '' AS empty, NULL AS nothing, 'c' AS last, -5 AS negative, '=1' AS formula",
    );
    await page.click("#runQuery");
    await expect(page.locator("#queryResult thead th")).toHaveText([
      "first", "empty", "nothing", "last", "negative", "formula",
    ]);
    await expect(page.locator("#queryResult tbody tr").first().locator("td")).toHaveText([
      "a", "", "", "c", "-5", "=1",
    ]);
    const [download] = await Promise.all([page.waitForEvent("download"), page.click("#downloadCsv")]);
    const csv = require("fs").readFileSync(await download.path(), "utf8");
    expect(csv).toBe("first,empty,nothing,last,negative,formula\na,,,c,-5,'=1");
    expect(errors()).toHaveLength(0);
  });
});

test.describe("Events Page", () => {
  test("loads without errors, with filters", async ({ page }) => {
    const errors = collectErrors(page);
    await page.goto("/events");
    await expect(page.locator(".site-header")).toHaveCount(1);
    await expect(page.locator(".status-filter")).toBeVisible();
    await expect(page.getByText("Show unlisted")).toBeVisible();
    await page.locator(".status-chip", { hasText: "Signed" }).click();
    await expect(page).toHaveURL(/status=signed/);
    await page.getByLabel("Show unlisted").check();
    await expect(page).toHaveURL(/status=signed&unlisted=show/);
    await expect(page.getByLabel("Show unlisted")).toBeChecked();
    expect(errors()).toHaveLength(0);
  });
});

// One site header, one main area and one marked tab, and nothing wider
// than the screen, after every htmx swap.
async function expectOneLayout(page) {
  await expect(page.locator(".site-header")).toHaveCount(1);
  await expect(page.locator("#main-content")).toHaveCount(1);
  await expect(page.locator(".site-tabs li.is-active")).toHaveCount(1);
  const width = await page.evaluate(() => document.documentElement.scrollWidth);
  expect(width).toBeLessThanOrEqual(page.viewportSize().width);
}

for (const viewport of [
  { width: 1280, height: 900 },
  { width: 360, height: 800 },
]) {
  test.describe(`Navigation at ${viewport.width}px`, () => {
    test.use({ viewport, timezoneId: "America/New_York" });

    test("search, map details and tabs keep one layout, and history works", async ({ page }) => {
      const errors = collectErrors(page);
      await page.goto(dashboard("&view=list"));
      await page.locator("#weather-list details.wx-station").first().waitFor();
      await expectOneLayout(page);

      // The search replaces only the list, so the box keeps focus.
      const stations = page.locator("details.wx-station");
      const initialCount = await stations.count();
      const stationId = (await page.locator(".wx-name strong").first().textContent()).trim();
      const search = page.locator("#weather-search");
      await search.fill(stationId);
      await expect(stations).toHaveCount(1);
      await expect(search).toBeFocused();
      await expect(page).toHaveURL(new RegExp(`q=${stationId}`));
      await stations.first().locator("summary").click();
      await expect(stations.first().locator(".forecast-detail")).toHaveCount(1);
      await search.fill("no such station anywhere");
      await expect(page.locator("#weather-list")).toContainText("No station in this list matches");
      await search.fill("");
      await expect(stations).toHaveCount(initialCount);
      await expectOneLayout(page);

      // Pins are links: Enter opens one, and a second replaces the first.
      await page.getByRole("link", { name: "Map", exact: true }).click();
      const pins = page.locator(".station-markers .pin");
      await pins.first().focus();
      await page.keyboard.press("Enter");
      const detail = page.locator("#map-station .station-detail");
      await expect(detail.locator(".forecast-detail")).toHaveCount(1);
      const second = (await pins.nth(1).getAttribute("hx-get")).split("/").pop();
      await pins.nth(1).focus();
      await page.keyboard.press("Enter");
      await expect(detail.locator(".station-detail-head h3")).toHaveText(second);
      await expect(page.locator("#map-station")).toHaveCount(1);
      const bounds = await detail.boundingBox();
      expect(bounds.x).toBeGreaterThanOrEqual(-1);
      expect(bounds.x + bounds.width).toBeLessThanOrEqual(viewport.width + 1);
      await expectOneLayout(page);

      // The raw data page always loads whole; the others swap in.
      for (const destination of ["/raw", "/events", "/raw", "/events"]) {
        await page.locator(`.site-tabs a[href='${destination}']`).click();
        await expect(page).toHaveURL((url) => url.pathname === destination);
        await expect(page.locator(`.site-tabs li.is-active a[href='${destination}']`)).toHaveCount(1);
        await expectOneLayout(page);
      }
      // Back from a swapped page to the raw data page reloads it, so it
      // gets its own policy and script again.
      await page.goBack();
      await expect(page.locator(".site-tabs li.is-active a[href='/raw']")).toHaveCount(1);
      await expect(page.locator("#raw-data-status")).not.toHaveText("Loading DuckDB…", { timeout: 60000 });
      await expectOneLayout(page);
      await page.goForward();
      await expect(page.locator(".site-tabs li.is-active a[href='/events']")).toHaveCount(1);
      await expectOneLayout(page);
      expect(errors()).toHaveLength(0);
    });
  });
}

test.describe("HTMX Navigation", () => {
  test("going back restores the page htmx swapped away from", async ({ page }) => {
    const errors = collectErrors(page);
    await page.goto(dashboard());
    await page.click('.site-tabs a[href="/events"]');
    await expect(page).toHaveURL(/\/events$/);
    await expect(page).toHaveTitle(/Events/);
    await page.goBack();
    await expect(page.getByRole("heading", { name: "Current weather" })).toBeVisible();
    await expect(page).toHaveTitle(/Dashboard/);
    await expectOneLayout(page);
    expect(errors()).toHaveLength(0);
  });

  test("the weather refresh keeps running, and keeps the open station", async ({ page }) => {
    await page.goto(dashboard("&view=map"));
    await pin(page).click();
    await expect(page.locator("#map-station .station-detail")).toHaveCount(1);
    // Run the five-minute refresh now.
    await page.evaluate(async () => {
      const section = document.getElementById("weather-table-container");
      section.dataset.before = "refresh";
      await htmx.ajax("GET", section.getAttribute("hx-get"), {
        source: section,
        target: section,
        swap: "outerHTML",
      });
    });
    await expect(page.locator("#weather-table-container[data-before]")).toHaveCount(0);
    await expect(page.locator("#weather-table-container")).toHaveCount(1);
    await expect(page.locator("#map-station .station-detail")).toHaveCount(1);
  });

  test("a station's error reply shows the message and a retry, not its body", async ({ page }) => {
    await page.goto(dashboard("&view=map"));
    await page.route("**/fragments/station/**", (route) =>
      route.fulfill({ status: 500, contentType: "text/plain", body: "boom" }),
    );
    await pin(page).click();
    const panel = page.locator("#map-station");
    await expect(panel.locator(".load-error")).toContainText("Couldn't load this station");
    await expect(panel).not.toContainText("boom");
    await expect(page.locator("#map-station-loading")).toBeHidden();
    await page.unroute("**/fragments/station/**");
    await panel.getByRole("button", { name: "Try again" }).click();
    await expect(panel.locator(".station-detail .forecast-detail")).toHaveCount(1);
  });

  test("on a wide screen, a pin's station scrolls into view", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto(dashboard("&view=map"));
    await pin(page).click();
    const detail = page.locator("#map-station .station-detail");
    await expect(detail).toHaveCount(1);
    await expect(detail.locator(".station-detail-head h3")).toBeInViewport();
  });

  test("a station request that gets no reply says so and can be retried", async ({ page }) => {
    await page.goto(dashboard("&view=map"));
    await page.route("**/fragments/station/**", (route) => route.abort("failed"));
    await pin(page).click();
    const panel = page.locator("#map-station");
    await expect(panel.locator(".load-error")).toContainText("Couldn't load this station");
    await expect(page.locator("#map-station-loading")).toBeHidden();
    await page.unroute("**/fragments/station/**");
    await panel.getByRole("button", { name: "Try again" }).click();
    await expect(panel.locator(".station-detail .forecast-detail")).toHaveCount(1);
  });
});

test.describe("Content-Security-Policy", () => {
  test("scripts and hx-on in a swapped response do not run", async ({ page, browserName }) => {
    const response = await page.goto(dashboard("&view=map"));
    const policy = response.headers()["content-security-policy"];
    expect(policy).toContain("script-src 'self';");
    expect(policy).toContain("require-trusted-types-for 'script'; trusted-types htmx");
    expect(policy).not.toContain("unsafe-eval");
    expect(policy).not.toContain("unsafe-inline");
    const station = "**/fragments/station/**";
    const answer = (body) => (route) =>
      route.fulfill({ status: 200, contentType: "text/html; charset=utf-8", body });

    // A <script> in the reply: Trusted Types refuse it (the "htmx" policy
    // has no createScript), and without them the policy blocks inline code.
    await page.route(station, answer('<p id="injected-script">x</p><script>window.ranScript = 1</script>'));
    await pin(page).click();
    await page.waitForTimeout(500);
    expect(await page.evaluate(() => window.ranScript)).toBeUndefined();

    // hx-on and an inline handler are swapped in but never run.
    await page.unroute(station);
    await page.route(
      station,
      answer(
        '<button id="injected-hx-on" hx-on:click="window.ranHxOn = 1">x</button>' +
          '<img id="injected-img" src="/missing.png" onerror="window.ranHandler = 1">',
      ),
    );
    await pin(page, "KSEA").click();
    await page.locator("#injected-hx-on").click();
    await page.waitForTimeout(500);
    expect(await page.evaluate(() => [window.ranHxOn, window.ranHandler])).toEqual([undefined, undefined]);

    if (browserName === "chromium") {
      // Only the htmx policy may create HTML from a string.
      const sink = await page.evaluate(() => {
        try {
          document.body.insertAdjacentHTML("beforeend", "<b>x</b>");
          return "allowed";
        } catch (error) {
          return error.name;
        }
      });
      expect(sink).toBe("TypeError");
    }
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

  test("an unknown station is not found", async ({ request }) => {
    for (const path of ["/fragments/station/ZZZZ", "/fragments/forecast/ZZZZ"]) {
      const response = await request.get(path);
      expect(response.status()).toBe(404);
    }
  });

  test("pages and fragments are gzipped", async ({ request }) => {
    const response = await request.get("/fragments/weather?view=list", {
      headers: { "Accept-Encoding": "gzip", "HX-Request": "true" },
    });
    expect(response.ok()).toBeTruthy();
    expect(response.headers()["content-encoding"]).toBe("gzip");
  });

  test("forecast fragment endpoint returns HTML", async ({ request }) => {
    const response = await request.get("/fragments/forecast/KATL");
    expect(response.ok()).toBeTruthy();
    expect(await response.text()).toContain("forecast");
  });
});
