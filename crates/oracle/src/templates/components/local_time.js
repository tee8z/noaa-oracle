// Local time conversion for UTC timestamps
// Converts all elements with class "local-time" to user's local timezone

function convertToLocalTime() {
  // Convert single timestamps
  const elements = document.querySelectorAll(".local-time[data-utc]");

  elements.forEach((el) => {
    const utcString = el.getAttribute("data-utc");
    if (!utcString) return;

    try {
      const date = new Date(utcString);
      if (isNaN(date.getTime())) return;

      // Format as local date/time
      const options = {
        year: "numeric",
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        timeZoneName: "short",
      };

      el.textContent = date.toLocaleString(undefined, options);
    } catch (e) {
      // Keep original value on error
      console.warn("Failed to convert time:", utcString, e);
    }
  });

  // Convert date-only elements (for forecasts)
  const dateElements = document.querySelectorAll(".local-date[data-utc]");

  dateElements.forEach((el) => {
    const utcString = el.getAttribute("data-utc");
    if (!utcString) return;

    try {
      const date = new Date(utcString);
      if (isNaN(date.getTime())) return;

      const options = {
        weekday: "short",
        month: "short",
        day: "numeric",
      };

      el.textContent = date.toLocaleDateString(undefined, options);
    } catch (e) {
      console.warn("Failed to convert date:", utcString, e);
    }
  });

  // Convert time ranges (observed start - end)
  const rangeElements = document.querySelectorAll(
    ".local-time-range[data-utc-start][data-utc-end]",
  );

  rangeElements.forEach((el) => {
    const startUtc = el.getAttribute("data-utc-start");
    const endUtc = el.getAttribute("data-utc-end");
    if (!startUtc || !endUtc) return;

    try {
      const startDate = new Date(startUtc);
      const endDate = new Date(endUtc);
      if (isNaN(startDate.getTime()) || isNaN(endDate.getTime())) return;

      // Time-only format for the range
      const timeOptions = {
        hour: "2-digit",
        minute: "2-digit",
      };

      // Check if same day - if so, just show time range
      const sameDay = startDate.toDateString() === endDate.toDateString();

      if (sameDay) {
        const startTime = startDate.toLocaleTimeString(undefined, timeOptions);
        const endTime = endDate.toLocaleTimeString(undefined, timeOptions);
        const dateStr = startDate.toLocaleDateString(undefined, {
          month: "short",
          day: "numeric",
        });
        const tz = endDate
          .toLocaleTimeString(undefined, { timeZoneName: "short" })
          .split(" ")
          .pop();
        el.textContent = `${dateStr}, ${startTime} - ${endTime} ${tz}`;
      } else {
        // Different days - show full range
        const options = {
          month: "short",
          day: "numeric",
          hour: "2-digit",
          minute: "2-digit",
        };
        const startStr = startDate.toLocaleString(undefined, options);
        const endStr = endDate.toLocaleString(undefined, options);
        const tz = endDate
          .toLocaleTimeString(undefined, { timeZoneName: "short" })
          .split(" ")
          .pop();
        el.textContent = `${startStr} - ${endStr} ${tz}`;
      }
    } catch (e) {
      console.warn("Failed to convert time range:", startUtc, endUtc, e);
    }
  });
}

// Initialize on page load
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", convertToLocalTime);
} else {
  convertToLocalTime();
}

// Re-run after HTMX swaps (for SPA navigation and partial updates)
document.addEventListener("htmx:afterSwap", convertToLocalTime);
document.addEventListener("htmx:afterSettle", convertToLocalTime);
