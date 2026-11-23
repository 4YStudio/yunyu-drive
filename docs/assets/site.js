/* ============================================================
   云语聚合网盘 · 官网交互
   只有四件事：主题切换、截图切换、滚动进入、顶栏吸顶描边。
   不引任何框架，也不发任何网络请求。
   ============================================================ */

(function () {
  "use strict";

  var root = document.documentElement;
  var THEME_KEY = "yunyu.site.theme";

  /* ------------------------------------------------------------ 主题 --- */
  var toggle = document.getElementById("theme-toggle");

  function applyTheme(theme) {
    root.dataset.theme = theme;
    // 顶栏与 hero 的 mark 有两套配色，切主题时一起换。
    document.querySelectorAll(".brand-mark").forEach(function (img) {
      img.src = theme === "dark" ? "./assets/mark.png" : "./assets/mark-ink.png";
    });
  }

  if (toggle) {
    toggle.addEventListener("click", function () {
      var next = root.dataset.theme === "dark" ? "light" : "dark";
      applyTheme(next);
      try {
        localStorage.setItem(THEME_KEY, next);
      } catch (e) {
        /* 隐私模式：不记住，但本次仍然生效 */
      }
    });
  }

  // 用户没手动选过时，跟随系统变化。
  var mq = window.matchMedia("(prefers-color-scheme: dark)");
  var onSystemTheme = function (ev) {
    var saved = null;
    try {
      saved = localStorage.getItem(THEME_KEY);
    } catch (e) {
      /* ignore */
    }
    if (!saved) applyTheme(ev.matches ? "dark" : "light");
  };
  if (mq.addEventListener) mq.addEventListener("change", onSystemTheme);
  else if (mq.addListener) mq.addListener(onSystemTheme);

  applyTheme(root.dataset.theme === "dark" ? "dark" : "light");

  /* ------------------------------------------------------------ 截图 --- */
  var VIEWS = {
    files: {
      caption: "<b>文件列表</b><span>多级目录、全盘搜索、网格 / 紧凑两种密度、右键菜单与多选。</span>",
    },
    transfers: {
      caption:
        "<b>传输列表</b><span>上传 / 下载 / 跨盘任务同一个队列，可暂停、重试、清理，速度与剩余时间实时刷新。</span>",
    },
    share: {
      caption:
        "<b>分享链接</b><span>粘贴链接即可预览、逐级浏览子目录、批量转存到自己网盘或下载到本地。</span>",
    },
    settings: {
      caption:
        "<b>设置</b><span>默认下载目录、并发数、全局速度上限、主题与跟随系统。</span>",
    },
  };

  var shot = document.getElementById("shot");
  var caption = document.getElementById("shot-caption");
  var viewButtons = document.querySelectorAll("#view-switch button");
  var themeButtons = document.querySelectorAll("#shot-theme button");

  var state = { view: "files", theme: "light" };

  function renderShot() {
    if (!shot) return;
    shot.src = "./" + state.view + "-" + state.theme + ".png";
    shot.alt =
      ({ files: "文件列表", transfers: "传输列表", share: "分享链接", settings: "设置" }[
        state.view
      ] || "") + "界面";
    if (caption && VIEWS[state.view]) caption.innerHTML = VIEWS[state.view].caption;
    viewButtons.forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.dataset.view === state.view));
    });
    themeButtons.forEach(function (b) {
      b.setAttribute("aria-pressed", String(b.dataset.shotTheme === state.theme));
    });
  }

  viewButtons.forEach(function (b) {
    b.addEventListener("click", function () {
      state.view = b.dataset.view;
      renderShot();
    });
  });
  themeButtons.forEach(function (b) {
    b.addEventListener("click", function () {
      state.theme = b.dataset.shotTheme;
      renderShot();
    });
  });

  // 截图默认跟当前页面主题走，用户单独点过就不再跟随。
  var shotThemeTouched = false;
  themeButtons.forEach(function (b) {
    b.addEventListener("click", function () {
      shotThemeTouched = true;
    });
  });
  state.theme = root.dataset.theme === "dark" ? "dark" : "light";
  renderShot();

  if (toggle) {
    toggle.addEventListener("click", function () {
      if (shotThemeTouched) return;
      state.theme = root.dataset.theme === "dark" ? "dark" : "light";
      renderShot();
    });
  }

  /* -------------------------------------------------------- 滚动进入 --- */
  var revealables = document.querySelectorAll(".reveal");
  if ("IntersectionObserver" in window && revealables.length) {
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            entry.target.classList.add("is-in");
            io.unobserve(entry.target);
          }
        });
      },
      { rootMargin: "0px 0px -8% 0px", threshold: 0.08 }
    );
    revealables.forEach(function (el) {
      io.observe(el);
    });
  } else {
    revealables.forEach(function (el) {
      el.classList.add("is-in");
    });
  }

  /* ------------------------------------------------------ 顶栏吸顶线 --- */
  var topbar = document.getElementById("topbar");
  if (topbar) {
    var onScroll = function () {
      topbar.classList.toggle("is-stuck", window.scrollY > 4);
    };
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
  }
})();
