#!/usr/bin/env node
'use strict';
const { spawnSync } = require('node:child_process');
const path = require('node:path');
const key = `${process.platform}-${process.arch}`;
const packages = {
  'win32-x64': ['@dscode/win32-x64', 'dscode.exe'],
  'linux-x64': ['@dscode/linux-x64', 'dscode'],
  'linux-arm64': ['@dscode/linux-arm64', 'dscode'],
  'darwin-x64': ['@dscode/darwin-x64', 'dscode'],
  'darwin-arm64': ['@dscode/darwin-arm64', 'dscode'],
};
const selected = packages[key];
if (!selected) {
  console.error(`DSCode 不支持当前平台：${key}`);
  process.exit(1);
}
let manifest;
try {
  manifest = require.resolve(`${selected[0]}/package.json`);
} catch {
  console.error(`缺少平台包 ${selected[0]}；请确认 npm 未禁用 optionalDependencies。`);
  process.exit(1);
}
const binary = path.join(path.dirname(manifest), 'bin', selected[1]);
const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(`启动 DSCode 失败：${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
