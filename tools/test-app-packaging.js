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

test('第三方声明包含变声依赖并随双端产物分发', () => {
  const notices = fs.readFileSync(path.join(root, 'THIRD-PARTY-NOTICES.md'), 'utf8');
  for (const crate of ['signalsmith-stretch', 'cc', 'bindgen', 'clang-sys']) {
    assert.match(notices, new RegExp(`^- ${crate} \\d+\\.\\d+\\.\\d+$`, 'm'),
      `${crate} must be listed in the shipped notices`);
  }
  assert.match(notices, /Copyright 2024 Colin Marc/,
    'Signalsmith Stretch wrapper copyright must be retained');
  assert.match(notices, /Copyright \(c\) 2022 Geraint Luff \/ Signalsmith Audio Ltd\./,
    'vendored Signalsmith Stretch header copyright must be retained');
  assert.match(notices, /Copyright \(c\) 2025 Signalsmith Audio/,
    'vendored Signalsmith Linear header copyright must be retained');
  assert.match(notices, /Permission is hereby granted, free of charge/,
    'Signalsmith MIT text must be retained');

  const mac = fs.readFileSync(path.join(root, 'apps/mixer-gui/build-app.sh'), 'utf8');
  const copyIntoApp = 'cp "$REPO_ROOT/THIRD-PARTY-NOTICES.md" "$APP/Contents/Resources/THIRD-PARTY-NOTICES.md"';
  const copyIntoDmg = 'cp "$REPO_ROOT/THIRD-PARTY-NOTICES.md" "$STAGE/第三方依赖声明.md"';
  assert.ok(mac.includes(copyIntoApp), 'macOS app must bundle the notices');
  assert.ok(mac.includes(copyIntoDmg), 'DMG must expose the notices without opening the app');
  assert.ok(mac.indexOf(copyIntoApp) < mac.indexOf('── 2/4 重新签名 ──'),
    'notices must be copied before signing');
  assert.ok(mac.includes('[ -f "$DMG_APP/Contents/Resources/THIRD-PARTY-NOTICES.md" ]'),
    'mounted DMG verification must check the bundled notices');

  const windows = fs.readFileSync(path.join(root, 'apps/mixer-gui/build-app-windows.ps1'), 'utf8');
  assert.match(windows, /Copy-DistributionDocuments \$Root \$distDir/,
    'Windows build must publish notices with the installer');
  assert.ok(windows.includes("'THIRD-PARTY-NOTICES.md'"),
    'Windows distribution documents must include the notices');
});

// Windows 安装包只含 exe 与 bundle.resources 声明的文件；复制到安装包旁边的文件不会出现在
// 用户机器上。静态编入 exe 的 Signalsmith 等代码要求随二进制保留版权声明。
test('Windows 安装包通过 bundle.resources 把许可证与第三方声明装进安装根目录', () => {
  const resources = config.bundle.resources;
  assert.ok(resources && !Array.isArray(resources),
    'bundle.resources 必须用映射写法，才能固定安装后的文件名');
  const configDir = path.join(root, 'apps/mixer-gui');
  for (const name of ['LICENSE', 'THIRD-PARTY-NOTICES.md']) {
    const sources = Object.entries(resources).filter(([, target]) => target === name).map(([source]) => source);
    assert.equal(sources.length, 1, `${name} 必须恰好被安装一次到安装根目录`);
    const resolved = path.resolve(configDir, sources[0]);
    assert.equal(resolved, path.join(root, name), `${name} 的源路径必须相对配置目录指向仓库根目录的同名文件`);
    assert.ok(fs.existsSync(resolved), `${resolved} 不存在，安装包会缺少该文件`);
  }
  // 目标不能带目录前缀：安装目录里要能直接看到这两个文件。
  for (const target of Object.values(resources)) {
    assert.ok(!target.includes('/') && !target.includes('\\'), `${target} 不应放进子目录`);
  }

  const windows = fs.readFileSync(path.join(root, 'apps/mixer-gui/build-app-windows.ps1'), 'utf8');
  assert.ok(windows.includes('Test-DistributionResources $tauriConfig $PSScriptRoot'),
    '构建前必须核对安装资源声明');
  assert.ok(windows.indexOf('Test-DistributionResources $tauriConfig $PSScriptRoot') <
    windows.indexOf("& \"$Root\\node_modules\\.bin\\tauri.cmd\""), '声明核对必须早于构建');
  assert.ok(windows.includes('Get-MissingNsisResources'), '构建后必须核对 NSIS 脚本收录项');
  assert.match(windows, /Find-NsisInstallerScript \(Join-Path \$Root 'target\\release\\nsis'\)/);
});

test('macOS App 包内的许可证文件名与安装资源声明一致且签名前放入', () => {
  const mac = fs.readFileSync(path.join(root, 'apps/mixer-gui/build-app.sh'), 'utf8');
  for (const name of Object.values(config.bundle.resources)) {
    assert.ok(mac.includes(`"$APP/Contents/Resources/${name}"`),
      `macOS 脚本必须把 ${name} 放在 Contents/Resources 根目录，与 bundle.resources 的落点一致`);
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
