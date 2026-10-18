//! Static assets and the page shell of the admin portal.

pub const SCRIPT: &str = r#"(function () {
  var root = document.documentElement;
  var stored = null;
  try { stored = localStorage.getItem('portal-theme'); } catch (error) {}
  var media = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;
  function system() { return media && media.matches ? 'dark' : 'light'; }
  root.dataset.theme = stored === 'light' || stored === 'dark' ? stored : system();
  if (media && media.addEventListener) {
    media.addEventListener('change', function () {
      var saved = null;
      try { saved = localStorage.getItem('portal-theme'); } catch (error) {}
      if (saved !== 'light' && saved !== 'dark') root.dataset.theme = system();
    });
  }
  function copy(text) {
    if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text);
    return new Promise(function (resolve, reject) {
      var field = document.createElement('textarea');
      field.value = text;
      field.setAttribute('readonly', '');
      field.style.position = 'fixed';
      field.style.opacity = '0';
      document.body.appendChild(field);
      field.select();
      var copied = document.execCommand('copy');
      document.body.removeChild(field);
      copied ? resolve() : reject();
    });
  }
  document.addEventListener('DOMContentLoaded', function () {
    document.querySelectorAll('[data-theme-toggle]').forEach(function (button) {
      button.addEventListener('click', function () {
        var next = root.dataset.theme === 'dark' ? 'light' : 'dark';
        root.dataset.theme = next;
        try { localStorage.setItem('portal-theme', next); } catch (error) {}
      });
    });
    document.querySelectorAll('[data-copy]').forEach(function (button) {
      button.addEventListener('click', function () {
        var label = button.querySelector('span');
        var text = button.getAttribute('data-copy');
        if (text.charAt(0) === '/') text = location.origin + text;
        copy(text).then(function () {
          if (label) label.textContent = 'Copied';
        }, function () {
          if (label) label.textContent = 'Copy failed';
        });
        setTimeout(function () { if (label) label.textContent = 'Copy link'; }, 1600);
      });
    });
  });
})();
"#;

pub const STYLE: &str = r#"
:root{color-scheme:light;
  --bg:#f5f5f2;--surface:#fff;--surface-2:#fafaf8;--hover:#f1f1ed;--border:#e7e6e1;--border-strong:#d6d5cf;
  --text:#1b1b19;--muted:#62625c;--faint:#8d8d86;
  --accent:#4f46e5;--accent-soft:#eef0ff;--accent-ink:#3730a3;--accent-line:#c9ccfb;
  --ok:#0f8f63;--ok-hover:#0c7c56;--ok-soft:#e8f7f0;--ok-ink:#0b6b4a;
  --warn:#a34b06;--warn-soft:#fdf4e6;--warn-line:#f3d9b1;
  --danger:#b93c0b;--danger-soft:#fff1ec;--danger-line:#fbd3c4;--danger-solid:#c2410c;
  --sky:#036196;--sky-soft:#e9f4fb;
  --gh:#5b21b6;--gh-soft:#f2edfd;
  --shadow:0 1px 2px rgba(20,20,15,.04),0 4px 16px rgba(20,20,15,.05);
  --pop-shadow:0 12px 32px rgba(20,20,15,.14);
  --code-bg:#fbfbf9;
}
[data-theme=dark]{color-scheme:dark;
  --bg:#0e0f11;--surface:#16171a;--surface-2:#1a1b1f;--hover:#212227;--border:#27282d;--border-strong:#35363c;
  --text:#ececec;--muted:#a6a6ad;--faint:#7c7d85;
  --accent:#8f8cff;--accent-soft:#1f2040;--accent-ink:#cfceff;--accent-line:#3b3c78;
  --ok:#1f9d70;--ok-hover:#25b07f;--ok-soft:#10261d;--ok-ink:#5fd3a5;
  --warn:#f0b35a;--warn-soft:#2a2112;--warn-line:#4a3a1d;
  --danger:#ff9a7a;--danger-soft:#2c1813;--danger-line:#55301f;--danger-solid:#c2410c;
  --sky:#7ccbf7;--sky-soft:#12232e;
  --gh:#c0aaff;--gh-soft:#231c3a;
  --shadow:0 1px 2px rgba(0,0,0,.3),0 8px 24px rgba(0,0,0,.22);
  --pop-shadow:0 12px 32px rgba(0,0,0,.5);
  --code-bg:#121316;
}
*{box-sizing:border-box}
html,body{margin:0}
body{background:var(--bg);color:var(--text);font:14px/1.5 Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Lato,system-ui,sans-serif;-webkit-font-smoothing:antialiased;min-height:100vh;min-height:100dvh;display:flex;flex-direction:column}
code,.mono,pre{font-family:ui-monospace,"SF Mono",Menlo,Consolas,"DejaVu Sans Mono",monospace}
code,.mono{font-size:12.5px;overflow-wrap:anywhere}
a{color:inherit;text-decoration:none}
a.link{color:var(--accent-ink);text-decoration:underline;text-underline-offset:3px}
button{font:inherit;cursor:pointer;color:inherit}
form{margin:0;display:contents}
h1{margin:0;font-size:26px;letter-spacing:-.025em;line-height:1.2;font-weight:700}
h2,h3{margin:0;letter-spacing:-.01em}
p{margin:0}

.top{min-height:60px;display:flex;align-items:center;gap:24px;padding:0 24px;background:var(--surface);border-bottom:1px solid var(--border);flex:none}
.brand{display:flex;align-items:center;gap:10px;font-weight:650;letter-spacing:-.01em;white-space:nowrap}
.mark{width:28px;height:28px;border-radius:8px;background:linear-gradient(135deg,#6366f1,#1f9d70);display:grid;place-items:center;color:#fff;flex:none}
.mark svg{width:14px;height:14px}
.brand small{display:block;font-weight:500;color:var(--faint);font-size:11.5px;margin-top:-2px}
.nav{display:flex;gap:2px}
.nav a{padding:7px 12px;border-radius:8px;color:var(--muted);font-weight:550;display:flex;align-items:center;gap:7px;white-space:nowrap}
.nav a:hover{background:var(--hover);color:var(--text)}
.nav a.on{background:var(--accent-soft);color:var(--accent-ink)}
.count{min-width:20px;height:20px;padding:0 6px;border-radius:10px;background:var(--accent);color:#fff;font-size:11.5px;font-weight:700;display:inline-grid;place-items:center}
[data-theme=dark] .count{color:#0e0f11}
.ext{color:var(--faint);font-size:11px}
.spacer{flex:1}
.tools{display:flex;gap:8px;align-items:center}
.iconbtn{width:36px;height:36px;border-radius:9px;border:1px solid var(--border);background:var(--surface);color:var(--muted);display:grid;place-items:center;flex:none}
.iconbtn:hover{background:var(--hover);color:var(--text)}
.iconbtn svg{width:18px;height:18px}
.theme-toggle .moon{display:none}[data-theme=dark] .theme-toggle .sun{display:none}[data-theme=dark] .theme-toggle .moon{display:block}

.flash{display:flex;gap:10px;align-items:flex-start;padding:11px 14px;border-radius:12px;border:1px solid var(--ok-soft);background:var(--ok-soft);color:var(--ok-ink);font-weight:550}
.flash svg{width:18px;height:18px;flex:none;margin-top:1px}
.flash.error{background:var(--danger-soft);border-color:var(--danger-line);color:var(--danger)}

.card{background:var(--surface);border:1px solid var(--border);border-radius:14px;box-shadow:var(--shadow)}
.app{height:100vh;height:100dvh}
.inbox{flex:1;min-height:0;display:grid;grid-template-columns:minmax(320px,400px) minmax(0,1fr);grid-template-rows:auto minmax(0,1fr);gap:16px 20px;padding:20px 24px}
.inbox>.flash{grid-column:1/-1}
.inbox>.list,.inbox>.detail{grid-row:2}
.inbox.no-flash{grid-template-rows:minmax(0,1fr)}
.inbox.no-flash>.list,.inbox.no-flash>.detail{grid-row:1}
.list{display:flex;flex-direction:column;overflow:auto;padding-bottom:8px}
.list-head{padding:18px 18px 10px;display:flex;align-items:baseline;justify-content:space-between;gap:12px}
.list-head h2{font-size:17px}
.list-head span{color:var(--faint);font-size:12.5px}
.group{padding:10px 18px 6px;font-size:11.5px;font-weight:650;color:var(--faint);text-transform:uppercase;letter-spacing:.06em}
.item{margin:2px 8px;padding:12px;border-radius:10px;display:grid;grid-template-columns:34px minmax(0,1fr) auto;gap:12px;align-items:start;border:1px solid transparent}
.item:hover{background:var(--hover)}
.item.on{background:var(--accent-soft);border-color:var(--accent-line)}
.item .t{font-weight:600;line-height:1.35;overflow-wrap:anywhere;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.item .t code{font-size:.92em}
.item .s{color:var(--muted);font-size:12.5px;margin-top:4px;display:flex;gap:6px;align-items:center;flex-wrap:wrap;overflow-wrap:anywhere}
.item .age{color:var(--faint);font-size:12px;white-space:nowrap}
.empty{margin:8px 18px 18px;padding:22px 18px;border:1px dashed var(--border-strong);border-radius:12px;color:var(--muted);text-align:center}
.empty b{display:block;color:var(--text);margin-bottom:4px}
.ico{width:34px;height:34px;border-radius:9px;display:grid;place-items:center;flex:none}
.ico svg{width:17px;height:17px}
.ico.gh{background:var(--gh-soft);color:var(--gh)}.ico.push{background:var(--warn-soft);color:var(--warn)}.ico.fetch{background:var(--sky-soft);color:var(--sky)}
.chip{display:inline-flex;align-items:center;gap:4px;min-height:20px;padding:1px 7px;border-radius:6px;font-size:11.5px;font-weight:600;white-space:nowrap}
.chip.red{background:var(--danger-soft);color:var(--danger)}
.chip.gray{background:var(--hover);color:var(--muted)}
.chip.green{background:var(--ok-soft);color:var(--ok-ink)}
.chip.amber{background:var(--warn-soft);color:var(--warn)}
.chip.gh{background:var(--gh-soft);color:var(--gh)}.chip.push{background:var(--warn-soft);color:var(--warn)}.chip.fetch{background:var(--sky-soft);color:var(--sky)}

.detail{display:flex;flex-direction:column;min-height:0;overflow:hidden}
.detail-body{padding:24px 28px 28px;overflow:auto;flex:1;min-height:0}
.detail-body.center{display:flex}
h1 code{font-size:.86em;font-weight:650}
.back{display:none;color:var(--accent-ink);font-weight:600;margin-bottom:12px}
.crumbs{display:flex;align-items:center;justify-content:space-between;gap:12px;flex-wrap:wrap;color:var(--faint);font-size:12.5px;margin-bottom:14px}
.crumbs .links{display:flex;gap:6px;align-items:center;flex-wrap:wrap}
.pill-btn{height:30px;padding:0 11px;border-radius:8px;border:1px solid var(--border);background:var(--surface);color:var(--muted);display:inline-flex;align-items:center;gap:6px;font-size:12.5px;font-weight:550;white-space:nowrap}
a.pill-btn:hover,button.pill-btn:hover{background:var(--hover);color:var(--text)}
.pill-btn.off{opacity:.45;pointer-events:none}
.pill-btn svg{width:14px;height:14px}
.pos{padding:0 4px;color:var(--faint);white-space:nowrap}
.sub{margin-top:6px;color:var(--muted);display:flex;gap:6px 8px;align-items:center;flex-wrap:wrap}
.callout{margin-top:18px;padding:12px 14px;border-radius:10px;background:var(--danger-soft);border:1px solid var(--danger-line);color:var(--danger);display:flex;gap:10px;align-items:flex-start;font-size:13.5px}
.callout svg{width:18px;height:18px;flex:none;margin-top:1px}
.callout b{font-weight:700}
.callout code{color:inherit}
.callout.info{background:var(--surface-2);border-color:var(--border);color:var(--muted)}
.callout.info b{color:var(--text)}
.callout.warn{background:var(--warn-soft);border-color:var(--warn-line);color:var(--warn)}
.callout .rows{display:grid;grid-template-columns:auto minmax(0,1fr);gap:2px 10px;margin-top:6px}
.callout .rows span{opacity:.8}
.facts{margin-top:18px;display:flex;flex-wrap:wrap;border:1px solid var(--border);border-radius:12px;overflow:hidden;background:var(--surface)}
.facts div{flex:1 1 150px;padding:11px 14px;border-right:1px solid var(--border);border-bottom:1px solid var(--border);margin:0 -1px -1px 0;min-width:0}
.facts div.wide{flex-basis:100%}
.facts span{display:block;color:var(--faint);font-size:11.5px;font-weight:600;margin-bottom:2px}
.facts strong,.facts code{font-weight:550;overflow-wrap:anywhere}
.facts .public{color:var(--danger)}
.sec{margin:22px 0 8px;font-size:12px;font-weight:650;color:var(--muted);display:flex;justify-content:space-between;gap:12px;flex-wrap:wrap}
.sec small{color:var(--faint);font-weight:500}
.text{border:1px solid var(--border);border-radius:12px;overflow:hidden;background:var(--code-bg);margin-bottom:10px}
.text .lbl{padding:8px 14px;border-bottom:1px solid var(--border);display:flex;justify-content:space-between;gap:12px;font-size:12px;color:var(--muted);background:var(--surface)}
.text .lbl b{color:var(--text);font-weight:650}
.text pre{margin:0;padding:10px 0;font-size:12.5px;line-height:1.6;white-space:pre-wrap;overflow-wrap:anywhere;tab-size:4}
.text pre>span{display:flex;padding-right:14px}
.text pre>span>i{flex:none;width:44px;text-align:right;padding-right:14px;color:var(--faint);font-style:normal;user-select:none}
.text pre>span>em{font-style:normal;min-width:0;flex:1}
.text pre .whitespace{color:var(--faint)}
.flag{display:flex;justify-content:space-between;align-items:center;padding:9px 14px;border:1px solid var(--border);border-radius:12px;margin-bottom:10px;font-size:13px}
.flag span{color:var(--muted)}

.actions{padding:14px 28px;border-top:1px solid var(--border);display:flex;align-items:center;gap:10px;flex-wrap:wrap;background:var(--surface-2);flex:none;position:relative}
.btn{min-height:40px;padding:0 16px;border-radius:10px;border:1px solid var(--border-strong);background:var(--surface);color:var(--text);font-weight:650;display:inline-flex;align-items:center;justify-content:center;gap:8px;white-space:nowrap}
.btn:hover{background:var(--hover)}
.btn.primary{background:var(--ok);border-color:var(--ok);color:#fff}
.btn.primary:hover{background:var(--ok-hover);border-color:var(--ok-hover)}
.btn.danger{background:var(--danger-soft);border-color:var(--danger-line);color:var(--danger)}
.btn.danger:hover{background:var(--danger-solid);border-color:var(--danger-solid);color:#fff}
.btn.ghost{border-color:transparent;background:transparent;color:var(--muted)}
.btn.ghost:hover{background:var(--hover);color:var(--text)}
.btn.sm{min-height:32px;padding:0 12px;font-size:12.5px;border-radius:8px}
.btn svg{width:16px;height:16px}
.allow{position:relative}
.allow>summary{list-style:none}
.allow>summary::-webkit-details-marker{display:none}
.allow[open]>summary svg{transform:rotate(180deg)}
.allow-pop{position:absolute;bottom:calc(100% + 10px);left:0;width:min(460px,calc(100vw - 48px));padding:16px;border-radius:12px;border:1px solid var(--border);background:var(--surface);box-shadow:var(--pop-shadow);z-index:5;display:grid;gap:10px}
.allow-pop p{color:var(--muted);font-size:13px}
.allow-pop .btn{white-space:normal;text-align:left;justify-content:flex-start;padding:9px 14px;line-height:1.35}

.state{margin:auto;max-width:420px;padding:40px 24px;text-align:center;display:grid;gap:10px;justify-items:center}
.state .big{width:52px;height:52px;border-radius:14px;display:grid;place-items:center;background:var(--ok-soft);color:var(--ok-ink);margin-bottom:6px}
.state .big.gray{background:var(--hover);color:var(--muted)}
.state .big svg{width:24px;height:24px}
.state h2{font-size:20px}
.state p{color:var(--muted)}
.state .btn{margin-top:8px}

.page{width:100%;max-width:1040px;margin:0 auto;padding:32px 24px 48px;display:grid;gap:18px;align-content:start}
.page-h p{margin-top:6px;color:var(--muted)}
.block{overflow:hidden}
.block-h{padding:14px 18px;display:flex;align-items:center;gap:10px;flex-wrap:wrap;border-bottom:1px solid var(--border)}
.block-h h3{font-size:14.5px}
.block-h span{color:var(--faint);font-size:12.5px}
.block-b{padding:18px;display:grid;gap:12px;justify-items:start}
.block-b p{color:var(--muted)}
.row{display:grid;grid-template-columns:minmax(0,1.2fr) minmax(0,1.6fr) 130px auto;gap:8px 16px;align-items:center;padding:12px 18px;border-bottom:1px solid var(--border)}
.row:last-child{border-bottom:0}
.row>div{min-width:0;overflow-wrap:anywhere}
.row .muted{color:var(--muted);font-size:13px;display:flex;gap:6px 8px;align-items:center;flex-wrap:wrap}
.row .when{color:var(--faint);font-size:12.5px}
.row .btn{justify-self:end}
.row details{grid-column:1/-1;min-width:0}
.row details[open]{padding-bottom:6px}
.row details .note{color:var(--muted);font-size:13px}
.row summary{color:var(--accent-ink);font-size:12.5px;cursor:pointer;margin-bottom:8px}
.none{padding:14px 18px;color:var(--faint)}
.pair-url{width:100%;max-width:640px;height:40px;padding:0 12px;border-radius:10px;border:1px solid var(--border-strong);background:var(--surface-2);color:var(--text);font:12.5px ui-monospace,"SF Mono",Menlo,"DejaVu Sans Mono",monospace}
.field{display:grid;gap:6px;width:100%;font-weight:600}
.field small{color:var(--muted);font-weight:400}
.danger-zone{border:1px solid var(--danger-line);background:var(--danger-soft);border-radius:12px;padding:14px;display:grid;gap:10px;justify-items:start;width:100%}
.danger-zone summary{color:var(--danger);font-weight:650;cursor:pointer}
.danger-zone p{color:var(--muted)}
.linkrow{display:flex;justify-content:space-between;align-items:center;gap:12px;padding:12px 18px;border-bottom:1px solid var(--border)}
.linkrow:last-child{border-bottom:0}
.linkrow:hover{background:var(--hover)}
.linkrow span{color:var(--muted);font-size:13px}

.lock{flex:1;display:grid;place-items:center;padding:24px;background:radial-gradient(640px 420px at 50% 0%,var(--accent-soft),transparent)}
.lock .card{width:100%;max-width:380px;padding:30px 26px;text-align:center;display:grid;gap:12px}
.lock .mark{width:44px;height:44px;border-radius:12px;margin:0 auto 6px}
.lock .mark svg{width:20px;height:20px}
.lock h1{font-size:21px}
.lock p{color:var(--muted);font-size:13.5px}
.lock form{display:grid;gap:12px;margin-top:8px}
.pin{height:50px;border-radius:10px;border:1px solid var(--border-strong);background:var(--surface-2);color:var(--text);text-align:center;font-size:20px;letter-spacing:.3em;width:100%;outline:none}
.pin:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}
.lock .btn{width:100%;min-height:46px}
.lock .flash{text-align:left}
.lock .foot{font-size:12.5px;color:var(--faint)}
.lock .floating{position:fixed;top:14px;right:14px}

@media (max-width:900px){
  .top{flex-wrap:wrap;gap:0 12px;padding:10px 14px 0;min-height:0}
  .brand{flex:1;min-height:40px}
  .nav{order:3;width:calc(100% + 28px);margin:8px -14px 0;padding:6px 10px;overflow-x:auto;border-top:1px solid var(--border);scrollbar-width:none}
  .nav::-webkit-scrollbar{display:none}
  .inbox{display:flex;flex-direction:column;height:auto;padding:14px;gap:14px}
  .inbox.at-root>.detail,.inbox.at-item>.list{display:none}
  .inbox.at-root .item.on{background:transparent;border-color:transparent}
  .nav a{padding:6px 9px}
  .list{overflow:visible}
  .detail{overflow:visible}
  .detail-body{padding:18px 16px 22px;overflow:visible}
  .back{display:inline-block}
  h1{font-size:23px}
  .actions{position:sticky;bottom:0;padding:12px 16px calc(12px + env(safe-area-inset-bottom));border-radius:0 0 14px 14px;background:color-mix(in srgb,var(--surface-2) 94%,transparent);backdrop-filter:blur(10px)}
  .actions>.btn,.actions>form>.btn{flex:1}
  .actions .allow{width:100%;text-align:center}
  .actions .allow>summary{width:100%}
  .allow-pop{left:0;right:0;width:auto}
  .page{padding:20px 14px 36px}
  .row{grid-template-columns:minmax(0,1fr) auto;padding:12px 14px}
  .row .muted,.row .when{grid-column:1}
  .row .btn{grid-row:1;grid-column:2}
  .facts div{flex-basis:130px}
  .facts div.wide{flex-basis:100%}
  .app{height:auto}
}
"#;

pub const ICON_GH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="6" cy="6" r="2.5"/><circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="18" r="2.5"/><path d="M6 8.5v7M18 15.5V10a3 3 0 0 0-3-3h-4"/><path d="M13 5l-2 2 2 2"/></svg>"#;
pub const ICON_PUSH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 19V5M6 11l6-6 6 6"/></svg>"#;
pub const ICON_FETCH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 5v14M6 13l6 6 6-6"/></svg>"#;
pub const ICON_WARN: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3l9.5 17h-19z"/><path d="M12 10v4M12 17.5v.01"/></svg>"#;
pub const ICON_INFO: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 7.5v.01"/></svg>"#;
pub const ICON_CHECK: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>"#;
pub const ICON_LINK: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/></svg>"#;
pub const ICON_CHEVRON: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 9l6 6 6-6"/></svg>"#;
pub const ICON_PREV: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 6l-6 6 6 6"/></svg>"#;
pub const ICON_NEXT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>"#;
pub const ICON_INBOX: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 13l2.5-7h11L20 13v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1z"/><path d="M4 13h4.5l1 2h5l1-2H20"/></svg>"#;
const ICON_MARK: &str = r#"<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 2l10 10-10 10L2 12z"/></svg>"#;
const ICON_SUN: &str = r#"<svg class="sun" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>"#;
const ICON_MOON: &str = r#"<svg class="moon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z"/></svg>"#;
const ICON_LOCK: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg>"#;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Inbox,
    Access,
    System,
}

pub fn mark() -> &'static str {
    ICON_MARK
}

pub fn theme_toggle() -> String {
    format!(
        r#"<button type="button" class="iconbtn theme-toggle" data-theme-toggle title="Switch between light and dark mode" aria-label="Switch between light and dark mode">{ICON_SUN}{ICON_MOON}</button>"#
    )
}

pub fn document(title: &str, body_class: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover"><meta name="color-scheme" content="light dark"><title>{title} · Sandbox Admin</title><script src="/assets/portal.js"></script><style>{STYLE}</style></head><body class="{body_class}">{body}</body></html>"#
    )
}

pub fn top_bar(active: Nav, pending: usize, port: u16, csrf: &str) -> String {
    let tab = |nav: Nav, href: &str, label: &str, extra: &str| {
        let class = if nav == active { " class=\"on\"" } else { "" };
        format!(r#"<a{class} href="{href}">{label}{extra}</a>"#)
    };
    let badge = if pending > 0 {
        format!(r#"<span class="count">{pending}</span>"#)
    } else {
        String::new()
    };
    format!(
        r#"<header class="top"><a class="brand" href="/"><span class="mark">{ICON_MARK}</span><span>Sandbox Admin<small>claude-sandbox · port {port}</small></span></a><nav class="nav">{}{}<a href="/display" target="_blank" rel="noopener">Display<span class="ext">↗</span></a><a href="/usage" target="_blank" rel="noopener">Usage<span class="ext">↗</span></a>{}</nav><div class="spacer"></div><div class="tools">{}<form method="post" action="/lock"><input type="hidden" name="csrf" value="{csrf}"><button class="iconbtn" title="Lock the portal in this browser" aria-label="Lock">{ICON_LOCK}</button></form></div></header>"#,
        tab(Nav::Inbox, "/", "Inbox", &badge),
        tab(Nav::Access, "/access", "Access", ""),
        tab(Nav::System, "/system", "System", ""),
        theme_toggle(),
    )
}
