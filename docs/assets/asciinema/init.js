// Turn each `<div class="asciinema-cast" data-cast="...">` on the page into an
// asciinema player. The data-cast path is resolved relative to the current
// document, so a cast under docs/assets/casts works both on a locally served
// site rooted at / and on a project site served under a path prefix.
window.addEventListener("DOMContentLoaded", function () {
  if (typeof AsciinemaPlayer === "undefined") {
    return;
  }
  document.querySelectorAll(".asciinema-cast").forEach(function (el) {
    var src = new URL(el.dataset.cast, document.baseURI).href;
    AsciinemaPlayer.create(src, el, {
      autoPlay: el.dataset.autoplay === "true",
      loop: el.dataset.loop === "true",
      idleTimeLimit: 2,
      theme: "asciinema",
      fit: "width",
      // Prefer the locally served JetBrains Mono, which draws box characters
      // edge to edge so the review's dialog borders and gutter rules are
      // unbroken. Its vertical rules overdraw past the em box and stay joined
      // at the player's default line height, so no line-height override is
      // needed. The player's own default stack falls back to a generic
      // monospace whose box glyphs can render narrower than the cell.
      terminalFontFamily: "'JetBrains Mono', monospace",
    });
  });
});
