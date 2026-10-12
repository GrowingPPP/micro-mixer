const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const root = path.resolve(__dirname, '..');
const config = JSON.parse(fs.readFileSync(path.join(root, 'apps/mixer-gui/tauri.conf.json'), 'utf8'));
const scripts = ['apps/mixer-gui/build-app.sh', 'm0/tools/app_verify.sh', 'tools/app-product-name.sh'];

function run(script, argument, projectRoot) {
  const result = spawnSync('bash', [path.join(projectRoot, script), argument], {
    cwd: os.tmpdir(), encoding: 'utf8', env: process.env,
  });
  assert.equal(result.error, undefined);
  return result;
}

function expectPaths(projectRoot, productName, version) {
  const app = path.join(projectRoot, 'target/release/bundle/macos', `${productName}.app`);
  const dmg = path.join(projectRoot, 'target/release/bundle/dmg', `${productName}_${version}.dmg`);
  const build = run('apps/mixer-gui/build-app.sh', '--print-artifact-paths', projectRoot);
  assert.equal(build.status, 0, build.stderr);
  assert.deepEqual(build.stdout.trim().split(/\r?\n/), [app, dmg]);
  const verify = run('m0/tools/app_verify.sh', '--print-app-path', projectRoot);
  assert.equal(verify.status, 0, verify.stderr);
  assert.equal(verify.stdout.trim(), app);
  const note = run('apps/mixer-gui/build-app.sh', '--print-first-open-note', projectRoot);
  assert.equal(note.status, 0, note.stderr);
  assert.ok(note.stdout.includes(`把「${productName}」拖进旁边的 Applications 文件夹`));
  assert.ok(note.stdout.includes('本程序把人声与音频混合后写进去'));
  assert.equal(note.stdout.includes('$APP_NAME'), false, '安装说明必须真正展开产品名');
  assert.equal(fs.existsSync(path.join(projectRoot, 'target')), false,
    '路径查看不能构建或创建产物');
}

// Windows executes its native packaging regressions through -TestSmokeHarness in CI.
// The bash path checks run on macOS and exercise the shipped scripts without signing,
// audio, hdiutil, process termination or changes to the user configuration.
test('macOS packaging and app verification derive names from metadata, including Chinese spaces', {
  skip: process.platform === 'win32',
}, () => {
  const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'mixer package names '));
  try {
    for (const file of scripts) {
      const target = path.join(fixture, file);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.copyFileSync(path.join(root, file), target);
    }
    const metadataPath = path.join(fixture, 'apps/mixer-gui/tauri.conf.json');
    for (const name of [config.productName, '测试 名称', 'Audio Mix']) {
      fs.writeFileSync(metadataPath, JSON.stringify({ productName: name, version: '0.9.8' }));
      expectPaths(fixture, name, '0.9.8');
    }
    for (const name of ['', '   ', 'bad/name', 'bad\\name', 'bad:name', 'bad*name']) {
      fs.writeFileSync(metadataPath, JSON.stringify({ productName: name, version: '0.9.8' }));
      for (const [script, flag] of [
        ['apps/mixer-gui/build-app.sh', '--print-artifact-paths'],
        ['m0/tools/app_verify.sh', '--print-app-path'],
      ]) {
        const result = run(script, flag, fixture);
        assert.notEqual(result.status, 0, `invalid product name ${JSON.stringify(name)}`);
        assert.match(result.stderr, /productName/);
      }
      assert.equal(fs.existsSync(path.join(fixture, 'target')), false);
    }
  } finally {
    fs.rmSync(fixture, { recursive: true, force: true });
  }
});

test('macOS permission prompts identify the same application as the bundle metadata', () => {
  const plist = fs.readFileSync(path.join(root, 'apps/mixer-gui/Info.plist'), 'utf8');
  for (const key of ['NSMicrophoneUsageDescription', 'NSDownloadsFolderUsageDescription',
    'NSDesktopFolderUsageDescription', 'NSDocumentsFolderUsageDescription']) {
    const description = plist.match(new RegExp(`<key>${key}</key>\\s*<string>([^<]+)</string>`))?.[1];
    assert.ok(description?.startsWith(config.productName), `${key} must name ${config.productName}`);
  }
  assert.equal(config.identifier, 'dev.local.mixer', 'renaming must retain application identity');
  assert.equal(config.bundle.windows.nsis.startMenuFolder, config.productName);
});

test('packaging scripts use the configured name throughout assembly, installation and verification', () => {
  for (const file of scripts.concat('apps/mixer-gui/build-app-windows.ps1')) {
    const source = fs.readFileSync(path.join(root, file), 'utf8');
    assert.equal(source.includes('音效板'), false, `${file} still refers to the retired application name`);
  }
  const mac = fs.readFileSync(path.join(root, scripts[0]), 'utf8');
  assert.match(mac, /-volname "\$\{APP_NAME\} \$VERSION"/);
  assert.match(mac, /DMG_APP="\$MP\/\$\{APP_NAME\}\.app"/);
  assert.match(mac, /把「\$\{APP_NAME\}」拖进/);
  const windows = fs.readFileSync(path.join(root, 'apps/mixer-gui/build-app-windows.ps1'), 'utf8');
  assert.match(windows, /Find-NsisInstaller .*\$AppName \$tauriConfig\.version/);
  assert.match(windows, /\$\{ProductName\}_\$\{Version\}_\*-setup\.exe/);
  assert.match(windows, /然后打开 \$\{AppName\}/);
});
