// rust-afariano: small progressive enhancements, no framework.
(function () {
  "use strict";

  // Mobile sidebar drawer.
  document.addEventListener("click", function (e) {
    if (e.target.closest("[data-nav-toggle]")) {
      document.body.classList.toggle("nav-open");
    } else if (e.target.closest(".scrim")) {
      document.body.classList.remove("nav-open");
    }
  });

  // Copy buttons: data-copy="element id".
  document.addEventListener("click", function (e) {
    var btn = e.target.closest("[data-copy]");
    if (!btn) return;
    var el = document.getElementById(btn.getAttribute("data-copy"));
    if (!el) return;
    var text = el.value !== undefined ? el.value : el.textContent;
    // Secrets are copied byte for byte; commands lose surrounding whitespace.
    if (!el.hasAttribute("data-exact")) text = text.trim();
    navigator.clipboard.writeText(text).then(function () {
      var label = btn.querySelector("[data-label]");
      if (label) {
        var old = label.textContent;
        label.textContent = "Copied";
        setTimeout(function () { label.textContent = old; }, 1200);
      } else {
        btn.classList.add("ok");
        setTimeout(function () { btn.classList.remove("ok"); }, 1200);
      }
    });
  });

  // Close an open dropdown when clicking elsewhere or pressing Escape.
  document.addEventListener("click", function (e) {
    document.querySelectorAll("details.dropdown[open]").forEach(function (d) {
      if (!d.contains(e.target)) d.removeAttribute("open");
    });
  });
  document.addEventListener("keydown", function (e) {
    if (e.key !== "Escape") return;
    document.querySelectorAll("details.dropdown[open]").forEach(function (d) { d.removeAttribute("open"); });
    document.body.classList.remove("nav-open");
  });

  // CI/CD variables: Show fetches one value and puts it in place, Hide takes it
  // off the page again. Without JavaScript the form opens it as plain text.
  document.addEventListener("submit", function (e) {
    var form = e.target;
    if (!form.hasAttribute("data-reveal") || e.defaultPrevented) return;
    e.preventDefault();
    var cell = form.closest("[data-secret]");
    var btn = form.querySelector("button");
    btn.disabled = true;
    fetch(form.action, { method: "POST", credentials: "same-origin", cache: "no-store" })
      .then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.text();
      })
      .then(function (text) {
        cell.querySelector(".secret-value").textContent = text;
        cell.querySelector("[data-secret-hidden]").hidden = true;
        cell.querySelector("[data-secret-shown]").hidden = false;
      })
      .catch(function (err) { window.alert("Could not load the value (" + err.message + ")."); })
      .then(function () { btn.disabled = false; });
  });
  document.addEventListener("click", function (e) {
    var hide = e.target.closest("[data-secret-hide]");
    if (!hide) return;
    var cell = hide.closest("[data-secret]");
    cell.querySelector(".secret-value").textContent = "";
    cell.querySelector("[data-secret-shown]").hidden = true;
    cell.querySelector("[data-secret-hidden]").hidden = false;
  });

  // Run pipeline: entering another existing branch or tag reloads the form with
  // the variables of that ref's .gitlab-ci.yml. A name that is not in the
  // suggestions is left alone (the server says so on submit).
  // Typing a name and clicking Run (or pressing Enter) submits instead: the
  // server then shows that ref's variables first.
  var reloadTimer = null;
  document.addEventListener("change", function (e) {
    var el = e.target.closest("[data-reload-with]");
    if (!el) return;
    var value = el.value.trim();
    if (!value || value === el.getAttribute("data-current")) return;
    if (el.list && !Array.prototype.some.call(el.list.options, function (o) { return o.value === value; })) return;
    var url = new URL(window.location.href);
    url.searchParams.set(el.getAttribute("data-reload-with"), value);
    clearTimeout(reloadTimer);
    reloadTimer = setTimeout(function () { window.location.assign(url.toString()); }, 300);
  });
  document.addEventListener("submit", function () {
    clearTimeout(reloadTimer);
    reloadTimer = null;
  }, true);

  // Confirm destructive forms.
  document.addEventListener("submit", function (e) {
    var msg = e.target.getAttribute("data-confirm");
    if (msg && !window.confirm(msg)) e.preventDefault();
  });

  // ------------------------------------------------------------- live pages
  // A page with data-live listens to that event stream. On each "change" it
  // fetches itself and swaps in the data-live-region parts that differ; when a
  // data-live-key differs (a manual job started, say) it reloads instead.
  // data-live-seq is the server's change counter when the page was rendered: the
  // stream pings at once if anything changed since. Hidden tabs drop the stream
  // (over HTTP/1.1 browsers allow six connections per host) and catch up when
  // shown again.
  var Live = { connect: function () {}, refresh: function () {} };
  var liveEl = document.querySelector("[data-live]");
  if (liveEl) Live = (function () {
    var base = liveEl.getAttribute("data-live");
    var seq = liveEl.getAttribute("data-live-seq");
    var es = null, wanted = false, retry = 1000, retryTimer = null;
    var timer = null, busy = false, again = false, dirty = false, last = 0, stopped = false;

    function close() {
      clearTimeout(retryTimer);
      if (es) { es.close(); es = null; }
    }
    function connect() {
      wanted = true;
      if (es || stopped || document.hidden) return;
      clearTimeout(retryTimer);
      es = new EventSource(base + (base.indexOf("?") < 0 ? "?" : "&") + "since=" + encodeURIComponent(seq));
      es.addEventListener("open", function () { retry = 1000; });
      es.addEventListener("change", refresh);
      es.addEventListener("error", function () {
        // Reconnect ourselves, so the stream hears the counter of what is on screen.
        close();
        retryTimer = setTimeout(connect, retry);
        retry = Math.min(retry * 2, 60000);
      });
    }

    function refresh() {
      if (stopped) return;
      if (document.hidden) { dirty = true; return; }
      if (busy) { again = true; return; }
      if (timer) return;
      // A burst of changes (one job ends, the next starts) becomes one fetch.
      timer = setTimeout(run, Math.max(150, last + 1000 - Date.now()));
    }
    function run() {
      timer = null;
      if (document.hidden) { dirty = true; return; }
      busy = true;
      last = Date.now();
      fetch(window.location.href, { credentials: "same-origin", cache: "no-store" })
        .then(function (r) {
          // Signed out (sent to sign-in), gone or no longer allowed: leave the page be.
          if (new URL(r.url).pathname !== window.location.pathname || (r.status >= 400 && r.status < 500)) {
            throw "stop";
          }
          if (!r.ok) throw new Error("HTTP " + r.status);
          return r.text();
        })
        .then(apply)
        .catch(function (e) {
          if (e === "stop") { stopped = true; close(); }
          else setTimeout(refresh, 5000); // a restart, most likely
        })
        .then(function () {
          busy = false;
          if (again) { again = false; refresh(); }
        });
    }
    function apply(html) {
      var doc = new DOMParser().parseFromString(html, "text/html");
      var mark = doc.querySelector("[data-live]");
      if (!mark) throw "stop";
      var keys = document.querySelectorAll("[data-live-key]");
      for (var k = 0; k < keys.length; k++) {
        var key = doc.getElementById(keys[k].id);
        if (!key || key.getAttribute("data-live-key") !== keys[k].getAttribute("data-live-key")) {
          stopped = true;
          window.location.reload();
          return;
        }
      }
      var olds = document.querySelectorAll("[data-live-region]");
      var news = [];
      for (var i = 0; i < olds.length; i++) {
        news.push(doc.getElementById(olds[i].id));
        if (!news[i]) { stopped = true; window.location.reload(); return; }
      }
      for (i = 0; i < olds.length; i++) {
        if (olds[i].outerHTML !== news[i].outerHTML) olds[i].replaceWith(document.adoptNode(news[i]));
      }
      if (doc.title) document.title = doc.title;
      seq = mark.getAttribute("data-live-seq");
      tick();
    }

    document.addEventListener("visibilitychange", function () {
      if (document.hidden) { close(); return; }
      if (dirty) { dirty = false; refresh(); }
      if (wanted) connect();
    });
    return { connect: connect, refresh: refresh };
  })();

  // Running durations keep counting between changes (data-elapsed: seconds so
  // far when rendered). Same format as the server's.
  function pad2(n) { return n < 10 ? "0" + n : "" + n; }
  function duration(s) {
    s = Math.max(0, Math.floor(s));
    if (s < 60) return s + "s";
    if (s < 3600) return Math.floor(s / 60) + "m " + pad2(s % 60) + "s";
    return Math.floor(s / 3600) + "h " + pad2(Math.floor((s % 3600) / 60)) + "m";
  }
  function tick() {
    var now = Date.now();
    document.querySelectorAll("[data-elapsed]").forEach(function (el) {
      if (el._t0 === undefined) el._t0 = now - parseInt(el.getAttribute("data-elapsed"), 10) * 1000;
      var text = duration((now - el._t0) / 1000);
      if (el.textContent !== text) el.textContent = text;
    });
  }
  // Often enough that a second never shows late; the DOM changes once a second.
  if (liveEl) setInterval(tick, 250);

  // ------------------------------------------------------------- job log
  // ANSI SGR colours/bold into spans, one <div class="line"> per log line
  // (numbered by CSS), "\r" redraws the current line like a terminal.
  var log = document.getElementById("log");
  if (log && log.dataset.stream) {
    var sgr = { bold: false, fg: 0 };
    var line = null;
    var pendingCR = false;
    var follow = true;
    var waiting = log.querySelector(".waiting");

    function newLine() {
      line = document.createElement("div");
      line.className = "line";
      log.appendChild(line);
    }
    function addText(text) {
      if (!text) return;
      if (!line) newLine();
      if (pendingCR) { line.textContent = ""; pendingCR = false; }
      var span = document.createElement("span");
      var cls = [];
      if (sgr.bold) cls.push("b");
      if (sgr.fg) cls.push("f" + sgr.fg);
      if (cls.length) span.className = cls.join(" ");
      span.textContent = text;
      line.appendChild(span);
    }
    function addPlain(text) {
      var parts = text.split(/(\r\n|\n|\r)/);
      for (var i = 0; i < parts.length; i++) {
        var p = parts[i];
        if (p === "\n" || p === "\r\n") { if (!line) newLine(); line = null; pendingCR = false; }
        else if (p === "\r") { pendingCR = true; }
        else addText(p);
      }
    }
    function feed(text) {
      var re = /\x1b\[([0-9;]*)m|\x1b\[[0-9;?]*[A-Za-z]/g;
      var last = 0, m;
      while ((m = re.exec(text))) {
        addPlain(text.slice(last, m.index));
        last = re.lastIndex;
        if (m[1] === undefined) continue;
        var codes = m[1] === "" ? ["0"] : m[1].split(";");
        for (var i = 0; i < codes.length; i++) {
          var c = parseInt(codes[i], 10);
          if (c === 0 || isNaN(c)) { if (codes[i] === "" && i > 0) continue; sgr.bold = false; sgr.fg = 0; }
          else if (c === 1) sgr.bold = true;
          else if (c === 22) sgr.bold = false;
          else if ((c >= 30 && c <= 37) || (c >= 90 && c <= 97)) sgr.fg = c;
          else if (c === 39) sgr.fg = 0;
          else if (c === 38) i += codes[i + 1] === "5" ? 2 : 4;
        }
      }
      addPlain(text.slice(last));
    }

    log.addEventListener("scroll", function () {
      follow = log.scrollTop + log.clientHeight >= log.scrollHeight - 30;
    });
    var bottom = document.getElementById("log-bottom");
    if (bottom) bottom.addEventListener("click", function () { follow = true; log.scrollTop = log.scrollHeight; });

    var es = new EventSource(log.dataset.stream);
    es.addEventListener("log", function (ev) {
      if (waiting) { waiting.remove(); waiting = null; }
      feed(JSON.parse(ev.data).t);
      if (follow) log.scrollTop = log.scrollHeight;
    });
    // The page's one connection: it also carries the pipeline's changes until the
    // job ends, then the page's own stream takes over.
    es.addEventListener("change", Live.refresh);
    es.addEventListener("end", function () {
      es.close();
      Live.connect();
    });
  } else {
    Live.connect();
  }
})();
