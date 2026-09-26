// A request for a target marked with data-load-error that fails shows the
// target's message and a retry instead of the reply: when it gets no reply
// (the network failed, or it took longer than htmx's 10 s limit) and when
// the reply is an HTTP error, whatever its body says. Other targets get
// replies, errors included, as the server sent them.
(function () {
  var latest = new WeakMap();
  document.addEventListener("htmx:before:request", function (event) {
    var ctx = event.detail.ctx;
    if (ctx.target) latest.set(ctx.target, ctx);
  });

  // Whether ctx is the newest request for a target that shows load errors.
  function handles(ctx) {
    var target = ctx && ctx.target;
    if (!target || !target.isConnected || !target.hasAttribute("data-load-error")) return false;
    // A newer request for the same target replaced this one.
    return latest.get(target) === ctx;
  }

  function showError(ctx) {
    var target = ctx.target;
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
  }

  document.addEventListener("htmx:error", function (event) {
    var ctx = event.detail.ctx;
    if (handles(ctx)) showError(ctx);
  });

  // An error reply is not swapped in: cancelling this event ends the request.
  document.addEventListener("htmx:after:request", function (event) {
    var ctx = event.detail.ctx;
    if (!ctx || !ctx.response || ctx.response.status < 400 || !handles(ctx)) return;
    event.preventDefault();
    showError(ctx);
  });
})();
