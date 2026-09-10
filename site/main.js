/* CCHarness 官网共享脚本：主题切换 / 移动端菜单 / 滚动显现 / 目录高亮 */
(function () {
  "use strict";

  /* ---------- 主题切换 ---------- */
  var root = document.documentElement;
  var toggle = document.getElementById("themeToggle");
  if (toggle) {
    toggle.addEventListener("click", function () {
      var next = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
      root.setAttribute("data-theme", next);
      try { localStorage.setItem("cch-theme", next); } catch (e) {}
    });
  }

  /* ---------- 移动端菜单 ---------- */
  var burger = document.getElementById("navBurger");
  if (burger) {
    burger.addEventListener("click", function () {
      document.body.classList.toggle("nav-open");
    });
    document.querySelectorAll(".nav-links a").forEach(function (a) {
      a.addEventListener("click", function () {
        document.body.classList.remove("nav-open");
      });
    });
  }

  /* ---------- 滚动显现 ---------- */
  if ("IntersectionObserver" in window) {
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (e) {
          if (e.isIntersecting) {
            e.target.classList.add("in");
            io.unobserve(e.target);
          }
        });
      },
      { threshold: 0.12 }
    );
    document.querySelectorAll(".reveal").forEach(function (el) { io.observe(el); });

    /* ---------- 文档页目录高亮（scroll-spy） ---------- */
    var tocLinks = document.querySelectorAll(".toc a[href^='#']");
    if (tocLinks.length) {
      var map = {};
      tocLinks.forEach(function (a) {
        var id = a.getAttribute("href").slice(1);
        var h = document.getElementById(id);
        if (h) map[id] = a;
      });
      var spy = new IntersectionObserver(
        function (entries) {
          entries.forEach(function (e) {
            var link = map[e.target.id];
            if (!link) return;
            if (e.isIntersecting) {
              tocLinks.forEach(function (a) { a.classList.remove("on"); });
              link.classList.add("on");
            }
          });
        },
        { rootMargin: "-80px 0px -66% 0px" }
      );
      Object.keys(map).forEach(function (id) { spy.observe(document.getElementById(id)); });
    }
  } else {
    document.querySelectorAll(".reveal").forEach(function (el) { el.classList.add("in"); });
  }

  /* ---------- 年份 ---------- */
  var y = document.getElementById("cchYear");
  if (y) y.textContent = String(new Date().getFullYear());
})();
