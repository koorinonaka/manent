// Manent LP. The language is already decided by the inline script in <head> (html[lang]);
// this file adds the JA / EN switch, the showreel, the "What it does" tabs, the entrance
// motion and the download link. The page is two trees, one per language (.lp[lang]).
(function () {
  'use strict';
  var root = document.documentElement;
  var reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  var HEAD = {
    ja: { title: 'Manent | 撮った画面が、その場に残る', description: '撮った画面をファイルにせず、撮った位置に等倍で残す Windows 11 のピン留めキャプチャツール。GIF のピン、その場での書き込み、Ctrl で部品を選ぶ撮り方に対応します。' },
    en: { title: 'Manent | Your capture stays where you took it', description: 'A pinning screen capture tool for Windows 11. Captures stay on screen where you took them, at actual size. GIF pins, in-place annotation, and Ctrl to pick a window or control.' }
  };
  var tree = function (l) { return document.querySelector('.lp[lang="' + l + '"]'); };

  // ---------- Japanese line breaks where word-break: auto-phrase is missing (Safari, Firefox) ----------
  // Without it a line may break inside a word (写 / ります). Allow breaks only before a segment that starts
  // with a non-hiragana word, so particles and endings stay with the word before them; keep-all does the rest.
  var noPhrase = !(window.CSS && CSS.supports('word-break', 'auto-phrase'));
  if (noPhrase && window.Intl && Intl.Segmenter && tree('ja')) {
    var seg = new Intl.Segmenter('ja', { granularity: 'word' });
    tree('ja').querySelectorAll('p, h1, h2, h3, dt, dd, li').forEach(function (el) {
      if (el.closest('[data-ui]')) return;
      var walk = document.createTreeWalker(el, NodeFilter.SHOW_TEXT), nodes = [];
      while (walk.nextNode()) if (/[぀-ヿ一-鿿]/.test(walk.currentNode.data)) nodes.push(walk.currentNode);
      nodes.forEach(function (n) {
        var frag = document.createDocumentFragment(), buf = '';
        Array.from(seg.segment(n.data)).forEach(function (s) {
          if (buf && s.isWordLike && /^[^぀-ゟー]/.test(s.segment) && !/[「（(\s]$/.test(buf)) {
            frag.append(buf, document.createElement('wbr'));
            buf = '';
          }
          buf += s.segment;
        });
        frag.append(buf);
        n.replaceWith(frag);
      });
      el.style.wordBreak = 'keep-all';
      el.style.overflowWrap = 'anywhere';
    });
  }

  // ---------- Showreel ----------
  // Only the visible tree's <video> gets a src, so the hidden language is never downloaded.
  function reelOf(l) { var t = tree(l); return t && t.querySelector('.lp-video'); }
  function startReel(l) {
    var v = reelOf(l);
    if (!v || v.getAttribute('src')) return;
    if (reduced) { v.removeAttribute('autoplay'); v.preload = 'metadata'; }
    v.src = v.getAttribute('data-src');
    if (!reduced) v.play().catch(function () {});
    syncButtons(v);
  }
  function stopReel(l) { var v = reelOf(l); if (v && !v.paused) v.pause(); }
  function syncButtons(v) {
    var fig = v.closest('.a-reelwrap') || v.parentNode;
    var pause = fig.querySelector('[data-reel="pause"]'), sound = fig.querySelector('[data-reel="sound"]');
    var play = fig.querySelector('[data-reel="play"]');
    var paused = v.paused;
    if (pause) {
      pause.classList.toggle('is-paused', paused);
      pause.setAttribute('aria-label', pause.getAttribute(paused ? 'data-play' : 'data-pause'));
    }
    if (sound) {
      sound.setAttribute('aria-pressed', String(!v.muted));
      sound.querySelector('.lp-rl').textContent = sound.getAttribute(v.muted ? 'data-on' : 'data-off');
    }
    // Reduced motion: the poster stays until 再生 is pressed.
    if (play) play.hidden = !(reduced && paused && !v.hasAttribute('data-started'));
  }
  document.querySelectorAll('.lp-video').forEach(function (v) {
    ['play', 'pause', 'volumechange'].forEach(function (e) { v.addEventListener(e, function () { syncButtons(v); }); });
    // The English reel is optional: if it cannot be read, show the Japanese one.
    v.addEventListener('error', function () {
      var ja = 'assets/showreel-ja.mp4';
      if (v.getAttribute('src') && v.getAttribute('src') !== ja) {
        v.src = ja;
        if (!reduced || v.hasAttribute('data-started')) v.play().catch(function () {});
      }
    });
    var fig = v.closest('.a-reelwrap') || v.parentNode;
    fig.addEventListener('click', function (e) {
      var b = e.target.closest('[data-reel]');
      if (!b) return;
      var k = b.getAttribute('data-reel');
      if (k === 'play' || k === 'pause') {
        if (v.paused) { v.setAttribute('data-started', ''); v.play().catch(function () {}); } else v.pause();
      } else if (k === 'sound') {
        v.muted = !v.muted;
        if (!v.muted && v.paused) { v.setAttribute('data-started', ''); v.play().catch(function () {}); }
      }
      syncButtons(v);
    });
    syncButtons(v);
  });

  // ---------- Language ----------
  function setLang(l, remember) {
    var old = root.lang === 'en' ? 'en' : 'ja';
    root.lang = l;
    document.title = HEAD[l].title;
    var d = document.querySelector('meta[name="description"]');
    if (d) d.setAttribute('content', HEAD[l].description);
    document.querySelectorAll('.lp-lang button').forEach(function (b) {
      b.setAttribute('aria-pressed', String(b.getAttribute('lang') === l));
    });
    if (remember) { try { localStorage.setItem('manent-lp-lang', l); } catch (e) {} }
    if (old !== l) stopReel(old);
    startReel(l);
  }
  document.querySelectorAll('.lp-lang button').forEach(function (b) {
    b.addEventListener('click', function () {
      setLang(b.getAttribute('lang'), true);
      var twin = tree(b.getAttribute('lang')).querySelector('.lp-lang button[lang="' + b.getAttribute('lang') + '"]');
      if (twin) twin.focus();
    });
  });
  setLang(root.lang === 'en' ? 'en' : 'ja', false);

  // ---------- What it does: the names switch the panel and the stage picture ----------
  document.querySelectorAll('.a-names').forEach(function (list) {
    var sec = list.closest('.a-index');
    var tabs = Array.prototype.slice.call(list.querySelectorAll('[role="tab"]'));
    function select(tab, focus) {
      tabs.forEach(function (t) {
        var on = t === tab;
        t.setAttribute('aria-selected', String(on));
        t.tabIndex = on ? 0 : -1;
        var panel = document.getElementById(t.getAttribute('aria-controls'));
        if (on && panel.hidden) { panel.classList.remove('is-in'); void panel.offsetWidth; panel.classList.add('is-in'); }
        panel.hidden = !on;
      });
      var n = tab.id.replace(/^a-tab-/, '').replace(/-en$/, '');
      sec.querySelectorAll('.a-pic').forEach(function (p) {
        var on = p.getAttribute('data-for') === n;
        if (on && p.hidden) { p.classList.remove('is-in'); void p.offsetWidth; p.classList.add('is-in'); }
        p.hidden = !on;
      });
      if (focus) tab.focus();
    }
    // Rotate every 5 s so the names read as switchable. The bar under the selected name (CSS) runs the
    // 5 s, and its animationend moves to the next name, so the bar and the switch never drift apart.
    // The bar pauses while the picture is off screen. When the visitor picks a name, the bar goes away
    // for 5 s (time to read the pick), then starts over under that name.
    var auto = false, wait = 0;
    function restart() {
      if (!auto) return;
      list.classList.remove('is-auto');
      clearTimeout(wait);
      wait = setTimeout(function () { list.classList.add('is-auto'); }, 5000);
    }
    tabs.forEach(function (t, i) {
      t.addEventListener('click', function () { restart(); select(t, false); });
      t.addEventListener('keydown', function (e) {
        var j = { ArrowDown: i + 1, ArrowRight: i + 1, ArrowUp: i - 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
        if (j === undefined) return;
        e.preventDefault();
        restart();
        select(tabs[(j + tabs.length) % tabs.length], true);
      });
    });
    if (reduced || !('IntersectionObserver' in window)) return;
    auto = true;
    list.classList.add('is-auto', 'is-paused');
    // Watch the picture's box, not the section: the section (and on a PC the sticky stage) is taller than the screen.
    new IntersectionObserver(function (es) { list.classList.toggle('is-paused', !es[0].isIntersecting); }, { threshold: 0.4 })
      .observe(sec.querySelector('.a-stage-in') || sec);
    list.addEventListener('animationend', function (e) {
      if (e.animationName !== 'a-tab-time' || !list.classList.contains('is-auto')) return;
      var i = tabs.indexOf(e.target);
      select(tabs[(i + 1) % tabs.length], false);
    });
  });

  // ---------- 1023 px and below: shrink each picture to its box instead of cropping it ----------
  var narrow = matchMedia('(max-width: 1023px)');
  // The box's width does not depend on the zoom, so the zoom is only written when it changes: a rewrite re-lays the page out.
  var fitW = 0;
  var fit = function () {
    // iOS Safari fires resize whenever its toolbar shows or hides (it does at the page's end): only a new width matters
    if (innerWidth === fitW) return;
    fitW = innerWidth;
    document.querySelectorAll('.a-sec .a-stage .lp-frag').forEach(function (f) {
      var z = '';
      if (narrow.matches) {
        // the pic is its own box only at 767 px and below, where the stage dissolves (display: contents)
        var s = f.closest('.a-pic') || f.closest('.a-stage'), cs = getComputedStyle(s);
        if (!s.clientWidth) s = f.closest('.a-stage'), cs = getComputedStyle(s);
        var k = (s.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight)) / parseFloat(f.style.width);
        if (k > 0 && k < 1) z = String(+k.toFixed(4)); // +: style.zoom reads 0.6250 back as 0.625
      }
      if (f.style.zoom !== z) f.style.zoom = z;
    });
  };
  fit();
  addEventListener('resize', fit);

  // ---------- Motion: entrances (once) and the wordmark shrinking with the scroll ----------
  if (!reduced && 'IntersectionObserver' in window) {
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) { if (e.isIntersecting) { e.target.classList.add('is-shown'); io.unobserve(e.target); } });
    }, { rootMargin: '0px 0px -10% 0px' });
    document.querySelectorAll('.a-frow, .a-index, .a-close').forEach(function (sec) {
      sec.querySelectorAll('.a-h2, h3:not(.a-panel-t), .a-frow p, .a-stage > .lp-frag, .a-stage-in > .lp-frag, .a-names, .lp-rows, .lp-note, .a-close-line, .a-close-words > p').forEach(function (el, i) {
        el.classList.add('is-rv');
        el.style.setProperty('--rv-d', (i % 4) * 75 + 'ms');
        io.observe(el);
      });
    });
  }
  if (!reduced) {
    var words = document.querySelectorAll('.a-word'), ticking = false;
    var shrink = function () {
      ticking = false;
      var k = 1 - 0.06 * Math.min(1, Math.max(0, scrollY / innerHeight));
      words.forEach(function (w) { w.style.transform = 'scale(' + k.toFixed(4) + ')'; });
    };
    addEventListener('scroll', function () { if (!ticking) { ticking = true; requestAnimationFrame(shrink); } }, { passive: true });
  }

  // ---------- Download: the latest Manent-Setup-*.exe; on any failure the links stay on releases/latest ----------
  if (window.fetch) {
    fetch('https://api.github.com/repos/koorinonaka/manent/releases/latest', { headers: { Accept: 'application/vnd.github+json' } })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (rel) {
        if (!rel || !rel.tag_name || !rel.assets) return;
        var find = function (re) { return rel.assets.filter(function (a) { return re.test(a.name) && a.browser_download_url; })[0]; };
        var exe = find(/^Manent-Setup-.*\.exe$/), zip = find(/-portable\.zip$/);
        if (!exe) return;
        var meta = rel.tag_name.replace(/^v?/, 'v') + ' · ' + (exe.size / 1e6).toFixed(1) + ' MB · Windows 11 x64';
        document.querySelectorAll('a[data-dl]').forEach(function (a) {
          a.href = exe.browser_download_url;
          var m = a.querySelector('.lp-dl-m');
          if (m) m.textContent = meta;
        });
        if (zip) document.querySelectorAll('a[data-dl-zip]').forEach(function (a) { a.href = zip.browser_download_url; });
      })
      .catch(function () {});
  }
})();
