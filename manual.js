// Manent manual. The language is already decided by the inline script in <head> (html[lang]);
// this file adds the JA / EN switch (shared with the LP through localStorage), the contents' current
// section, the contents bar below 1024 px, and the pause button of the moving figures.
// The page is two trees, one per language (.m[lang]); English ids are the Japanese ones plus "-en".
(function () {
  'use strict';
  var root = document.documentElement;
  var reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  var tree = function (l) { return document.querySelector('.m[lang="' + l + '"]'); };
  var cur = function () { return root.lang === 'en' ? 'en' : 'ja'; };
  var twinId = function (id, l) { var base = id.replace(/-en$/, ''); return l === 'en' ? base + '-en' : base; };

  // ---------- Japanese line breaks where word-break: auto-phrase is missing (Safari, Firefox) ----------
  // The same as the LP's main.js: allow breaks only before a segment that starts with a non-hiragana word.
  var noPhrase = !(window.CSS && CSS.supports('word-break', 'auto-phrase'));
  if (noPhrase && window.Intl && Intl.Segmenter && tree('ja')) {
    var seg = new Intl.Segmenter('ja', { granularity: 'word' });
    tree('ja').querySelectorAll('p, h1, h2, h3, dt, dd, li, td').forEach(function (el) {
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

  // ---------- Current section (the settings window's nav rule) ----------
  // The clicked section gets the mark. On scroll: the last heading at or above the top of the view;
  // at the very bottom, the last heading.
  var heads = {}, marked = null, lock = null;
  ['ja', 'en'].forEach(function (l) { var t = tree(l); heads[l] = t ? Array.prototype.slice.call(t.querySelectorAll('.m-main h2[id], .m-main h3[id]')) : []; });
  function mark(id) {
    var t = tree(cur());
    if (!t || id === marked) return;
    marked = id;
    var h = document.getElementById(id), chId = h && h.closest('.m-ch').getAttribute('aria-labelledby');
    t.querySelectorAll('.m-toc').forEach(function (nav) {
      nav.querySelectorAll('a[aria-current]').forEach(function (a) { a.removeAttribute('aria-current'); });
      nav.querySelectorAll('li[data-ch]').forEach(function (li) { li.classList.toggle('is-open', li.getAttribute('data-ch') === chId); });
      var a = nav.querySelector('a[href="#' + id + '"]');
      if (a) a.setAttribute('aria-current', 'true');
    });
    var c = t.querySelector('.m-tocm-c');
    if (c && h) c.textContent = h.textContent;
  }
  function spyTop() { return parseFloat(getComputedStyle(heads[cur()][0] || root).scrollMarginTop) || 0; }
  function spy() {
    if (lock) return;
    var list = heads[cur()];
    if (!list.length) return;
    var se = document.scrollingElement, top = spyTop() + 1, pick = list[0];
    if (se.scrollTop + innerHeight >= se.scrollHeight - 2) pick = list[list.length - 1];
    else for (var i = 0; i < list.length; i++) { if (list[i].getBoundingClientRect().top <= top) pick = list[i]; else break; }
    mark(pick.id);
  }
  var ticking = false;
  addEventListener('scroll', function () { if (!ticking) { ticking = true; requestAnimationFrame(function () { ticking = false; spy(); }); } }, { passive: true });
  addEventListener('resize', spy);
  var unlock = function () { lock = null; };
  addEventListener('scrollend', unlock);
  ['wheel', 'touchstart', 'keydown'].forEach(function (e) { addEventListener(e, unlock, { passive: true }); });

  document.querySelectorAll('.m-toc a').forEach(function (a) {
    a.addEventListener('click', function () {
      var id = a.getAttribute('href').slice(1);
      mark(id);
      lock = id;
      setTimeout(function () { if (lock === id) unlock(); }, 1200); // scrollend is missing in some browsers
      var d = a.closest('details');
      if (d) d.open = false;
    });
  });
  // The bar closes on a click outside it and on Esc.
  document.addEventListener('click', function (e) {
    document.querySelectorAll('.m-tocm[open]').forEach(function (d) { if (!d.contains(e.target)) d.open = false; });
  });
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'Escape') return;
    document.querySelectorAll('.m-tocm[open]').forEach(function (d) { d.open = false; d.querySelector('summary').focus(); });
  });

  // ---------- Moving figures ----------
  // The page loads the posters. A GIF plays only while it is on screen and not paused; off screen it goes
  // back to the poster. Reduced motion starts every GIF paused.
  var figs = Array.prototype.slice.call(document.querySelectorAll('.m-fig.is-gif')).map(function (f) {
    var img = f.querySelector('img'), btn = f.querySelector('.m-fig-btn');
    return { f: f, img: img, btn: btn, poster: img.getAttribute('src'), gif: img.getAttribute('data-gif'), paused: reduced, seen: false };
  });
  function sync(g) {
    var play = g.seen && !g.paused, want = play ? g.gif : g.poster;
    if (g.img.getAttribute('src') !== want) g.img.setAttribute('src', want);
    g.btn.classList.toggle('is-paused', !play && g.paused);
    g.btn.setAttribute('aria-label', g.btn.getAttribute(g.paused ? 'data-play' : 'data-pause'));
  }
  figs.forEach(function (g) {
    g.btn.hidden = false;
    g.btn.addEventListener('click', function () { g.paused = !g.paused; sync(g); });
    sync(g);
  });
  // Every figure but the head's rises in once, the first time it comes into view.
  var rise = [];
  if (!reduced) {
    document.querySelectorAll('.m-main .m-ch .m-fig').forEach(function (f) {
      if (f.getBoundingClientRect().top > innerHeight) { f.classList.add('is-pre'); rise.push(f); }
    });
  }
  if ('IntersectionObserver' in window) {
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) {
        e.target.classList.remove('is-pre');
        var g = figs.filter(function (x) { return x.f === e.target; })[0];
        if (g) { g.seen = e.isIntersecting; sync(g); }
      });
    }, { rootMargin: '0px 0px -8% 0px' });
    figs.forEach(function (g) { io.observe(g.f); });
    rise.forEach(function (f) { io.observe(f); });
  } else {
    figs.forEach(function (g) { g.seen = true; sync(g); });
    rise.forEach(function (f) { f.classList.remove('is-pre'); });
  }

  // ---------- Language ----------
  // Switching keeps the reader where they were: the current heading of the old tree is put at the same
  // height in the new one. A hash that points into the hidden tree is moved to its twin.
  function go(id, y, hash) {
    var h = document.getElementById(id);
    if (!h) return;
    lock = null;
    marked = null;
    scrollTo({ top: h.getBoundingClientRect().top + scrollY - y, behavior: 'instant' });
    if (hash) history.replaceState(null, '', '#' + id);
    mark(id);
  }
  function setLang(l, remember) {
    var old = cur(), keep = old !== l && marked && document.getElementById(marked), y = keep ? keep.getBoundingClientRect().top : 0;
    root.lang = l;
    var t = tree(l);
    if (t) {
      document.title = t.getAttribute('data-title');
      var d = document.querySelector('meta[name="description"]');
      if (d) d.setAttribute('content', t.getAttribute('data-desc'));
    }
    document.querySelectorAll('.m-lang button').forEach(function (b) { b.setAttribute('aria-pressed', String(b.getAttribute('lang') === l)); });
    if (remember) { try { localStorage.setItem('manent-lp-lang', l); } catch (e) {} }
    if (keep && scrollY > 0) go(twinId(keep.id, l), y, !!location.hash);
    else { marked = null; spy(); }
  }
  document.querySelectorAll('.m-lang button').forEach(function (b) {
    b.addEventListener('click', function () {
      var l = b.getAttribute('lang');
      setLang(l, true);
      var twin = tree(l).querySelector('.m-lang button[lang="' + l + '"]');
      if (twin && twin.offsetParent) twin.focus({ preventScroll: true });
    });
  });
  setLang(cur(), false);
  function fixHash() {
    var id = decodeURIComponent(location.hash.slice(1)), el = id && document.getElementById(id);
    if (!el || el.closest('.m').getAttribute('lang') === cur()) return;
    go(twinId(id, cur()), spyTop(), true);
  }
  fixHash();
  addEventListener('hashchange', fixHash);
  spy();
})();
