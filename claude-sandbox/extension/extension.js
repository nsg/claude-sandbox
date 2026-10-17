import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Soup from 'gi://Soup';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const DEFAULT_URL = 'http://127.0.0.1:3774';
const POLL_SECONDS = 60;
const BAR_WIDTH = 150;
const DIMMED = 140;
const PROVIDERS = [
    {key: 'anthropic', name: 'Anthropic', letter: 'A'},
    {key: 'openai', name: 'OpenAI', letter: 'O'},
    {key: 'ollama', name: 'Ollama', letter: 'L'},
];

function levelClass(percent) {
    if (percent >= 90)
        return 'plan-usage-crit';
    return percent >= 70 ? 'plan-usage-warn' : null;
}

function capitalize(text) {
    return text.charAt(0).toUpperCase() + text.slice(1);
}

function bucketName(bucket) {
    const period = typeof bucket.period === 'string' && bucket.period !== 'other'
        ? capitalize(bucket.period) : null;
    const label = typeof bucket.label === 'string' ? bucket.label : null;
    if (!period)
        return label ?? 'Limit';
    if (!label || label.toLowerCase() === period.toLowerCase())
        return period;
    return `${period} · ${label}`;
}

function parseTime(value) {
    const time = typeof value === 'string' ? Date.parse(value) : NaN;
    return Number.isNaN(time) ? null : time;
}

function parseUsage(data) {
    return PROVIDERS.map(provider => {
        const raw = data?.providers?.[provider.key] ?? {};
        const buckets = (Array.isArray(raw.buckets) ? raw.buckets : [])
            .filter(bucket => Number.isFinite(bucket?.used_percent))
            .map(bucket => ({
                name: bucketName(bucket),
                percent: Math.max(0, Math.min(100, Math.round(bucket.used_percent))),
                resetsAt: parseTime(bucket.resets_at),
            }));
        return {
            ...provider,
            buckets,
            freshness: ['fresh', 'stale'].includes(raw.freshness) ? raw.freshness : 'unknown',
            updatedAt: parseTime(raw.updated_at),
            peak: buckets.length ? Math.max(...buckets.map(bucket => bucket.percent)) : null,
        };
    });
}

function formatDuration(milliseconds) {
    const minutes = Math.max(1, Math.round(milliseconds / 60000));
    if (minutes < 60)
        return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24)
        return `${hours}h ${minutes % 60}m`;
    return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

function formatReset(resetsAt, now) {
    if (resetsAt === null)
        return '—';
    return resetsAt <= now ? 'now' : `in ${formatDuration(resetsAt - now)}`;
}

function formatAge(updatedAt, now) {
    if (updatedAt === null)
        return null;
    const minutes = Math.floor((now - updatedAt) / 60000);
    if (minutes < 1)
        return 'just now';
    if (minutes < 60)
        return `${minutes} min ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24)
        return `${hours} h ago`;
    const days = Math.floor(hours / 24);
    return `${days} ${days === 1 ? 'day' : 'days'} ago`;
}

function formatFreshness(provider, now) {
    const age = formatAge(provider.updatedAt, now);
    if (provider.freshness === 'unknown')
        return 'no data';
    if (provider.freshness === 'stale')
        return age ? `stale · ${age}` : 'stale';
    return age ?? 'fresh';
}

function formatSummary(providers, now) {
    const fresh = providers.filter(provider => provider.freshness === 'fresh' && provider.peak !== null);
    if (!fresh.length)
        return 'No fresh data';
    const highest = fresh.reduce((top, provider) => provider.peak > top.peak ? provider : top);
    const resets = fresh
        .flatMap(provider => provider.buckets.map(bucket => bucket.resetsAt))
        .filter(resetsAt => resetsAt !== null && resetsAt > now);
    const summary = `Highest: ${highest.peak}% · ${highest.name}`;
    return resets.length
        ? `${summary} · next reset in ${formatDuration(Math.min(...resets) - now)}`
        : summary;
}

function dimmedLabel(text, styleClass = null) {
    return new St.Label({
        text,
        style_class: styleClass,
        opacity: DIMMED,
        y_align: Clutter.ActorAlign.CENTER,
    });
}

const Indicator = GObject.registerClass(
class Indicator extends PanelMenu.Button {
    _init(url) {
        super._init(0.5, 'Plan usage');

        this._url = url;
        this._providers = null;
        this._session = new Soup.Session({timeout: 5});
        this._cancellable = new Gio.Cancellable();

        this._panelBox = new St.BoxLayout({
            style_class: 'plan-usage-panel',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this.add_child(this._panelBox);

        // Popups are light or dark depending on the shell theme; the panel is not.
        this.menu.box.connect('style-changed', box => {
            const color = box.get_theme_node().get_foreground_color();
            const luminance = 0.299 * color.red + 0.587 * color.green + 0.114 * color.blue;
            if (luminance < 128)
                box.add_style_class_name('plan-usage-light');
            else
                box.remove_style_class_name('plan-usage-light');
        });
        this.menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._renderMenu();
        });

        this._render();
        this._refresh();
        this._timeoutId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, POLL_SECONDS, () => {
            this._refresh();
            return GLib.SOURCE_CONTINUE;
        });
    }

    destroy() {
        GLib.source_remove(this._timeoutId);
        this._cancellable.cancel();
        this._session.abort();
        super.destroy();
    }

    _refresh() {
        const message = Soup.Message.new('GET', `${this._url}/api/usage`);
        if (!message) {
            this._update(null);
            return;
        }
        this._session.send_and_read_async(message, GLib.PRIORITY_DEFAULT, this._cancellable,
            (session, result) => {
                let providers = null;
                try {
                    const bytes = session.send_and_read_finish(result);
                    // 503 carries a valid body in which every provider is unknown.
                    const data = JSON.parse(new TextDecoder().decode(bytes.get_data()));
                    if (data?.schema_version === 1)
                        providers = parseUsage(data);
                } catch (error) {
                    if (error instanceof GLib.Error &&
                        error.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                        return;
                }
                this._update(providers);
            });
    }

    _update(providers) {
        this._providers = providers;
        this._render();
    }

    _render() {
        this._renderPanel();
        this._renderMenu();
    }

    _renderPanel() {
        this._panelBox.destroy_all_children();
        PROVIDERS.forEach((provider, index) => {
            if (index > 0)
                this._panelBox.add_child(dimmedLabel('·'));
            const usage = this._providers?.[index];
            const known = usage && usage.freshness !== 'unknown' && usage.peak !== null;
            const label = new St.Label({
                text: `${provider.letter} ${known ? usage.peak : '–'}`,
                y_align: Clutter.ActorAlign.CENTER,
            });
            if (known && usage.freshness === 'fresh') {
                const level = levelClass(usage.peak);
                if (level)
                    label.add_style_class_name(level);
            } else {
                label.opacity = DIMMED;
            }
            this._panelBox.add_child(label);
        });
    }

    _renderMenu() {
        const now = Date.now();
        this.menu.removeAll();

        const title = this._addItem();
        title.add_child(new St.Label({
            text: 'Plan usage',
            style_class: 'plan-usage-title',
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        }));
        if (this._providers)
            title.add_child(dimmedLabel(formatSummary(this._providers, now), 'plan-usage-small'));

        if (this._providers) {
            for (const provider of this._providers) {
                this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
                this._addItem().add_child(this._buildProvider(provider, now));
            }
        } else {
            const message = new St.Label({
                text: `No usage data from ${this._url}. The usage service runs with ` +
                    'claude-sandbox --portal t3code.',
                style_class: 'plan-usage-message',
                opacity: DIMMED,
            });
            message.clutter_text.line_wrap = true;
            this._addItem().add_child(message);
        }

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const dashboard = new PopupMenu.PopupMenuItem('Open usage dashboard');
        dashboard.setOrnament(PopupMenu.Ornament.HIDDEN);
        dashboard.connect('activate', () => {
            try {
                Gio.AppInfo.launch_default_for_uri(`${this._url}/usage`,
                    global.create_app_launch_context(0, -1));
            } catch (error) {
                Main.notifyError('Could not open the usage dashboard', error.message);
            }
        });
        this.menu.addMenuItem(dashboard);
    }

    _addItem() {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        // St marks every non-reactive widget insensitive, which themes dim.
        item.remove_style_pseudo_class('insensitive');
        item.setOrnament(PopupMenu.Ornament.HIDDEN);
        this.menu.addMenuItem(item);
        return item;
    }

    _buildProvider(provider, now) {
        const stale = provider.freshness !== 'fresh';
        const box = new St.BoxLayout({
            vertical: true,
            x_expand: true,
            style_class: 'plan-usage-provider',
        });

        const head = new St.BoxLayout();
        head.add_child(new St.Label({
            text: provider.name,
            style_class: 'plan-usage-provider-name',
            x_expand: true,
            opacity: stale ? DIMMED : 255,
        }));
        head.add_child(dimmedLabel(formatFreshness(provider, now), 'plan-usage-small'));
        box.add_child(head);

        if (!provider.buckets.length && provider.freshness !== 'unknown')
            box.add_child(dimmedLabel('No limits reported', 'plan-usage-small'));

        for (const bucket of provider.buckets) {
            const level = stale ? null : levelClass(bucket.percent);
            const row = new St.BoxLayout({
                style_class: 'plan-usage-row',
                opacity: stale ? DIMMED : 255,
            });
            row.add_child(new St.Label({
                text: bucket.name,
                style_class: 'plan-usage-bucket',
                x_expand: true,
                y_align: Clutter.ActorAlign.CENTER,
            }));

            const bar = new St.BoxLayout({
                style_class: 'plan-usage-bar',
                y_align: Clutter.ActorAlign.CENTER,
            });
            const fill = new St.Widget({
                style_class: 'plan-usage-fill',
                style: `width: ${Math.round(BAR_WIDTH * bucket.percent / 100)}px;`,
                visible: bucket.percent > 0,
            });
            bar.add_child(fill);
            const percent = new St.Label({
                text: `${bucket.percent}%`,
                style_class: 'plan-usage-percent',
                y_align: Clutter.ActorAlign.CENTER,
            });
            if (level) {
                fill.add_style_class_name(level);
                percent.add_style_class_name(level);
            }
            row.add_child(bar);
            row.add_child(percent);
            row.add_child(dimmedLabel(formatReset(bucket.resetsAt, now), 'plan-usage-reset plan-usage-small'));
            box.add_child(row);
        }
        return box;
    }
});

export default class PlanUsageExtension extends Extension {
    enable() {
        this._indicator = new Indicator(this._readUrl());
        Main.panel.addToStatusArea(this.uuid, this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }

    _readUrl() {
        try {
            const [, contents] = this.dir.get_child('config.json').load_contents(null);
            const url = JSON.parse(new TextDecoder().decode(contents)).url;
            if (typeof url === 'string' && url)
                return url.replace(/\/+$/, '');
        } catch (_) {
            // Fall through to the default admin port.
        }
        return DEFAULT_URL;
    }
}
