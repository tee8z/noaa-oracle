// Sends what readers do on the site to POST /api/v1/telemetry, when the
// page has <meta name="telemetry" content="on">: the page view and its
// timings, vitals, clicks on buttons and links, form submits, htmx
// requests and script errors. It never reads what is typed into a field,
// and ignores anything inside [data-telemetry="off"]. The session id lives
// in sessionStorage for this tab only; there are no cookies. Nothing here
// may break the page, so every handler swallows its own errors.
(function () {
  try {
    var flag = document.querySelector('meta[name="telemetry"]');
    if (!flag || flag.getAttribute("content") !== "on") return;
  } catch (e) {
    return;
  }

  var ENDPOINT = "/api/v1/telemetry";
  var MAX_BATCH = 50;
  // Below the server's 16 KiB limit.
  var MAX_BATCH_BYTES = 15000;
  var MAX_QUEUE = 200;
  var MAX_STRING = 200;

  function safe(fn) {
    return function () {
      try {
        return fn.apply(this, arguments);
      } catch (e) {}
    };
  }

  function sessionId() {
    var sid = null;
    try {
      sid = sessionStorage.getItem("fdc.sid");
    } catch (e) {}
    if (sid && /^[A-Za-z0-9_-]{16,32}$/.test(sid)) return sid;
    var bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    var binary = "";
    for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    sid = btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    try {
      sessionStorage.setItem("fdc.sid", sid);
    } catch (e) {}
    return sid;
  }

  var sid = sessionId();
  var ridMeta = document.querySelector('meta[name="request-id"]');
  var rid = ridMeta ? ridMeta.getAttribute("content") : null;
  var queue = [];

  function short(value) {
    return String(value).slice(0, MAX_STRING);
  }

  // A path without its query or fragment.
  function pathOf(url) {
    try {
      return new URL(url, location.href).pathname;
    } catch (e) {
      return "";
    }
  }

  function record(ev, fields) {
    if (queue.length >= MAX_QUEUE) return;
    var event = { ev: ev, t: Math.round(performance.now()), page: location.pathname };
    for (var key in fields) {
      var value = fields[key];
      if (value === undefined || value === null || value === "") continue;
      event[key] = typeof value === "number" ? value : short(value);
    }
    queue.push(event);
  }

  function flush() {
    if (!queue.length || !navigator.sendBeacon) {
      queue = [];
      return;
    }
    while (queue.length) {
      var batch = [];
      var bytes = 100;
      while (queue.length && batch.length < MAX_BATCH) {
        var size = JSON.stringify(queue[0]).length + 1;
        if (batch.length && bytes + size > MAX_BATCH_BYTES) break;
        bytes += size;
        batch.push(queue.shift());
      }
      var body = JSON.stringify({ sid: sid, rid: rid, events: batch });
      // Sent once; a batch the browser refuses is dropped.
      navigator.sendBeacon(ENDPOINT, new Blob([body], { type: "application/json" }));
    }
  }

  function ignored(element) {
    return !element || !element.closest || !!element.closest('[data-telemetry="off"]');
  }

  window.fdcMark = safe(function (name) {
    record("mark", { name: name });
  });

  // The page view, once load timings are known.
  window.addEventListener(
    "load",
    safe(function () {
      setTimeout(
        safe(function () {
          var nav = performance.getEntriesByType("navigation")[0];
          var ref = "";
          if (document.referrer) {
            var from = new URL(document.referrer);
            ref = from.origin === location.origin ? from.pathname : from.host;
          }
          record("page_view", {
            ref: ref,
            ttfb: nav ? Math.round(nav.responseStart) : null,
            dcl: nav ? Math.round(nav.domContentLoadedEventEnd) : null,
            load: nav ? Math.round(nav.loadEventEnd) : null,
          });
        }),
        0,
      );
    }),
  );

  // Vitals: largest contentful paint, layout shift, and the longest
  // interaction, sent when the page is left.
  var lcp = null;
  var cls = 0;
  var inp = null;
  function observe(type, callback, options) {
    try {
      var observer = new PerformanceObserver(safe(function (list) {
        list.getEntries().forEach(callback);
      }));
      var settings = { type: type, buffered: true };
      for (var key in options) settings[key] = options[key];
      observer.observe(settings);
    } catch (e) {}
  }
  observe("largest-contentful-paint", function (entry) {
    lcp = Math.round(entry.startTime);
  });
  observe("layout-shift", function (entry) {
    if (!entry.hadRecentInput) cls += entry.value;
  });
  observe(
    "event",
    function (entry) {
      if (inp === null || entry.duration > inp) inp = Math.round(entry.duration);
    },
    { durationThreshold: 40 },
  );

  document.addEventListener(
    "click",
    safe(function (event) {
      var target = event.target;
      var element =
        target && target.closest && target.closest("button, a, [role=button], [data-track]");
      if (!element || ignored(element)) return;
      var named = element.matches("button, a, [role=button]") && !element.isContentEditable;
      record("click", {
        el: element.tagName.toLowerCase(),
        id: element.id,
        track: element.getAttribute("data-track"),
        text: named ? (element.textContent || "").replace(/\s+/g, " ").trim().slice(0, 40) : null,
      });
    }),
    true,
  );

  document.addEventListener(
    "submit",
    safe(function (event) {
      var form = event.target;
      if (ignored(form)) return;
      record("submit", { form: form.id });
    }),
    true,
  );

  // htmx 4 events: the session id goes on every request, and each request
  // is recorded with the id the server gave it.
  var started = new WeakMap();
  document.addEventListener(
    "htmx:config:request",
    safe(function (event) {
      var ctx = event.detail.ctx;
      ctx.request.headers["X-Session-Id"] = sid;
    }),
  );
  document.addEventListener(
    "htmx:before:request",
    safe(function (event) {
      started.set(event.detail.ctx, performance.now());
    }),
  );
  document.addEventListener(
    "htmx:finally:request",
    safe(function (event) {
      var ctx = event.detail.ctx;
      var start = started.get(ctx);
      if (start === undefined) return;
      var response = ctx.response;
      record("htmx", {
        verb: ctx.request.method,
        path: pathOf(ctx.request.action),
        status: response ? response.status : 0,
        ms: Math.round(performance.now() - start),
        rid: response && response.headers ? response.headers.get("X-Request-Id") : null,
      });
    }),
  );

  window.addEventListener(
    "error",
    safe(function (event) {
      if (!event.message) return;
      var src = event.filename ? pathOf(event.filename).split("/").pop() : "";
      record("js_error", { msg: event.message, src: src, line: event.lineno || null });
    }),
  );
  window.addEventListener(
    "unhandledrejection",
    safe(function (event) {
      var reason = event.reason;
      record("js_error", { msg: reason && reason.message ? reason.message : String(reason) });
    }),
  );

  setInterval(safe(flush), 10000);
  document.addEventListener(
    "visibilitychange",
    safe(function () {
      if (document.visibilityState === "hidden") flush();
    }),
  );
  var vitalsSent = false;
  window.addEventListener(
    "pagehide",
    safe(function () {
      if (!vitalsSent && (lcp !== null || cls || inp !== null)) {
        vitalsSent = true;
        record("vitals", { lcp: lcp, cls: Math.round(cls * 10000) / 10000, inp: inp });
      }
      flush();
    }),
  );
})();
