use crate::portal_ui;

pub(crate) const STYLE: &str = r#"
[hidden]{display:none!important}
.vms{max-width:1120px}
.vm-list{display:grid;gap:16px}
.vm{padding:20px;display:grid;gap:18px;min-width:0}
.vm-head{display:flex;justify-content:space-between;align-items:flex-start;gap:12px}
.vm-head h3{font-size:17px;overflow-wrap:anywhere}
.vm-head p{margin-top:3px;color:var(--muted);font-size:13px;overflow-wrap:anywhere}
.gauges{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:18px 24px}
.gauge-row{display:flex;justify-content:space-between;align-items:baseline;gap:10px;font-size:13px}
.gauge-row b{font-weight:600}
.gauge-row span{font-weight:700;white-space:nowrap}
.gauge-meta{color:var(--faint);font-size:12px;overflow-wrap:anywhere}
.vm .facts{margin-top:0}
.vm-empty{padding:36px 18px;border:1px dashed var(--border-strong);border-radius:14px;color:var(--muted);text-align:center}
.vm-empty b{display:block;color:var(--text);margin-bottom:4px}
.vm-foot{display:flex;justify-content:space-between;gap:8px 16px;flex-wrap:wrap;color:var(--faint);font-size:12.5px}
@media (max-width:900px){.gauges{grid-template-columns:1fr}}
"#;

pub(crate) const SCRIPT: &str = r#"(function () {
  const REFRESH_MS = 5000;
  const previous = new Map();

  const el = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };

  const bytes = value => {
    if (value === null || value === undefined) return "—";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let amount = value;
    let unit = 0;
    while (amount >= 1024 && unit < units.length - 1) {
      amount /= 1024;
      unit += 1;
    }
    return `${amount >= 10 || unit === 0 ? Math.round(amount) : amount.toFixed(1)} ${units[unit]}`;
  };

  const since = value => {
    const time = value ? new Date(value).getTime() : NaN;
    if (!Number.isFinite(time) || time <= 0) return "—";
    const minutes = Math.max(0, Math.floor((Date.now() - time) / 60000));
    if (minutes < 60) return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    if (hours < 48) return `${hours}h ${minutes % 60}m`;
    return `${Math.floor(hours / 24)}d ${hours % 24}h`;
  };

  const running = vm => vm.status === "Running" || vm.status === "Ready";
  const clamp = value => Math.max(0, Math.min(100, value));
  const level = value => value >= 90 ? "critical" : value >= 75 ? "warning" : "normal";

  function cpuPercent(vm, now) {
    const last = previous.get(vm.name);
    if (vm.cpu_usage_ns === null || vm.cpu_usage_ns === undefined) {
      previous.delete(vm.name);
      return null;
    }
    const boot = `${vm.created_at}|${vm.started_at}`;
    previous.set(vm.name, { ns: vm.cpu_usage_ns, at: now, boot });
    if (!last || last.boot !== boot || now <= last.at || vm.cpu_usage_ns < last.ns) return null;
    return clamp((vm.cpu_usage_ns - last.ns) / ((now - last.at) * 1e6) / (vm.cpus || 1) * 100);
  }

  function gauge(label, percent, meta) {
    const item = el("div", "gauge");
    const row = el("div", "gauge-row");
    row.append(el("b", "", label));
    row.append(el("span", "", percent === null ? "—" : `${Math.round(percent)}%`));
    item.append(row);
    const meter = el("div", "meter");
    meter.dataset.level = level(percent || 0);
    meter.setAttribute("role", "meter");
    meter.setAttribute("aria-valuemin", "0");
    meter.setAttribute("aria-valuemax", "100");
    meter.setAttribute("aria-valuenow", String(Math.round(percent || 0)));
    meter.setAttribute("aria-label", label);
    const fill = el("i");
    fill.style.width = `${clamp(percent || 0)}%`;
    meter.append(fill);
    item.append(meter);
    item.append(el("div", "gauge-meta", meta));
    return item;
  }

  const share = (used, total) => used && total ? clamp(used / total * 100) : null;

  function fact(label, value) {
    const cell = el("div");
    cell.append(el("span", "", label));
    cell.append(el("strong", "", value));
    return cell;
  }

  function statusChip(status) {
    const tone = running({ status }) ? "green" : status === "Error" ? "red"
      : status === "Stopped" || status === "Frozen" ? "gray" : "amber";
    return el("span", `chip ${tone}`, status || "Unknown");
  }

  function renderVm(vm, now) {
    const card = el("article", "card vm");
    const head = el("div", "vm-head");
    const title = el("div");
    title.append(el("h3", "mono", vm.name));
    title.append(el("p", "", [vm.image || "Installed from media", ...(vm.ipv4 || [])].join(" · ")));
    head.append(title);
    head.append(statusChip(vm.status));
    card.append(head);

    const cpu = cpuPercent(vm, now);
    const gauges = el("div", "gauges");
    gauges.append(gauge("CPU", cpu, cpu === null
      ? "Measuring…" : `of ${vm.cpus || 1} vCPU${vm.cpus === 1 ? "" : "s"}`));
    gauges.append(gauge("Memory", share(vm.memory_usage, vm.memory_total),
      vm.memory_usage ? `${bytes(vm.memory_usage)} of ${bytes(vm.memory_total)}` : "Not reported"));
    gauges.append(gauge("Disk", share(vm.disk_usage, vm.disk_total),
      vm.disk_usage ? `${bytes(vm.disk_usage)} of ${bytes(vm.disk_total)}` : "Not reported"));
    card.append(gauges);

    const facts = el("div", "facts");
    facts.append(fact("vCPU limit", vm.cpus ? String(vm.cpus) : "—"));
    facts.append(fact("Memory limit", bytes(vm.memory_limit)));
    facts.append(fact("Disk size", bytes(vm.disk_total)));
    facts.append(fact("Network in / out", `${bytes(vm.rx_bytes)} / ${bytes(vm.tx_bytes)}`));
    facts.append(fact(running(vm) ? "Up for" : "Created", since(running(vm) ? vm.started_at : vm.created_at)));
    facts.append(fact("Processes", vm.processes ? String(vm.processes) : "No agent"));
    card.append(facts);
    return card;
  }

  function render(vms) {
    const now = Date.now();
    const names = new Set(vms.map(vm => vm.name));
    [...previous.keys()].forEach(name => { if (!names.has(name)) previous.delete(name); });
    const list = document.getElementById("vm-list");
    if (!vms.length) {
      const empty = el("div", "vm-empty");
      empty.append(el("b", "", "No virtual machines"));
      empty.append("Agents start one with vm launch; it appears here while it runs.");
      list.replaceChildren(empty);
    } else {
      list.replaceChildren(...vms.map(vm => renderVm(vm, now)));
    }
    const active = vms.filter(running);
    document.getElementById("running").textContent = `${active.length} of ${vms.length}`;
    document.getElementById("vcpus").textContent =
      String(active.reduce((total, vm) => total + (vm.cpus || 0), 0));
    document.getElementById("memory").textContent =
      bytes(active.reduce((total, vm) => total + (vm.memory_limit || 0), 0));
    document.getElementById("refreshed").textContent =
      `Refreshed ${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })} · every 5 seconds`;
  }

  async function refresh() {
    const error = document.getElementById("error");
    try {
      const response = await fetch("/api/vms", { cache: "no-store" });
      if (response.status === 401) {
        location.reload();
        return;
      }
      const data = await response.json();
      if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
      render(Array.isArray(data.vms) ? data.vms : []);
      error.hidden = true;
    } catch (failure) {
      error.lastChild.textContent = `Could not read the VMs from Incus: ${failure.message}. Retrying.`;
      error.hidden = false;
    }
    setTimeout(refresh, REFRESH_MS);
  }

  document.addEventListener("DOMContentLoaded", refresh);
})();
"#;

pub(crate) fn main() -> String {
    format!(
        r#"<main class="page vms"><div class="page-h"><h1>Virtual machines</h1><p>VMs launched through the Virtual Machine Bridge, with their limits and live resource use.</p></div><section class="kpis" aria-label="VM summary"><div class="card kpi"><span>Running</span><strong id="running">Loading…</strong></div><div class="card kpi"><span>vCPUs allocated</span><strong id="vcpus">—</strong></div><div class="card kpi"><span>Memory allocated</span><strong id="memory">—</strong></div></section><div class="flash error" id="error" role="alert" hidden>{}<span></span></div><section class="vm-list" id="vm-list" aria-live="polite"></section><footer class="vm-foot"><span>VMs are ephemeral: stopping one deletes it and its disk.</span><span id="refreshed">Refreshes every 5 seconds</span></footer></main>"#,
        portal_ui::ICON_WARN
    )
}

pub(crate) fn disabled_main() -> String {
    r#"<main class="page vms"><div class="page-h"><h1>Virtual machines</h1></div><div class="vm-empty"><b>The Virtual Machine Bridge is off</b>Start the sandbox with <code>--vm</code> to let agents run VMs and see them here.</div></main>"#.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_polls_the_api_and_builds_safe_dom_text() {
        assert!(SCRIPT.contains("fetch(\"/api/vms\""));
        assert!(SCRIPT.contains("textContent = text"));
        assert!(!SCRIPT.contains("innerHTML"));
        assert!(!main().contains("<script"));
    }
}
