// A request that gets no reply (the network failed, or it took longer than
// htmx's 10 s limit) shows a message and a retry in its target, if the
// target has data-load-error. Replies, errors included, are swapped in as
// the server sent them.
(function () {
  var latest = new WeakMap();
  document.addEventListener("htmx:before:request", function (event) {
    var ctx = event.detail.ctx;
    if (ctx.target) latest.set(ctx.target, ctx);
  });
  document.addEventListener("htmx:error", function (event) {
    var ctx = event.detail.ctx;
    var target = ctx && ctx.target;
    if (!target || !target.isConnected || !target.hasAttribute("data-load-error")) return;
    // A newer request for the same target replaced this one.
    if (latest.get(target) !== ctx) return;
    var message = document.createElement("p");
    message.textContent = target.getAttribute("data-load-error");
    var retry = document.createElement("button");
    retry.type = "button";
    retry.className = "button is-small";
    retry.textContent = "Try again";
    retry.addEventListener("click", function () {
      var source = ctx.sourceElement && ctx.sourceElement.isConnected ? ctx.sourceElement : target;
      htmx.ajax("GET", ctx.request.action, { source: source, target: target, swap: "innerHTML" });
    });
    var box = document.createElement("div");
    box.className = "load-error";
    box.setAttribute("role", "alert");
    box.append(message, retry);
    target.replaceChildren(box);
  });
})();
