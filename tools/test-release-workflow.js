const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const release = fs.readFileSync(path.join(root, '.github/workflows/release.yml'), 'utf8');
const windows = fs.readFileSync(path.join(root, '.github/workflows/windows-build.yml'), 'utf8');

test('release workflow only runs on v tags and manual rehearsal', () => {
  assert.match(release, /push:\s*\n\s*tags:\s*\n\s*- "v\*"/);
  assert.match(release, /workflow_dispatch:/);
  assert.ok(release.includes('手动触发只构建，不创建 release'));
});

test('release preflight requires tag, versions and master history', () => {
  assert.ok(release.includes("require('./apps/mixer-gui/tauri.conf.json').version"));
  assert.ok(release.includes("p.name === 'mixer-gui'"));
  assert.ok(release.includes('cargo metadata --locked --format-version 1'));
  assert.ok(release.includes('git merge-base --is-ancestor HEAD origin/master'));
  assert.ok(release.includes('refs/tags/${TAG}^{commit}'));
});

test('release workflow builds both platforms with ASCII asset names', () => {
  assert.ok(release.includes('uses: ./.github/workflows/windows-build.yml'));
  assert.ok(release.includes('MAC="release-assets/macos/mixer-gui_${VERSION}_macos.dmg"'));
  assert.ok(release.includes('WIN="release-assets/windows/mixer-gui_${VERSION}_windows_x64_setup.exe"'));
  assert.ok(release.includes('sha256sum -c SHA256SUMS.txt'));
  assert.ok(release.includes('MIXER_TARGET: universal-apple-darwin'));
  assert.ok(release.includes('name: macos-dmg\n          path: dist/'));
  assert.ok(release.includes('cargo metadata --locked --format-version 1'));
  assert.ok(windows.includes('name: windows-nsis\n          path: dist/'));
  assert.ok(windows.includes('$assetName = "mixer-gui_${version}_windows_x64_setup.exe"'));
  assert.ok(windows.includes('cargo metadata --locked --format-version 1'));
});

test('release is draft-only and manual runs never publish', () => {
  assert.ok(release.includes("if: github.event_name == 'push'"));
  assert.ok(release.includes('--draft'));
  assert.ok(release.includes('--verify-tag'));
  assert.equal(release.includes('--latest'), false);
  assert.equal(release.includes('--clobber'), false);
  assert.ok(release.includes('contents: write'));
  assert.ok(release.includes('--title "${PRODUCT} ${TAG}"'));
  assert.ok(release.includes('gh release edit "$TAG" --notes-file release-body.md'));
});

test('Windows package workflow is callable and produces a checksummed installer', () => {
  assert.match(windows, /workflow_dispatch:/);
  assert.match(windows, /workflow_call:/);
  assert.match(windows, /build-app-windows\.ps1 -SkipSmoke/);
  assert.match(windows, /installer\.nsi/);
  assert.match(windows, /Get-FileHash -LiteralPath \$assetPath -Algorithm SHA256/);
  assert.match(windows, /if \(\$installers\.Count -ne 1\)/);
});
