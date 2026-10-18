import { exec } from 'kernelsu';
import {
  languageStorageKey,
  preference,
  resolveLocale,
  translate,
  translateBackend,
  type MessageKey,
} from './i18n';
import './style.css';

type Config = {
  enabled: boolean;
  port_mode: 'random' | 'fixed';
  fixed_port: number;
  mdns_enabled: boolean;
  allow_adb_root: boolean;
};
type Host = { fingerprint: string; name: string; paired_at: number; last_connected: number | null };
type Status = {
  version: string;
  state: string;
  reason: string;
  root: boolean;
  config: Config;
  hosts: Host[];
  connections: number;
  endpoints: string[];
  pairing: null | { code: string; expires_at: number; failures: number; endpoints: string[] };
};
type Log = { time: number; message: string };
const get = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const stateNames: Record<string, MessageKey> = {
  running: 'state_running',
  paused_native: 'state_paused_native',
  disabled: 'state_disabled',
  error: 'state_error',
  waiting_network: 'state_waiting_network',
  starting: 'state_starting',
};
let choice = preference(null);
try {
  choice = preference(localStorage.getItem(languageStorageKey));
} catch {
  /* Storage may be disabled in WebView. */
}
let locale = resolveLocale(choice, navigator.languages);
const t = (key: MessageKey, params?: Record<string, string | number>) =>
  translate(key, locale, params);
let current: Status | undefined;
let initialized = false;
let busy = false;
let polling = false;
let unavailable = false;
let hostSignature = '';
let logs: Log[] | undefined;
let lastNotice: { key: MessageKey; error: boolean; detail?: string } | undefined;

class UiError extends Error {
  constructor(readonly key: MessageKey) {
    super(key);
  }
}
function renderNotice() {
  if (!lastNotice) return;
  const node = get('notice');
  node.textContent = t(lastNotice.key, {
    detail: translateBackend(lastNotice.detail || '', locale),
  });
  node.hidden = false;
  node.classList.toggle('error', lastNotice.error);
}
function notice(key: MessageKey, error = false, detail?: string) {
  lastNotice = { key, error, detail };
  renderNotice();
}
function failure(error: unknown) {
  if (error instanceof UiError) notice(error.key, true);
  else notice('error_detail', true, error instanceof Error ? error.message : String(error));
}
function payload(value: unknown): string {
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  return btoa(Array.from(bytes, (byte) => String.fromCharCode(byte)).join(''))
    .replaceAll('+', '-')
    .replaceAll('/', '_')
    .replace(/=+$/, '');
}
async function rpc<T>(request: object): Promise<T> {
  // Only a fixed binary and a base64url alphabet enter the shell command.
  const encoded = payload(request);
  if (!/^[A-Za-z0-9_-]+$/.test(encoded)) throw new UiError('encoding_failed');
  const result = await exec(`/data/adb/modules/altdb/bin/altdb ctl --request '${encoded}'`);
  if (!result.stdout.trim()) throw new UiError('no_service');
  let reply: { ok: boolean; data?: T; error?: string };
  try {
    reply = JSON.parse(result.stdout);
  } catch {
    throw new UiError('invalid_reply');
  }
  if (!reply || typeof reply.ok !== 'boolean') throw new UiError('invalid_reply');
  if (!reply.ok || result.errno !== 0) {
    if (reply.error) throw new Error(reply.error);
    throw new UiError('failed');
  }
  return reply.data as T;
}
async function action(request: object) {
  if (busy) return;
  busy = true;
  get<HTMLButtonElement>('save').disabled = true;
  try {
    const status = await rpc<Status>(request);
    unavailable = false;
    render(status);
    notice('updated');
  } catch (error) {
    failure(error);
  } finally {
    busy = false;
    get<HTMLButtonElement>('save').disabled = false;
  }
}
async function copy(text: string) {
  try {
    if (navigator.clipboard) await navigator.clipboard.writeText(text);
    else {
      const area = document.createElement('textarea');
      area.value = text;
      document.body.append(area);
      area.select();
      try {
        if (!document.execCommand('copy')) throw new Error('Clipboard unavailable');
      } finally {
        area.remove();
      }
    }
    notice('copied');
  } catch {
    notice('copy_failed', true);
  }
}
function commands(id: string, addresses: string[], verb: string) {
  const parent = get(id);
  const signature = JSON.stringify([locale, verb, addresses]);
  if (parent.dataset.signature === signature) return;
  parent.dataset.signature = signature;
  parent.replaceChildren();
  for (const address of addresses) {
    const row = document.createElement('div');
    row.className = 'command';
    const code = document.createElement('code');
    code.textContent = `adb ${verb} ${address}`;
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'quiet';
    button.textContent = t('copy');
    button.onclick = () => {
      void copy(code.textContent || '');
    };
    row.append(code, button);
    parent.append(row);
  }
}
function render(status: Status) {
  current = status;
  get('state').textContent = unavailable
    ? t('unavailable')
    : stateNames[status.state]
      ? t(stateNames[status.state])
      : status.state;
  get('state').classList.toggle('active', !unavailable && status.state === 'running');
  get('identity').textContent = status.root ? 'root' : 'shell';
  get('reason').textContent = status.reason
    ? translateBackend(status.reason, locale)
    : t('tls_only');
  get('version').textContent = status.version;
  get('connections').textContent = String(status.connections);
  get('connection-hint').textContent = t(
    status.config.mdns_enabled ? 'connection_hint' : 'connection_manual_hint',
  );
  get('host-count').textContent = String(status.hosts.length);
  commands('endpoints', status.endpoints, 'connect');
  get('pair-panel').hidden = !status.pairing;
  get('pair-code').textContent = status.pairing?.code || '';
  commands('pair-endpoints', status.pairing?.endpoints || [], 'pair');
  const toggle = get<HTMLButtonElement>('pair-toggle');
  toggle.textContent = t(status.pairing ? 'pair_stop' : 'pair_start');
  toggle.disabled = unavailable || status.state !== 'running';
  if (status.pairing) {
    const left = Math.max(0, status.pairing.expires_at - Math.floor(Date.now() / 1000));
    get('pair-time').textContent = t('pair_remaining', {
      time: `${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}`,
    });
  } else get('pair-time').textContent = t('pair_manual');
  if (!initialized) {
    populate(status.config);
    initialized = true;
  }
  const signature = JSON.stringify([locale, status.hosts]);
  if (signature !== hostSignature) {
    hostSignature = signature;
    const container = get('hosts');
    container.replaceChildren();
    if (!status.hosts.length) {
      const empty = document.createElement('p');
      empty.className = 'muted';
      empty.textContent = t('no_hosts');
      container.append(empty);
    }
    for (const host of status.hosts) {
      const row = document.createElement('article');
      row.className = 'host';
      const detail = document.createElement('div');
      const name = document.createElement('strong');
      name.textContent = host.name;
      const fp = document.createElement('code');
      fp.textContent = host.fingerprint;
      const date = document.createElement('small');
      date.textContent = host.last_connected
        ? t('last_connected', { date: new Date(host.last_connected * 1000).toLocaleString(locale) })
        : t('never_connected');
      detail.append(name, fp, date);
      const revoke = document.createElement('button');
      revoke.className = 'quiet danger';
      revoke.textContent = t('revoke');
      revoke.onclick = () => {
        void action({ op: 'revoke', fingerprint: host.fingerprint });
      };
      row.append(detail, revoke);
      container.append(row);
    }
  }
}
function renderLogs() {
  get('logs').textContent =
    logs === undefined
      ? t('logs_prompt')
      : logs
          .map(
            (row) =>
              `${new Date(row.time * 1000).toLocaleTimeString(locale)}  ${translateBackend(row.message, locale)}`,
          )
          .join('\n') || t('no_logs');
}
function localize() {
  locale = resolveLocale(choice, navigator.languages);
  document.documentElement.lang = locale;
  get<HTMLSelectElement>('language').value = choice;
  document.querySelectorAll<HTMLElement>('[data-i18n]').forEach((node) => {
    node.textContent = t(node.dataset.i18n as MessageKey);
  });
  document.querySelectorAll<HTMLElement>('[data-i18n-aria-label]').forEach((node) => {
    node.setAttribute('aria-label', t(node.dataset.i18nAriaLabel as MessageKey));
  });
  if (current) render(current);
  else if (unavailable) get('state').textContent = t('unavailable');
  renderNotice();
  renderLogs();
}
function populate(config: Config) {
  get<HTMLInputElement>('enabled').checked = config.enabled;
  get<HTMLSelectElement>('port-mode').value = config.port_mode;
  get<HTMLInputElement>('fixed-port').value = String(config.fixed_port);
  get<HTMLInputElement>('mdns-enabled').checked = config.mdns_enabled;
  get<HTMLInputElement>('allow-adb-root').checked = config.allow_adb_root;
  portField();
}
function portField() {
  get('fixed-field').hidden = get<HTMLSelectElement>('port-mode').value !== 'fixed';
}
get('language').addEventListener('change', () => {
  choice = preference(get<HTMLSelectElement>('language').value);
  try {
    localStorage.setItem(languageStorageKey, choice);
  } catch {
    /* Keep the choice for this page session. */
  }
  localize();
});
window.addEventListener('languagechange', () => {
  if (choice === 'auto') localize();
});
get('port-mode').addEventListener('change', portField);
get('pair-toggle').addEventListener('click', () => {
  void action({ op: current?.pairing ? 'pair_stop' : 'pair_start' });
});
get('settings').addEventListener('submit', (event) => {
  event.preventDefault();
  const port = Number(get<HTMLInputElement>('fixed-port').value);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) {
    notice('invalid_port', true);
    return;
  }
  const config: Config = {
    enabled: get<HTMLInputElement>('enabled').checked,
    port_mode: get<HTMLSelectElement>('port-mode').value as Config['port_mode'],
    fixed_port: port,
    mdns_enabled: get<HTMLInputElement>('mdns-enabled').checked,
    allow_adb_root: get<HTMLInputElement>('allow-adb-root').checked,
  };
  void action({ op: 'configure', config });
});
get('refresh-logs').addEventListener('click', async () => {
  try {
    logs = await rpc<Log[]>({ op: 'logs' });
    renderLogs();
  } catch (error) {
    failure(error);
  }
});
async function refresh() {
  if (busy || polling || document.hidden) return;
  polling = true;
  try {
    const status = await rpc<Status>({ op: 'status' });
    unavailable = false;
    render(status);
  } catch (error) {
    unavailable = true;
    if (current) render(current);
    else get('state').textContent = t('unavailable');
    failure(error);
  } finally {
    polling = false;
  }
}
localize();
void refresh();
setInterval(() => {
  void refresh();
}, 1000);
