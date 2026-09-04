pub(crate) const PAGE: &str = r####"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <meta name="color-scheme" content="dark">
  <title>Plan usage · Claude Sandbox</title>
  <style>
    :root {
      --bg: #0d0f0d;
      --panel: #151815;
      --panel-raised: #1a1d19;
      --ink: #f3f1e7;
      --muted: #999f95;
      --line: #30352e;
      --acid: #d7ff64;
      --coral: #ff816a;
      --sky: #79c9ff;
      --amber: #ffc261;
    }

    * { box-sizing: border-box; }

    body {
      margin: 0;
      min-height: 100vh;
      color: var(--ink);
      background:
        radial-gradient(circle at 82% 0%, rgba(125, 161, 50, .2), transparent 29rem),
        linear-gradient(rgba(255,255,255,.018) 1px, transparent 1px),
        linear-gradient(90deg, rgba(255,255,255,.018) 1px, transparent 1px),
        var(--bg);
      background-size: auto, 48px 48px, 48px 48px, auto;
      font: 14px/1.5 ui-monospace, "SFMono-Regular", Menlo, Consolas, monospace;
    }

    a { color: inherit; }

    main {
      width: min(1180px, calc(100% - 40px));
      margin: 0 auto;
      padding: 40px 0 56px;
    }

    .topline {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 20px;
      padding-top: 13px;
      border-top: 1px solid var(--acid);
      color: var(--muted);
      font-size: 11px;
      letter-spacing: .11em;
      text-transform: uppercase;
    }

    .live-mark { display: inline-flex; align-items: center; gap: 9px; }
    .live-mark::before {
      width: 7px;
      height: 7px;
      border-radius: 50%;
      background: var(--acid);
      box-shadow: 0 0 16px rgba(215,255,100,.65);
      content: "";
    }

    .api-link {
      text-decoration: none;
      border-bottom: 1px solid #59604f;
      padding-bottom: 2px;
    }
    .api-link:hover { color: var(--acid); border-color: var(--acid); }

    header {
      display: grid;
      grid-template-columns: minmax(0, 1.5fr) minmax(250px, .65fr);
      gap: 48px;
      align-items: end;
      padding: 62px 0 46px;
    }

    h1 {
      margin: 0;
      max-width: 760px;
      font: 400 clamp(52px, 8.7vw, 112px)/.82 Georgia, "Times New Roman", serif;
      letter-spacing: -.065em;
    }

    h1 em { color: var(--acid); font-style: italic; }

    .intro {
      margin: 0 0 5px;
      color: var(--muted);
      font: 16px/1.55 ui-sans-serif, system-ui, sans-serif;
    }

    .summary-strip {
      display: grid;
      grid-template-columns: repeat(3, 1fr);
      border: 1px solid var(--line);
      background: rgba(16,18,15,.84);
    }

    .summary-item {
      min-height: 88px;
      padding: 17px 20px;
      border-right: 1px solid var(--line);
    }
    .summary-item:last-child { border-right: 0; }
    .summary-label {
      display: block;
      margin-bottom: 8px;
      color: var(--muted);
      font-size: 10px;
      letter-spacing: .12em;
      text-transform: uppercase;
    }
    .summary-value { font-size: 16px; letter-spacing: -.02em; }

    .section-heading {
      display: flex;
      align-items: baseline;
      justify-content: space-between;
      gap: 20px;
      margin: 42px 0 15px;
    }
    .section-heading h2 {
      margin: 0;
      font-size: 11px;
      font-weight: 500;
      letter-spacing: .15em;
      text-transform: uppercase;
    }
    .section-heading span { color: var(--muted); font-size: 11px; }

    .providers {
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      border-top: 1px solid var(--line);
      border-left: 1px solid var(--line);
    }

    .provider {
      --accent: var(--acid);
      min-width: 0;
      padding: 24px;
      border-right: 1px solid var(--line);
      border-bottom: 1px solid var(--line);
      background: linear-gradient(145deg, rgba(255,255,255,.025), transparent 48%), var(--panel);
    }
    .provider[data-provider="anthropic"] { --accent: var(--coral); }
    .provider[data-provider="openai"] { --accent: var(--acid); }
    .provider[data-provider="ollama"] { --accent: var(--sky); }

    .provider-head { display: flex; justify-content: space-between; gap: 16px; }
    .provider-name {
      margin: 0;
      font: 400 29px/1 Georgia, serif;
      letter-spacing: -.035em;
    }

    .freshness {
      display: inline-flex;
      align-items: center;
      gap: 7px;
      color: var(--muted);
      font-size: 9px;
      letter-spacing: .12em;
      text-transform: uppercase;
    }
    .freshness::before {
      width: 6px;
      height: 6px;
      border-radius: 50%;
      background: var(--muted);
      content: "";
    }
    .freshness[data-state="fresh"]::before { background: var(--accent); }
    .freshness[data-state="stale"]::before { background: var(--amber); }

    .gauge-wrap {
      display: grid;
      grid-template-columns: 126px 1fr;
      gap: 18px;
      align-items: center;
      min-height: 166px;
      padding: 24px 0 20px;
    }

    .gauge {
      --value: 0;
      position: relative;
      display: grid;
      width: 126px;
      aspect-ratio: 1;
      place-items: center;
      border-radius: 50%;
      background: conic-gradient(var(--accent) calc(var(--value) * 1%), #282c26 0);
    }
    .gauge::before {
      position: absolute;
      width: 94px;
      aspect-ratio: 1;
      border: 1px solid #343a31;
      border-radius: 50%;
      background: var(--panel);
      content: "";
    }
    .gauge-value { position: relative; font-size: 27px; letter-spacing: -.06em; }
    .gauge-value small { color: var(--muted); font-size: 12px; letter-spacing: 0; }

    .pressure-label {
      display: block;
      margin-bottom: 8px;
      color: var(--muted);
      font-size: 9px;
      letter-spacing: .12em;
      text-transform: uppercase;
    }
    .pressure-name { margin: 0; overflow-wrap: anywhere; font-size: 13px; }
    .pressure-reset { margin: 8px 0 0; color: var(--muted); font-size: 11px; }

    .bucket-list { display: grid; gap: 19px; }
    .bucket { min-width: 0; }
    .bucket-row { display: flex; justify-content: space-between; gap: 12px; }
    .bucket-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11px; }
    .bucket-percent { flex: 0 0 auto; color: var(--accent); font-size: 11px; }
    .bar { height: 5px; margin: 8px 0 7px; overflow: hidden; background: #2a2e28; }
    .bar-fill { width: 0; height: 100%; background: var(--accent); transition: width .5s ease; }
    .bucket-meta { display: flex; justify-content: space-between; gap: 8px; color: #7c8279; font-size: 9px; text-transform: uppercase; }

    .provider-empty {
      display: grid;
      min-height: 166px;
      margin-top: 20px;
      place-items: center;
      border: 1px dashed #353a33;
      color: var(--muted);
      text-align: center;
    }

    .provider-updated {
      margin: 24px 0 0;
      padding-top: 13px;
      border-top: 1px solid var(--line);
      color: #6f756c;
      font-size: 9px;
      letter-spacing: .07em;
      text-transform: uppercase;
    }

    .error {
      margin-top: 20px;
      padding: 15px 18px;
      border: 1px solid #7e473f;
      color: #ffb8ac;
      background: rgba(126,71,63,.12);
    }
    [hidden] { display: none !important; }

    footer {
      display: flex;
      justify-content: space-between;
      gap: 20px;
      margin-top: 38px;
      color: #6f756c;
      font-size: 9px;
      letter-spacing: .1em;
      text-transform: uppercase;
    }

    @media (max-width: 900px) {
      header { grid-template-columns: 1fr; gap: 28px; }
      .intro { max-width: 580px; }
      .providers { grid-template-columns: 1fr; }
    }

    @media (max-width: 560px) {
      main { width: min(100% - 24px, 1180px); padding-top: 22px; }
      .topline { align-items: flex-start; flex-direction: column; }
      header { padding: 42px 0 34px; }
      .summary-strip { grid-template-columns: 1fr; }
      .summary-item { min-height: 70px; border-right: 0; border-bottom: 1px solid var(--line); }
      .summary-item:last-child { border-bottom: 0; }
      .provider { padding: 20px; }
      .gauge-wrap { grid-template-columns: 112px 1fr; }
      .gauge { width: 112px; }
      .gauge::before { width: 82px; }
      footer { flex-direction: column; }
    }
  </style>
</head>
<body>
  <main>
    <div class="topline">
      <span class="live-mark">Public plan telemetry</span>
      <a class="api-link" href="/api/usage">JSON API ↗</a>
    </div>

    <header>
      <h1>Plan<br><em>headroom.</em></h1>
      <p class="intro">A live view of plan-limit pressure across the model providers available to this sandbox.</p>
    </header>

    <section class="summary-strip" aria-label="Usage summary">
      <div class="summary-item">
        <span class="summary-label">Reporting</span>
        <span class="summary-value" id="reporting">Loading…</span>
      </div>
      <div class="summary-item">
        <span class="summary-label">Highest pressure</span>
        <span class="summary-value" id="pressure">—</span>
      </div>
      <div class="summary-item">
        <span class="summary-label">Next reset</span>
        <span class="summary-value" id="next-reset">—</span>
      </div>
    </section>

    <div class="section-heading">
      <h2>Provider limits</h2>
      <span>Used / plan allowance</span>
    </div>
    <section class="providers" id="providers" aria-live="polite"></section>
    <div class="error" id="error" role="alert" hidden></div>

    <footer>
      <span>Advisory telemetry · no authentication</span>
      <span id="refreshed">Auto-refreshes every 60 seconds</span>
    </footer>
  </main>

  <script>
    const providerNames = { anthropic: "Anthropic", openai: "OpenAI", ollama: "Ollama" };
    const providerOrder = Object.keys(providerNames);

    const el = (tag, className, text) => {
      const node = document.createElement(tag);
      if (className) node.className = className;
      if (text !== undefined) node.textContent = text;
      return node;
    };

    const shortDate = value => {
      if (!value) return "Reset not reported";
      const date = new Date(value);
      return `Resets ${new Intl.DateTimeFormat(undefined, {
        month: "short", day: "numeric", hour: "numeric", minute: "2-digit"
      }).format(date)}`;
    };

    const until = value => {
      if (!value) return "Not scheduled";
      const milliseconds = new Date(value).getTime() - Date.now();
      if (milliseconds <= 0) return "Due now";
      const minutes = Math.ceil(milliseconds / 60000);
      if (minutes < 60) return `In ${minutes}m`;
      const hours = Math.ceil(minutes / 60);
      if (hours < 48) return `In ${hours}h`;
      return `In ${Math.ceil(hours / 24)}d`;
    };

    const bucketName = bucket => bucket.label || `${bucket.period} limit`;

    function renderProvider(key, provider) {
      const card = el("article", "provider");
      card.dataset.provider = key;

      const head = el("div", "provider-head");
      head.append(el("h3", "provider-name", providerNames[key]));
      const freshness = el("span", "freshness", provider.freshness || "unknown");
      freshness.dataset.state = provider.freshness || "unknown";
      head.append(freshness);
      card.append(head);

      const buckets = Array.isArray(provider.buckets) ? provider.buckets : [];
      if (!buckets.length) {
        card.append(el("div", "provider-empty", "No usage limits reported"));
      } else {
        const highest = buckets.reduce((current, bucket) =>
          bucket.used_percent > current.used_percent ? bucket : current
        );
        const gaugeWrap = el("div", "gauge-wrap");
        const gauge = el("div", "gauge");
        gauge.style.setProperty("--value", Math.max(0, Math.min(100, highest.used_percent)));
        const gaugeValue = el("span", "gauge-value", `${highest.used_percent}`);
        gaugeValue.append(el("small", "", "%"));
        gauge.append(gaugeValue);
        gaugeWrap.append(gauge);

        const pressureCopy = el("div", "pressure-copy");
        pressureCopy.append(el("span", "pressure-label", "Most used"));
        pressureCopy.append(el("p", "pressure-name", bucketName(highest)));
        pressureCopy.append(el("p", "pressure-reset", shortDate(highest.resets_at)));
        gaugeWrap.append(pressureCopy);
        card.append(gaugeWrap);

        const list = el("div", "bucket-list");
        buckets.forEach(bucket => {
          const item = el("div", "bucket");
          const row = el("div", "bucket-row");
          row.append(el("span", "bucket-label", bucketName(bucket)));
          row.append(el("span", "bucket-percent", `${bucket.used_percent}%`));
          item.append(row);
          const bar = el("div", "bar");
          const fill = el("div", "bar-fill");
          fill.style.width = `${Math.max(0, Math.min(100, bucket.used_percent))}%`;
          bar.append(fill);
          item.append(bar);
          const meta = el("div", "bucket-meta");
          meta.append(el("span", "", bucket.period));
          meta.append(el("span", "", until(bucket.resets_at)));
          item.append(meta);
          list.append(item);
        });
        card.append(list);
      }

      const updated = provider.updated_at
        ? `Observed ${new Date(provider.updated_at).toLocaleString()}`
        : "Waiting for first observation";
      card.append(el("p", "provider-updated", updated));
      return card;
    }

    function render(data) {
      const providers = data && data.providers ? data.providers : {};
      const root = document.getElementById("providers");
      root.replaceChildren(...providerOrder.map(key =>
        renderProvider(key, providers[key] || { freshness: "unknown", buckets: [] })
      ));

      const available = providerOrder.filter(key => providers[key]?.freshness !== "unknown");
      document.getElementById("reporting").textContent = `${available.length} of 3 providers`;

      const buckets = providerOrder.flatMap(key =>
        (providers[key]?.buckets || []).map(bucket => ({ ...bucket, provider: providerNames[key] }))
      );
      const highest = buckets.reduce((current, bucket) =>
        !current || bucket.used_percent > current.used_percent ? bucket : current
      , null);
      document.getElementById("pressure").textContent = highest
        ? `${highest.used_percent}% · ${highest.provider}` : "No data yet";

      const resets = buckets.map(bucket => bucket.resets_at).filter(Boolean).sort();
      document.getElementById("next-reset").textContent = resets.length ? until(resets[0]) : "Not scheduled";
      document.getElementById("refreshed").textContent =
        `Refreshed ${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} · every 60s`;
    }

    async function refresh() {
      const error = document.getElementById("error");
      try {
        const response = await fetch("/api/usage", { cache: "no-store" });
        const data = await response.json();
        render(data);
        error.hidden = true;
      } catch (_) {
        error.textContent = "Usage data could not be loaded. The dashboard will retry automatically.";
        error.hidden = false;
        if (!document.getElementById("providers").children.length) {
          render({ providers: {} });
        }
      }
    }

    refresh();
    setInterval(refresh, 60000);
  </script>
</body>
</html>
"####;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_uses_the_public_api_and_safe_dom_text() {
        assert!(PAGE.contains("fetch(\"/api/usage\""));
        assert!(PAGE.contains("textContent = text"));
        assert!(!PAGE.contains("innerHTML"));
    }
}
