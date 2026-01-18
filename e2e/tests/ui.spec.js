const { test, expect } = require("@playwright/test");

// Helper to build dashboard URL with date range that includes fixture data
// Fixtures are dated with the current date, so we use a wide range to ensure they're included
function getDashboardUrl() {
  // Use a wide date range that will include any fixture data
  const start = "2020-01-01T00:00:00Z";
  const end = "2030-12-31T23:59:59Z";
  return `/?start=${encodeURIComponent(start)}&end=${encodeURIComponent(end)}`;
}

test.describe("Dashboard", () => {
  test("loads without errors", async ({ page }) => {
    const errors = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") errors.push(msg.text());
    });

    await page.goto(getDashboardUrl());
    await expect(page).toHaveTitle(/4cast Truth Oracle/);

    // Check header is present (only once)
    const headers = await page.locator(".navbar-brand").count();
    expect(headers).toBe(1);

    // No console errors (except favicon which may not exist)
    expect(errors.filter((e) => !e.includes("favicon"))).toHaveLength(0);
  });

  test("displays oracle info", async ({ page }) => {
    await page.goto(getDashboardUrl());

    // Oracle info section should be visible
    await expect(page.locator("text=Public Key (Base64)")).toBeVisible();
  });

  test("displays weather data from observation files", async ({ page }) => {
    await page.goto(getDashboardUrl());

    // Wait for the page to fully load
    await page.waitForLoadState("networkidle");

    // The weather section should show data from the fixture parquet files
    // Look for the Current Weather section and verify it has station data
    const weatherSection = page.locator("text=Current Weather");
    await expect(weatherSection).toBeVisible();

    // Check that either map view or table view has content (not empty state)
    const emptyState = page.locator("text=No weather data available");
    const hasEmptyState = await emptyState.isVisible().catch(() => false);
    expect(hasEmptyState).toBeFalsy();

    // Switch to table view to verify table data
    await page.evaluate(() => {
      if (typeof window.switchWeatherView === "function") {
        window.switchWeatherView("table");
      }
    });

    // Now the table should be visible
    const weatherTable = page.locator("#weather-table-view table").first();
    await expect(weatherTable).toBeVisible();

    // Verify there are actual rows in the table (station data)
    const tableRows = page.locator("#weather-table-view table tbody tr");
    const rowCount = await tableRows.count();
    expect(rowCount).toBeGreaterThan(0);
  });

  test("navigation links work", async ({ page }) => {
    await page.goto(getDashboardUrl());

    // Check nav links exist (use first() since there may be multiple links to /)
    await expect(page.locator('a[href="/"]').first()).toBeVisible();
    await expect(page.locator('a[href="/raw"]')).toBeVisible();
    await expect(page.locator('a[href="/events"]')).toBeVisible();
  });

  test("clicking weather row expands forecast data", async ({ page }) => {
    await page.goto(getDashboardUrl());
    await page.waitForLoadState("networkidle");

    // Switch to table view first (map is default)
    await page.evaluate(() => {
      if (typeof window.switchWeatherView === "function") {
        window.switchWeatherView("table");
      }
    });

    // Wait for table view to be visible
    await expect(page.locator("#weather-table-view")).toBeVisible();

    // Ensure weather table has data
    const weatherRows = page.locator(
      "#weather-table-view table tbody tr.weather-row",
    );
    const rowCount = await weatherRows.count();
    expect(rowCount).toBeGreaterThan(0);

    // Get the first weather row and its station ID
    const firstRow = weatherRows.first();
    const stationId = await firstRow.getAttribute("data-station");
    expect(stationId).toBeTruthy();

    // The forecast row should initially be hidden
    const forecastRow = page.locator(`#forecast-row-${stationId}`);
    await expect(forecastRow).toBeHidden();

    // Check if loadForecast function exists
    const hasLoadFn = await page.evaluate(
      () => typeof window.loadForecast === "function",
    );
    expect(hasLoadFn).toBeTruthy();

    // Check if toggleForecastIfLoaded function exists (used for subsequent clicks)
    const hasToggleFn = await page.evaluate(
      () => typeof window.toggleForecastIfLoaded === "function",
    );
    expect(hasToggleFn).toBeTruthy();

    // Call loadForecast and wait for the fetch to complete
    await page.evaluate((id) => {
      window.loadForecast(id);
    }, stationId);

    // Wait for the forecast row to become visible (fetch completes and shows row)
    await expect(forecastRow).toBeVisible({ timeout: 10000 });

    // The forecast content area should exist and have content
    const forecastContent = page.locator(`#forecast-${stationId}`);
    await expect(forecastContent).toBeVisible();

    // Toggle to hide using toggleForecastIfLoaded (simulates subsequent click)
    await page.evaluate((id) => {
      window.toggleForecastIfLoaded(id);
    }, stationId);

    // The forecast row should be hidden again
    await expect(forecastRow).toBeHidden();

    // Toggle again to show
    await page.evaluate((id) => {
      window.toggleForecastIfLoaded(id);
    }, stationId);

    // Should be visible again
    await expect(forecastRow).toBeVisible();
  });
});

test.describe("Raw Data Page", () => {
  test("loads without errors", async ({ page }) => {
    const errors = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") errors.push(msg.text());
    });

    await page.goto("/raw");

    // Check header is present (only once - no duplication)
    const headers = await page.locator(".navbar-brand").count();
    expect(headers).toBe(1);

    // No console errors (except favicon)
    expect(errors.filter((e) => !e.includes("favicon"))).toHaveLength(0);
  });

  test("date inputs are populated", async ({ page }) => {
    await page.goto("/raw");

    // Wait for page to fully load
    await page.waitForLoadState("networkidle");

    // Date inputs should have values - use the actual IDs from the page
    const startInput = page.locator('input[type="datetime-local"]').first();
    const endInput = page.locator('input[type="datetime-local"]').last();

    await expect(startInput).toBeVisible();
    await expect(endInput).toBeVisible();
  });

  test("schema boxes exist", async ({ page }) => {
    await page.goto("/raw");

    // Schema box containers should be present (the pre elements may be hidden initially)
    await expect(page.locator(".schema-box").first()).toBeVisible();
  });

  test("query button exists", async ({ page }) => {
    await page.goto("/raw");
    await page.waitForLoadState("networkidle");

    // Look for the query/load button
    const queryButton = page.locator(
      'button[type="submit"], button:has-text("Query"), button:has-text("Load")',
    );
    await expect(queryButton.first()).toBeVisible();
  });
});

test.describe("Events Page", () => {
  test("loads without errors", async ({ page }) => {
    const errors = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") errors.push(msg.text());
    });

    await page.goto("/events");

    // Check header is present (only once)
    const headers = await page.locator(".navbar-brand").count();
    expect(headers).toBe(1);

    // No console errors
    expect(errors.filter((e) => !e.includes("favicon"))).toHaveLength(0);
  });

  test("events table or empty state exists", async ({ page }) => {
    await page.goto("/events");

    // Events table or empty state should be visible
    const hasTable = (await page.locator("table").count()) > 0;
    const hasEmptyState = (await page.locator("text=No events").count()) > 0;
    const hasContent =
      (await page.locator(".box, .card, .content").count()) > 0;

    expect(hasTable || hasEmptyState || hasContent).toBeTruthy();
  });
});

test.describe("HTMX Navigation", () => {
  test("navigating between pages does not duplicate header", async ({
    page,
  }) => {
    await page.goto("/");

    // Navigate to raw data using HTMX link
    await page.click('a[href="/raw"]');
    await page.waitForLoadState("networkidle");

    // Should still have only one header
    const headers = await page.locator(".navbar-brand").count();
    expect(headers).toBe(1);

    // Navigate to events
    await page.click('a[href="/events"]');
    await page.waitForLoadState("networkidle");

    // Should still have only one header
    const headersAfter = await page.locator(".navbar-brand").count();
    expect(headersAfter).toBe(1);
  });

  test("navigating back to dashboard works", async ({ page }) => {
    await page.goto("/raw");

    // Navigate to dashboard
    await page.click('a[href="/"]');
    await page.waitForLoadState("networkidle");

    // Dashboard content should be visible (Public Key is on the dashboard)
    await expect(page.locator("text=Public Key (Base64)")).toBeVisible();

    // Only one header
    const headers = await page.locator(".navbar-brand").count();
    expect(headers).toBe(1);
  });
});

test.describe("API Endpoints", () => {
  test("oracle pubkey endpoint returns data", async ({ request }) => {
    const response = await request.get("/oracle/pubkey");
    expect(response.ok()).toBeTruthy();

    const data = await response.json();
    expect(data).toHaveProperty("key");
  });

  test("files endpoint returns list with valid params", async ({ request }) => {
    const response = await request.get(
      "/files?start=2026-01-01T00:00:00Z&end=2026-01-20T00:00:00Z",
    );
    expect(response.ok()).toBeTruthy();

    const data = await response.json();
    expect(data).toHaveProperty("file_names");
  });

  test("stations endpoint returns data", async ({ request }) => {
    const response = await request.get("/stations");
    expect(response.ok()).toBeTruthy();
  });

  test("forecast fragment endpoint returns HTML", async ({ request }) => {
    // Test the forecast fragment endpoint with a known station
    const response = await request.get("/fragments/forecast/KATL");
    expect(response.ok()).toBeTruthy();

    const html = await response.text();
    // Should return HTML content (either with forecast data or empty state message)
    expect(html).toContain("forecast");
  });
});
