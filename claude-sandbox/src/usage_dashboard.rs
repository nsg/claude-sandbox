use crate::portal_ui;

const STYLE: &str = r#"
[hidden]{display:none!important}
.usage{max-width:1120px}
.usage-h{display:flex;align-items:flex-end;justify-content:space-between;gap:16px;flex-wrap:wrap}
.kpis{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:14px}
.kpi{padding:16px 18px}
.kpi span{display:block;color:var(--faint);font-size:12px;font-weight:600}
.kpi strong{display:block;margin-top:4px;font-size:19px;font-weight:700;letter-spacing:-.01em;overflow-wrap:anywhere}
.sec-h{display:flex;align-items:baseline;justify-content:space-between;gap:12px;margin-top:6px}
.sec-h h2{font-size:15px}
.sec-h span{color:var(--faint);font-size:12.5px}
.providers{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:16px}
.provider{padding:20px;display:grid;gap:18px;align-content:start;min-width:0}
.provider-head{display:flex;justify-content:space-between;align-items:center;gap:12px}
.provider-head h3{font-size:17px}
.fresh{display:inline-flex;align-items:center;gap:6px;color:var(--muted);font-size:12px;font-weight:600}
.fresh::before{content:"";width:7px;height:7px;border-radius:50%;background:var(--faint)}
.fresh[data-state=fresh]::before{background:var(--ok)}
.fresh[data-state=stale]::before{background:var(--warn)}
.headline .figure{font-size:44px;font-weight:700;letter-spacing:-.03em;line-height:1}
.headline .figure small{font-size:20px;color:var(--muted);font-weight:650;margin-left:2px}
.headline p{margin-top:6px;color:var(--muted);font-size:13px;overflow-wrap:anywhere}
.buckets{display:grid;gap:16px}
.bucket-row{display:flex;justify-content:space-between;align-items:center;gap:10px;font-size:13px}
.bucket-row .name{font-weight:600;min-width:0;overflow-wrap:anywhere}
.bucket-row .pct{display:inline-flex;align-items:center;gap:6px;font-weight:700;white-space:nowrap}
.meter{--fill:var(--accent);height:8px;margin:7px 0 5px;border-radius:4px;overflow:hidden;background:color-mix(in srgb,var(--fill) 16%,var(--surface))}
.meter i{display:block;height:100%;min-width:0;border-radius:4px;background:var(--fill);transition:width .5s ease}
.meter[data-level=warning]{--fill:#fab219}
.meter[data-level=critical]{--fill:#d03b3b}
[data-theme=dark] .meter[data-level=critical]{--fill:#e5524f}
.bucket-meta{display:flex;justify-content:space-between;gap:8px;color:var(--faint);font-size:12px}
.provider-empty{padding:26px 16px;border:1px dashed var(--border-strong);border-radius:12px;color:var(--muted);text-align:center}
.provider-foot{padding-top:12px;border-top:1px solid var(--border);color:var(--faint);font-size:12px}
.usage-foot{display:flex;justify-content:space-between;gap:8px 16px;flex-wrap:wrap;color:var(--faint);font-size:12.5px}
@media (max-width:900px){.providers{grid-template-columns:1fr}}
@media (max-width:600px){.kpis{grid-template-columns:1fr}.kpi{display:flex;justify-content:space-between;align-items:baseline;gap:12px}.kpi strong{margin:0;font-size:16px;text-align:right}}
"#;

pub(crate) const SCRIPT: &str = r#"(function () {
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
    return `Resets ${new Intl.DateTimeFormat(undefined, {
      month: "short", day: "numeric", hour: "numeric", minute: "2-digit"
    }).format(new Date(value))}`;
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
  const clamp = value => Math.max(0, Math.min(100, value));
  const level = value => value >= 90 ? "critical" : value >= 75 ? "warning" : "normal";

  function statusChip(value) {
    if (level(value) === "critical") return el("span", "chip red", "⚠ At limit");
    if (level(value) === "warning") return el("span", "chip amber", "⚠ Near limit");
    return null;
  }

  function renderBucket(bucket) {
    const item = el("div", "bucket");
    const row = el("div", "bucket-row");
    row.append(el("span", "name", bucketName(bucket)));
    const percent = el("span", "pct");
    const chip = statusChip(bucket.used_percent);
    if (chip) percent.append(chip);
    percent.append(`${bucket.used_percent}%`);
    row.append(percent);
    item.append(row);
    const meter = el("div", "meter");
    meter.dataset.level = level(bucket.used_percent);
    meter.title = `${bucketName(bucket)}: ${bucket.used_percent}% used · ${shortDate(bucket.resets_at)}`;
    meter.setAttribute("role", "meter");
    meter.setAttribute("aria-valuemin", "0");
    meter.setAttribute("aria-valuemax", "100");
    meter.setAttribute("aria-valuenow", String(clamp(bucket.used_percent)));
    meter.setAttribute("aria-label", bucketName(bucket));
    const fill = el("i");
    fill.style.width = `${clamp(bucket.used_percent)}%`;
    meter.append(fill);
    item.append(meter);
    const meta = el("div", "bucket-meta");
    meta.append(el("span", "", bucket.period));
    meta.append(el("span", "", until(bucket.resets_at)));
    item.append(meta);
    return item;
  }

  function renderProvider(key, provider) {
    const card = el("article", "card provider");
    const head = el("div", "provider-head");
    head.append(el("h3", "", providerNames[key]));
    const state = provider.freshness || "unknown";
    const freshness = el("span", "fresh", state === "unknown" ? "no data" : state);
    freshness.dataset.state = state;
    head.append(freshness);
    card.append(head);

    const buckets = Array.isArray(provider.buckets) ? provider.buckets : [];
    if (!buckets.length) {
      card.append(el("div", "provider-empty", "No usage limits reported"));
    } else {
      const highest = buckets.reduce((current, bucket) =>
        bucket.used_percent > current.used_percent ? bucket : current
      );
      const headline = el("div", "headline");
      const figure = el("div", "figure", `${highest.used_percent}`);
      figure.append(el("small", "", "%"));
      headline.append(figure);
      headline.append(el("p", "", `Most used: ${bucketName(highest)} · ${shortDate(highest.resets_at)}`));
      card.append(headline);
      const list = el("div", "buckets");
      buckets.forEach(bucket => list.append(renderBucket(bucket)));
      card.append(list);
    }

    card.append(el("p", "provider-foot", provider.updated_at
      ? `Observed ${new Date(provider.updated_at).toLocaleString()}`
      : "Waiting for the first observation"));
    return card;
  }

  function render(data) {
    const providers = data && data.providers ? data.providers : {};
    document.getElementById("providers").replaceChildren(...providerOrder.map(key =>
      renderProvider(key, providers[key] || { freshness: "unknown", buckets: [] })
    ));

    const available = providerOrder.filter(key => providers[key]?.freshness !== "unknown"
      && providers[key]?.freshness !== undefined);
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
      `Refreshed ${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} · every 60 seconds`;
  }

  async function refresh() {
    const error = document.getElementById("error");
    try {
      const response = await fetch("/api/usage", { cache: "no-store" });
      render(await response.json());
      error.hidden = true;
    } catch (_) {
      error.lastChild.textContent = "Usage data could not be loaded. The dashboard will retry automatically.";
      error.hidden = false;
      if (!document.getElementById("providers").children.length) render({ providers: {} });
    }
  }

  document.addEventListener("DOMContentLoaded", () => {
    refresh();
    setInterval(refresh, 60000);
  });
})();
"#;

pub(crate) fn page() -> String {
    let body = format!(
        r#"{}<main class="page usage"><div class="usage-h"><div class="page-h"><h1>Plan usage</h1><p>Live plan-limit pressure across the model providers available to this sandbox.</p></div><a class="pill-btn" href="/api/usage">JSON API ↗</a></div><section class="kpis" aria-label="Usage summary"><div class="card kpi"><span>Reporting</span><strong id="reporting">Loading…</strong></div><div class="card kpi"><span>Highest pressure</span><strong id="pressure">—</strong></div><div class="card kpi"><span>Next reset</span><strong id="next-reset">—</strong></div></section><div class="sec-h"><h2>Provider limits</h2><span>Used of each plan allowance</span></div><section class="providers" id="providers" aria-live="polite"></section><div class="flash error" id="error" role="alert" hidden>{}<span></span></div><footer class="usage-foot"><span>Advisory telemetry · no authentication</span><span id="refreshed">Refreshes every 60 seconds</span></footer></main>"#,
        portal_ui::public_bar(),
        portal_ui::ICON_WARN
    );
    portal_ui::page(
        "Plan usage · Claude Sandbox",
        "",
        STYLE,
        r#"<script src="/assets/usage.js" defer></script>"#,
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_uses_the_public_api_and_safe_dom_text() {
        assert!(SCRIPT.contains("fetch(\"/api/usage\""));
        assert!(SCRIPT.contains("textContent = text"));
        assert!(!SCRIPT.contains("innerHTML"));
        let page = page();
        assert!(page.contains(r#"<script src="/assets/usage.js" defer></script>"#));
        assert!(!page.contains("<script>"));
        assert!(!page.contains(r#"class="nav""#));
    }
}
