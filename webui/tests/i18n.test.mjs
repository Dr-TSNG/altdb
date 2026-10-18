import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { messages, preference, resolveLocale, translate, translateBackend } from '../src/i18n.ts';

test('system language matching, unsupported languages and explicit overrides', () => {
  assert.equal(resolveLocale('auto', ['zh-Hans-CN', 'en-US']), 'zh-CN');
  assert.equal(resolveLocale('auto', ['zh_TW']), 'zh-CN');
  assert.equal(resolveLocale('auto', ['en-GB', 'zh-CN']), 'en');
  assert.equal(resolveLocale('auto', ['de-DE', 'zh-CN']), 'zh-CN');
  assert.equal(resolveLocale('auto', ['ja-JP']), 'en');
  assert.equal(resolveLocale('auto', []), 'en');
  assert.equal(resolveLocale('en', ['zh-CN']), 'en');
  assert.equal(resolveLocale('zh-CN', ['en-US']), 'zh-CN');
  assert.equal(preference(null), 'auto');
  assert.equal(preference('corrupt preference'), 'auto');
  assert.equal(preference('en'), 'en');
  assert.equal(preference('zh-CN'), 'zh-CN');
});

test('every translation and HTML label has matching keys and placeholders', () => {
  assert.deepEqual(Object.keys(messages.en).sort(), Object.keys(messages['zh-CN']).sort());
  const placeholders = (text) => [...text.matchAll(/\{(\w+)\}/g)].map((match) => match[1]).sort();
  for (const key of Object.keys(messages.en)) {
    assert.ok(messages['zh-CN'][key].trim(), key);
    assert.deepEqual(placeholders(messages.en[key]), placeholders(messages['zh-CN'][key]), key);
  }
  const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
  for (const [, key] of html.matchAll(/data-i18n(?:-aria-label)?="([^"]+)"/g)) {
    assert.ok(Object.hasOwn(messages.en, key), key);
  }
});

test('dynamic values remain literal in either language', () => {
  assert.equal(translate('pair_remaining', 'en', { time: '4:03' }), 'Closes in 4:03');
  assert.equal(translate('pair_remaining', 'zh-CN', { time: '4:03' }), '4:03 后关闭');
  assert.equal(
    translate('last_connected', 'en', { date: '$& <script>' }),
    'Last connected: $& <script>',
  );
  assert.equal(translateBackend('Disabled in settings', 'zh-CN'), '已在设置中停用');
  assert.equal(translateBackend('Service started', 'en'), 'Service started');
  assert.equal(
    translateBackend('Unknown diagnostic $& <script>', 'zh-CN'),
    'Unknown diagnostic $& <script>',
  );
  assert.equal(translateBackend('constructor', 'zh-CN'), 'constructor');
  assert.equal(
    translateBackend(
      'Cannot read system debugging state; service paused: Cannot determine native adbd state',
      'zh-CN',
    ),
    '无法读取系统调试状态，服务已暂停：无法确认原生 adbd 的运行状态',
  );
  assert.equal(
    translateBackend('Cannot read system debugging state; service paused: $&', 'zh-CN'),
    '无法读取系统调试状态，服务已暂停：$&',
  );
});

test('CLI-owned status messages stay English', () => {
  for (const file of ['src/main.rs', 'src/unix/daemon.rs', 'src/unix/ksu.rs']) {
    const source = readFileSync(new URL(`../../${file}`, import.meta.url), 'utf8');
    assert.doesNotMatch(source, /\p{Script=Han}/u, file);
  }
});
