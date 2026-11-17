// Shows how many times the releases were downloaded, from GitHub's public API.
// Nothing about the visitor is sent anywhere else. Hidden when there is nothing to
// show or GitHub cannot be reached.
(function () {
  const out = document.getElementById("downloads");
  if (!out) return;
  const words = {
    uk: (n) => `Завантажено ${n.toLocaleString("uk-UA")} разів`,
    en: (n) => `Downloaded ${n.toLocaleString("en-US")} times`,
  };
  const say = words[document.documentElement.lang] || words.en;
  fetch("https://api.github.com/repos/OlexiyOdarchuk/prostir/releases?per_page=100")
    .then((r) => (r.ok ? r.json() : []))
    .then((releases) => {
      const total = releases
        .flatMap((r) => r.assets || [])
        .reduce((sum, a) => sum + (a.download_count || 0), 0);
      if (total > 0) out.textContent = say(total);
    })
    .catch(() => {});
})();
