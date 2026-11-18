// Marks the page as scripted, then reveals each band once as it scrolls into view.
document.documentElement.classList.add("js");
(function () {
  const items = document.querySelectorAll(".reveal");
  if (!("IntersectionObserver" in window)) {
    items.forEach((el) => el.classList.add("seen"));
    return;
  }
  const io = new IntersectionObserver(
    (entries) => {
      entries.forEach((e) => {
        if (e.isIntersecting) {
          e.target.classList.add("seen");
          io.unobserve(e.target);
        }
      });
    },
    { rootMargin: "0px 0px -10% 0px" }
  );
  items.forEach((el) => io.observe(el));
})();

// Demo video: one pause/play button instead of the browser controls. It plays on its
// own only when the visitor has not asked for reduced motion, and only while it is on
// screen. With reduced motion nothing loads until the button is pressed.
(function () {
  const video = document.querySelector(".demo video");
  const button = document.querySelector(".demo-toggle");
  if (!video || !button) return;
  const label = button.querySelector("span");
  const icon = button.querySelector("path");
  const PLAY = "M3 1.5v13l11-6.5z";
  const PAUSE = "M3 1.5h3.5v13H3zM9.5 1.5H13v13H9.5z";
  const calm = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  let wanted = !calm; // the visitor's choice; starts as "play" unless motion is unwelcome
  let visible = true;

  const show = () => {
    const playing = !video.paused;
    label.textContent = playing ? button.dataset.pause : button.dataset.play;
    icon.setAttribute("d", playing ? PAUSE : PLAY);
  };
  const sync = () => {
    if (wanted && visible) video.play().catch(() => { wanted = false; show(); });
    else video.pause();
  };

  video.removeAttribute("controls");
  video.addEventListener("play", show);
  video.addEventListener("pause", show);
  button.addEventListener("click", () => {
    wanted = video.paused;
    sync();
  });
  button.hidden = false;
  show();

  if ("IntersectionObserver" in window) {
    new IntersectionObserver((entries) => {
      visible = entries[entries.length - 1].isIntersecting;
      sync();
    }).observe(video);
  } else {
    sync();
  }
})();

// How many PCs use Prostir, from GoatCounter's public counter. Prostir counts each PC
// once a day (no id, nothing about the user); this page only reads the totals.
// Hidden when there is nothing to show or the counter cannot be reached.
(function () {
  const out = document.getElementById("downloads");
  if (!out) return;
  const uk = document.documentElement.lang === "uk";
  const site = "https://ishawyha.goatcounter.com/counter/";
  const count = (path) =>
    fetch(site + encodeURIComponent(path) + ".json")
      .then((r) => (r.ok ? r.json() : null))
      .then((j) => (j ? parseInt(String(j.count).replace(/[^0-9]/g, ""), 10) || 0 : 0))
      .catch(() => 0);
  const yesterday = new Date(Date.now() - 86400000).toISOString().slice(0, 10);
  Promise.all([count("/day/" + yesterday), count("/install")]).then(([day, installs]) => {
    const parts = [];
    if (day > 0) parts.push(uk ? `Учора Prostir працював на ${day} ПК` : `Used on ${day} ${day === 1 ? "PC" : "PCs"} yesterday`);
    if (installs > 0) {
      const n = installs % 100, d = installs % 10;
      const word = n >= 11 && n <= 14 ? "встановлень" : d === 1 ? "встановлення" : d >= 2 && d <= 4 ? "встановлення" : "встановлень";
      parts.push(uk ? `${installs} ${word}` : `${installs} ${installs === 1 ? "install" : "installs"}`);
    }
    if (parts.length) out.textContent = parts.join(" · ");
  });
})();
