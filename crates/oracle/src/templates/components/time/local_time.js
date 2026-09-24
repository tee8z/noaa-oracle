// The server writes times in UTC because it cannot know the reader's time
// zone. Rewrite them into local time; the UTC text stays in the tooltip.
(function () {
  var dayTime = { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" };
  var clock = { hour: "2-digit", minute: "2-digit" };
  var zone = function (date) {
    var parts = date.toLocaleTimeString(undefined, { timeZoneName: "short" }).split(" ");
    return parts[parts.length - 1];
  };
  var localize = function (root) {
    root.querySelectorAll("time.local-time[datetime]").forEach(function (el) {
      var date = new Date(el.getAttribute("datetime"));
      if (isNaN(date)) return;
      el.title = el.title || el.textContent;
      el.textContent = date.toLocaleString(undefined, dayTime) + " " + zone(date);
    });
    root.querySelectorAll(".local-window[data-start][data-end]").forEach(function (el) {
      var start = new Date(el.dataset.start);
      var end = new Date(el.dataset.end);
      if (isNaN(start) || isNaN(end)) return;
      el.title = el.title || el.textContent;
      var sameDay = start.toDateString() === end.toDateString();
      el.textContent = start.toLocaleString(undefined, dayTime) +
        (sameDay ? "–" + end.toLocaleTimeString(undefined, clock)
                 : " – " + end.toLocaleString(undefined, dayTime)) +
        " " + zone(end);
    });
  };
  document.addEventListener("DOMContentLoaded", function () { localize(document); });
  document.addEventListener("htmx:load", function (event) { localize(event.target); });
})();
