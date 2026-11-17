// How many PCs use Prostir, from GoatCounter's public counter. Prostir counts each PC
// once a day (no id, nothing about the user); this page only reads the totals.
// Hidden when there is nothing to show or the counter cannot be reached.
(function () {
  const out = document.getElementById("downloads");
  if (!out) return;
  const uk = document.documentElement.lang === "uk";
  const site = "https://prostir.goatcounter.com/counter/";
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
